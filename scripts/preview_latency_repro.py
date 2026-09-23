#!/usr/bin/env python3
"""#184 round G3 — local reproduction of the live-preview audio latency (dev tool,
NOT shipped, not run in CI).

Drives the SAME ffmpeg command line `crates/sp-server/src/playback/
preview_encoder.rs::build_ffmpeg_args` builds (libx264 path), over two loopback
TCP listeners exactly like `run_child`:

* a synthetic "decode seam" thread ticks at the video fps and offers ONE NV12
  640x360 frame (bounded queue of 4, drop-on-full — `VIDEO_CHANNEL_CAP`) plus
  ONE interleaved-f32 stereo audio block (bounded queue of 48, drop-on-full —
  `AUDIO_CHANNEL_CAP`) per tick;
* the video feeder writes frames on connect (feed-on-connect, `spawn_video_feeder`)
  and stamps the first write;
* the audio feeder is a 1:1 port of `spawn_audio_feeder`: preroll
  `audio_preroll_samples(gap, lead)`, then `align_block` / `align_timeout`
  against `wall_frames` on every block and every 200 ms receive timeout;
* the audio CONTENT is a loud 440 Hz tone that switches to digital silence at
  wall time T (seconds after the audio feeder started).

A reader thread splits ffmpeg's fragmented-MP4 stdout into top-level boxes and
records the ARRIVAL wall time of every `moof`. After the run the audio track is
decoded, the silence onset (media time) is located, and the report prints:

* ``content_shift_s``  = silence media time - the video media time of wall T
  (by design == lead_ms: the decode-seam audio leads the video by the lookahead,
  the preroll delays it by the same amount);
* ``emit_audio_s``     = wall arrival of the first fragment whose AUDIO reaches
  the silence onset - T;
* ``emit_video_s``     = wall arrival of the first fragment whose VIDEO reaches
  the silence media time - T (MSE plays the intersection of the tracks, so a
  viewer at the live edge hears it at max(emit_audio, emit_video));
* ``written_minus_emitted_s`` — what the aligner WROTE vs the audio media time
  ffmpeg had actually EMITTED, sampled once a second, plus the audio socket's
  kernel queues (our Send-Q, ffmpeg's Recv-Q) from `ss`.

``--feeder g2`` is the 0.65.0-dev.15 feeder (lead written up front as a
silence preroll, aligned at DEQUEUE time); ``--feeder g3`` is the round-G3
feeder (`preview_audio_hold.rs`: the lead HELD in the feeder, each block aligned
by its ARRIVAL). ``--sndbuf`` / ``--audio-url-query recv_buffer_size=N`` shrink
the loopback socket (a Windows-default-like 64 KB), ``--stall-every/--stall-ms``
make the seam hiccup + burst, ``--nice --cpu-hogs N`` starve the child.

Usage::

    python3 scripts/preview_latency_repro.py --ffmpeg /usr/bin/ffmpeg \\
        [--feeder g3] [--sndbuf 65536 --audio-url-query recv_buffer_size=65536] \\
        [--duration 40 --switch-at 25 --lead-ms 1500]

Measured on dev1 (2026-09-24) — see `.claude/rules/preview.md` "#184 round G3".
"""

from __future__ import annotations

import argparse
import json
import math
import os
import queue
import socket
import struct
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field

# --- constants mirrored from the Rust side (preview_stream.rs / preview_encoder.rs)
OUT_W, OUT_H = 640, 360
VIDEO_CHANNEL_CAP = 4
AUDIO_CHANNEL_CAP = 48
FRAMES_PER_MS = 48
ALIGN_PAD_THRESHOLD_MS = 150
MAX_AHEAD_MS = 300
ALIGN_TARGET_AHEAD_MS = 100
RATE = 48_000


# --- 1:1 ports of the pure Rust aligner --------------------------------------
def audio_preroll_samples(connect_gap_ms: int, lead_ms: int) -> int:
    return (min(connect_gap_ms, 5000) + lead_ms) * 48 * 2


def align_timeout(wall_frames: int, written_frames: int) -> int:
    threshold = ALIGN_PAD_THRESHOLD_MS * FRAMES_PER_MS
    if written_frames + threshold < wall_frames:
        return wall_frames - written_frames
    return 0


def align_block(wall_frames: int, written_frames: int, block_frames: int) -> tuple[int, int]:
    pad = align_timeout(wall_frames, written_frames)
    block_end = written_frames + pad + block_frames
    max_end = wall_frames + MAX_AHEAD_MS * FRAMES_PER_MS
    if block_end > max_end:
        target_end = wall_frames + ALIGN_TARGET_AHEAD_MS * FRAMES_PER_MS
        skip = min(block_end - target_end, block_frames)
    else:
        skip = 0
    return pad, skip


def build_ffmpeg_args(video_port: int, audio_port: int, video_in_opts: list[str],
                      audio_in_opts: list[str], out_opts: list[str],
                      audio_url_query: str = "") -> list[str]:
    """Exactly `build_ffmpeg_args(v, a, "libx264")`, with optional extra INPUT
    options inserted right before each `-i` and extra output options before the
    muxer (empty lists == the production vector)."""
    return ["-hide_banner", "-loglevel", "error",
            "-use_wallclock_as_timestamps", "1", "-f", "rawvideo", "-pix_fmt", "nv12",
            "-s", f"{OUT_W}x{OUT_H}", *video_in_opts, "-i", f"tcp://127.0.0.1:{video_port}",
            "-f", "f32le", "-ar", "48000", "-ac", "2", *audio_in_opts,
            "-i", f"tcp://127.0.0.1:{audio_port}" + (f"?{audio_url_query}" if audio_url_query else ""),
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-b:v", "500k", "-maxrate", "500k", "-bufsize", "500k",
            "-g", "25", "-fps_mode", "cfr", "-r", "25",
            "-c:a", "aac", "-b:a", "64k",
            *out_opts,
            "-movflags", "+frag_keyframe+empty_moov+default_base_moof",
            "-frag_duration", "500000", "-flush_packets", "1", "-f", "mp4", "pipe:1"]


@dataclass
class Shared:
    stop: threading.Event = field(default_factory=threading.Event)
    first_video_wall: float = 0.0
    audio_feeder_start: float = 0.0
    switch_wall: float = 0.0
    silence_written_frame: int = -1  # written_frames when the first silent CONTENT frame was written
    written_frames: int = 0
    padded_frames: int = 0
    skipped_frames: int = 0
    video_drops: int = 0
    audio_drops: int = 0
    samples: list = field(default_factory=list)  # (t, written_media_s, emitted_audio_media_s, sendq, recvq)
    fragments: list = field(default_factory=list)  # (arrival_wall, moof, mdat)
    init: bytes = b""


def seam(sh: Shared, vq: queue.Queue, aq: queue.Queue, fps: float, switch_at: float,
         stall_every: float, stall_ms: int) -> None:
    """The decode seam: one video frame + one audio block per tick, drop-on-full.

    With ``stall_every`` > 0 the seam STALLS for ``stall_ms`` every
    ``stall_every`` seconds and then catches up by emitting the missed ticks
    back-to-back — the loaded-box decode hiccup + catch-up burst (#184 G2)."""
    frame = bytes([16]) * (OUT_W * OUT_H) + bytes([128]) * (OUT_W * OUT_H // 2)
    block_frames_f = RATE / fps
    phase = 0.0
    carry = 0.0
    tick = 1.0 / fps
    nxt = time.monotonic()
    next_stall = nxt + stall_every if stall_every > 0 else float("inf")
    n = 0
    while not sh.stop.is_set():
        nxt += tick
        now = time.monotonic()
        if now >= next_stall:
            time.sleep(stall_ms / 1000)
            next_stall = time.monotonic() + stall_every
        elif nxt > now:
            time.sleep(nxt - now)
        n += 1
        f = bytearray(frame)  # vary the luma so x264 has something to encode
        f[(n * 97) % (OUT_W * OUT_H)] = 235
        try:
            vq.put_nowait(bytes(f))
        except queue.Full:
            sh.video_drops += 1  # drop-on-full, like StreamShared::offer_video
        carry += block_frames_f
        nfr = int(carry)
        carry -= nfr
        silent = bool(sh.audio_feeder_start) and (time.monotonic() - sh.audio_feeder_start) >= switch_at
        if silent and not sh.switch_wall:
            sh.switch_wall = time.monotonic()
        buf = bytearray(nfr * 8)
        if not silent:
            for i in range(nfr):
                v = 0.5 * math.sin(phase)
                phase += 2 * math.pi * 440 / RATE
                struct.pack_into("<ff", buf, i * 8, v, v)
        try:
            aq.put_nowait((time.monotonic(), silent, bytes(buf)))  # arrival stamp (G3)
        except queue.Full:
            sh.audio_drops += 1  # drop-on-full, like StreamShared::offer_audio


def video_feeder(sh: Shared, sock: socket.socket, vq: queue.Queue) -> None:
    while not sh.stop.is_set():
        try:
            fr = vq.get(timeout=0.2)
        except queue.Empty:
            continue
        try:
            sock.sendall(fr)
        except OSError as e:
            print(f"video feeder: write failed ({e}) — child gone", file=sys.stderr)
            return
        if not sh.first_video_wall:
            sh.first_video_wall = time.monotonic()


def audio_feeder(sh: Shared, sock: socket.socket, aq: queue.Queue, lead_ms: int) -> None:
    while not aq.empty():  # drop stale blocks queued before the connect
        aq.get_nowait()
    gap_ms = 0 if not sh.first_video_wall else int((time.monotonic() - sh.first_video_wall) * 1000)
    preroll = audio_preroll_samples(gap_ms, lead_ms)
    start = time.monotonic()
    sh.audio_feeder_start = start
    base = preroll // 2

    def wall_frames() -> int:
        return base + int((time.monotonic() - start) * 1_000_000) * FRAMES_PER_MS // 1000

    try:
        sock.sendall(bytes(preroll * 4))
        written = base
        sh.written_frames = written
        while not sh.stop.is_set():
            try:
                _arrival, silent, block = aq.get(timeout=0.2)
                pad, skip = align_block(wall_frames(), written, len(block) // 8)
            except queue.Empty:
                silent, block, skip = False, None, 0
                pad = align_timeout(wall_frames(), written)
            if pad:
                sock.sendall(bytes(pad * 8))
                written += pad
                sh.padded_frames += pad
            sh.skipped_frames += skip
            if block is not None:
                tail = block[skip * 8:]
                if silent and sh.silence_written_frame < 0 and tail:
                    sh.silence_written_frame = written
                if tail:
                    sock.sendall(tail)
                written += len(tail) // 8
            sh.written_frames = written
    except OSError as e:
        print(f"audio feeder: write failed ({e}) — child gone", file=sys.stderr)


# --- #184 round G3: 1:1 port of `preview_audio_timeline.rs` + the G3 feeder --
AUDIO_WRITE_AHEAD_MS = 200


class AudioTimeline:
    """Positions (stereo frames) on the encoder's sample-count audio timeline,
    from times in µs since the feeder started. The decode-seam lead is HELD in
    the feeder (a block is written `lead - write_ahead` after it ARRIVED) instead
    of being parked in the socket / ffmpeg as a lead-long silence preroll."""

    def __init__(self, base_frames: int, lead_ms: int):
        self.base = base_frames
        self.lead_us = lead_ms * 1000
        self.ahead_us = min(AUDIO_WRITE_AHEAD_MS, lead_ms) * 1000

    def position_at(self, now_us: int) -> int:
        return self.base + (now_us + self.ahead_us) * FRAMES_PER_MS // 1000

    def block_target(self, arrival_us: int) -> int:
        return self.base + (arrival_us + self.lead_us) * FRAMES_PER_MS // 1000

    def due_us(self, arrival_us: int) -> int:
        return arrival_us + self.lead_us - self.ahead_us


def g3_audio_feeder(sh: Shared, sock: socket.socket, aq: queue.Queue, lead_ms: int) -> None:
    from collections import deque
    while not aq.empty():  # drop stale blocks queued before the connect
        aq.get_nowait()
    gap_ms = 0 if not sh.first_video_wall else int((time.monotonic() - sh.first_video_wall) * 1000)
    preroll = audio_preroll_samples(gap_ms, 0)  # the connect gap only — the lead is HELD
    start = time.monotonic()
    sh.audio_feeder_start = start
    tl = AudioTimeline(preroll // 2, lead_ms)

    def us(t: float) -> int:
        return max(0, int((t - start) * 1_000_000))

    held: deque = deque()
    try:
        sock.sendall(bytes(preroll * 4))
        written = preroll // 2
        while not sh.stop.is_set():
            now_us = us(time.monotonic())
            wait_s = 0.2
            if held:
                wait_s = min(0.2, max(0.0, (tl.due_us(us(held[0][0])) - now_us) / 1e6))
            try:
                held.append(aq.get(timeout=wait_s) if wait_s > 0 else aq.get_nowait())
            except queue.Empty:
                pass
            while True:
                try:
                    held.append(aq.get_nowait())
                except queue.Empty:
                    break
            now_us = us(time.monotonic())
            while held and tl.due_us(us(held[0][0])) <= now_us:
                arrival, silent, block = held.popleft()
                pad, skip = align_block(tl.block_target(us(arrival)), written, len(block) // 8)
                if pad:
                    sock.sendall(bytes(pad * 8))
                    written += pad
                    sh.padded_frames += pad
                sh.skipped_frames += skip
                tail = block[skip * 8:]
                if silent and sh.silence_written_frame < 0 and tail:
                    sh.silence_written_frame = written
                if tail:
                    sock.sendall(tail)
                written += len(tail) // 8
            pad = align_timeout(tl.position_at(us(time.monotonic())), written)
            if pad:
                sock.sendall(bytes(pad * 8))
                written += pad
                sh.padded_frames += pad
            sh.written_frames = written
    except OSError as e:
        print(f"audio feeder: write failed ({e}) — child gone", file=sys.stderr)


def read_boxes(sh: Shared, out) -> None:
    buf = b""
    cur_frag = b""
    while True:
        chunk = out.read1(65536)
        if not chunk:
            return
        buf += chunk
        while len(buf) >= 8:
            size, typ = struct.unpack(">I4s", buf[:8])
            if size == 1:
                if len(buf) < 16:
                    break
                size = struct.unpack(">Q", buf[8:16])[0]
            if len(buf) < size:
                break
            box, buf = buf[:size], buf[size:]
            t = typ.decode("latin1")
            if t in ("ftyp", "moov"):
                sh.init += box
            elif t == "moof":
                cur_frag = box
            elif t == "mdat":
                sh.fragments.append((time.monotonic(), cur_frag, box))
                cur_frag = b""


def children(data: bytes):
    off = 0
    while off + 8 <= len(data):
        size, typ = struct.unpack(">I4s", data[off:off + 8])
        hdr = 8
        if size == 1:
            size = struct.unpack(">Q", data[off + 8:off + 16])[0]
            hdr = 16
        if size < hdr:
            return
        yield typ.decode("latin1"), data[off + hdr:off + size]
        off += size


def find(data: bytes, path: list[str]):
    if not path:
        yield data
        return
    for t, body in children(data):
        if t == path[0]:
            yield from find(body, path[1:])


def parse_init(init: bytes) -> dict:
    """track_id -> {handler, timescale, default_duration}."""
    tracks: dict = {}
    trex_def = {}
    for moov in find(init, ["moov"]):
        for trak in find(moov, ["trak"]):
            tkhd = next(find(trak, ["tkhd"]))
            tid = struct.unpack(">I", tkhd[4 + (16 if tkhd[0] == 1 else 8):][:4])[0]
            mdhd = next(find(trak, ["mdia", "mdhd"]))
            ts = struct.unpack(">I", mdhd[4 + (16 if mdhd[0] == 1 else 8):][:4])[0]
            hdlr = next(find(trak, ["mdia", "hdlr"]))
            tracks[tid] = {"handler": hdlr[8:12].decode("latin1"), "timescale": ts}
        for trex in find(moov, ["mvex", "trex"]):
            tid, _desc, dur = struct.unpack(">III", trex[4:16])
            trex_def[tid] = dur
    for tid, d in trex_def.items():
        if tid in tracks:
            tracks[tid]["default_duration"] = d
    return tracks


def parse_moof(moof_box: bytes, tracks: dict) -> dict:
    """track_id -> (start_s, end_s) media interval of one fragment."""
    res = {}
    for traf in find(moof_box[8:], ["traf"]):
        tfhd = next(find(traf, ["tfhd"]))
        flags = int.from_bytes(tfhd[1:4], "big")
        tid = struct.unpack(">I", tfhd[4:8])[0]
        off = 8
        if flags & 0x01:
            off += 8
        if flags & 0x02:
            off += 4
        def_dur = tracks[tid].get("default_duration", 0)
        if flags & 0x08:
            def_dur = struct.unpack(">I", tfhd[off:off + 4])[0]
        tfdt = next(find(traf, ["tfdt"]))
        base = struct.unpack(">Q", tfdt[4:12])[0] if tfdt[0] == 1 else struct.unpack(">I", tfdt[4:8])[0]
        total = 0
        for trun in find(traf, ["trun"]):
            tflags = int.from_bytes(trun[1:4], "big")
            count = struct.unpack(">I", trun[4:8])[0]
            p = 8 + (4 if tflags & 0x01 else 0) + (4 if tflags & 0x04 else 0)
            per = sum(4 for bit in (0x100, 0x200, 0x400, 0x800) if tflags & bit)
            for i in range(count):
                if tflags & 0x100:
                    total += struct.unpack(">I", trun[p + i * per:p + i * per + 4])[0]
                else:
                    total += def_dur
        ts = tracks[tid]["timescale"]
        res[tid] = (base / ts, (base + total) / ts)
    return res


def socket_queues(port: int) -> tuple[int, int]:
    """(our Send-Q towards ffmpeg, ffmpeg's Recv-Q) on the audio connection, via `ss`."""
    out = subprocess.run(["ss", "-tnH", "state", "established"], capture_output=True, text=True,
                         check=True).stdout
    sendq = recvq = -1
    for line in out.splitlines():
        parts = line.split()
        if len(parts) < 4:
            continue
        rq, sq, local, peer = int(parts[0]), int(parts[1]), parts[2], parts[3]
        if local.endswith(f":{port}"):
            sendq = sq          # our (listener-side) socket
        elif peer.endswith(f":{port}"):
            recvq = rq          # ffmpeg's socket
    return sendq, recvq


def run(args) -> dict:
    sh = Shared()
    vl = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    al = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    for lst in (vl, al):
        lst.bind(("127.0.0.1", 0))
        lst.listen(1)
    if args.sndbuf:
        al.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, args.sndbuf)
    vport, aport = vl.getsockname()[1], al.getsockname()[1]
    cmd = [args.ffmpeg, *build_ffmpeg_args(vport, aport, args.video_in_opt, args.audio_in_opt,
                                           args.out_opt, args.audio_url_query)]
    if args.nice:
        cmd = ["nice", "-n", "19", *cmd]  # ~ BELOW_NORMAL_PRIORITY_CLASS on the box
    # CPU hogs at nice 19 — the box's low-priority stem/dub workers competing
    # with the (BELOW_NORMAL) encoder child, never with the harness itself.
    hogs = [subprocess.Popen([sys.executable, "-c", "import os\nos.nice(19)\nwhile True: pass"])
            for _ in range(args.cpu_hogs)]
    child = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE)
    vq: queue.Queue = queue.Queue(VIDEO_CHANNEL_CAP)
    aq: queue.Queue = queue.Queue(AUDIO_CHANNEL_CAP)
    for target, targs in ((seam, (sh, vq, aq, args.fps, args.switch_at, args.stall_every,
                                  args.stall_ms)),
                          (read_boxes, (sh, child.stdout))):
        threading.Thread(target=target, args=targs, daemon=True).start()
    vl.settimeout(5)
    vs, _ = vl.accept()
    threading.Thread(target=video_feeder, args=(sh, vs, vq), daemon=True).start()
    al.settimeout(5)
    as_, _ = al.accept()
    if args.sndbuf:
        as_.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, args.sndbuf)
    eff_sndbuf = as_.getsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF)
    feeder = g3_audio_feeder if args.feeder == "g3" else audio_feeder
    threading.Thread(target=feeder, args=(sh, as_, aq, args.lead_ms), daemon=True).start()
    tracks = None
    end = time.monotonic() + args.duration
    while time.monotonic() < end:
        time.sleep(1.0)
        if tracks is None and sh.init:
            tracks = parse_init(sh.init)
        emitted = emitted_v = 0.0
        if tracks:
            atid = next(t for t, d in tracks.items() if d["handler"] == "soun")
            vtid = next(t for t, d in tracks.items() if d["handler"] == "vide")
            for _, moof, _ in sh.fragments[-3:]:
                r = parse_moof(moof, tracks)
                if atid in r:
                    emitted = max(emitted, r[atid][1])
                if vtid in r:
                    emitted_v = max(emitted_v, r[vtid][1])
        sq, rq = socket_queues(aport)
        now = time.monotonic()
        sh.samples.append((now - sh.audio_feeder_start, sh.written_frames / RATE, emitted, sq, rq,
                           (now - sh.first_video_wall) - emitted_v, aq.qsize()))
    sh.stop.set()
    child.kill()
    for h in hogs:
        h.kill()
        h.wait()
    _, err = child.communicate()
    return analyse(sh, args, eff_sndbuf, err.decode(errors="replace"))


def rms_db(samples, i: int, win: int) -> float:
    rms = math.sqrt(sum(x * x for x in samples[i:i + win]) / win)
    return 20 * math.log10(rms) if rms > 0 else -200.0


def analyse(sh: Shared, args, eff_sndbuf: int, stderr: str) -> dict:
    tracks = parse_init(sh.init)
    atid = next(t for t, d in tracks.items() if d["handler"] == "soun")
    vtid = next(t for t, d in tracks.items() if d["handler"] == "vide")
    frags = [(w, parse_moof(m, tracks)) for w, m, _ in sh.fragments]
    path = os.path.join(args.workdir, f"out-{args.label}-{os.getpid()}.mp4")
    with open(path, "wb") as fh:
        fh.write(sh.init)
        for _, m, d in sh.fragments:
            fh.write(m)
            fh.write(d)
    pcm = subprocess.run([args.ffmpeg, "-v", "error", "-i", path, "-map", "0:a", "-f", "f32le",
                          "-ac", "1", "-ar", "48000", "-"], capture_output=True, check=True).stdout
    n = len(pcm) // 4
    samples = struct.unpack(f"<{n}f", pcm[:n * 4])
    first_audio = min(r[atid][0] for _, r in frags if atid in r)
    win = 960  # 20 ms
    dbs = [rms_db(samples, i, win) for i in range(0, n - win, win)]
    onset = None
    loud_seen = False
    for k, db in enumerate(dbs):
        loud_seen = loud_seen or db > -20
        # the switch is the FIRST silence that lasts >= 3 s (seam-stall pads are
        # shorter silences and must not match)
        if loud_seen and k + 150 <= len(dbs) and all(d < -60 for d in dbs[k:k + 150]):
            onset = first_audio + k * win / RATE
            break
    t_sw = sh.switch_wall
    video_media_at_switch = t_sw - sh.first_video_wall
    res = {
        "ffmpeg": subprocess.run([args.ffmpeg, "-version"], capture_output=True, text=True,
                                 check=True).stdout.splitlines()[0][:40],
        "variant": args.label,
        "feeder": args.feeder,
        "sndbuf_effective": eff_sndbuf,
        "lead_ms": args.lead_ms,
        "fragments": len(frags),
        "drops_video_audio": [sh.video_drops, sh.audio_drops],
        "padded_ms_skipped_ms": [sh.padded_frames // FRAMES_PER_MS, sh.skipped_frames // FRAMES_PER_MS],
        "silence_onset_media_s": None if onset is None else round(onset, 3),
        "silence_written_media_s": (round(sh.silence_written_frame / RATE, 3)
                                    if sh.silence_written_frame >= 0 else None),
        "video_media_at_switch_s": round(video_media_at_switch, 3),
    }
    if onset is not None:
        res["content_shift_s"] = round(onset - video_media_at_switch, 3)
        ea = next((w for w, r in frags if atid in r and r[atid][1] > onset), None)
        ev = next((w for w, r in frags if vtid in r and r[vtid][1] > onset), None)
        res["emit_audio_s"] = round(ea - t_sw, 3) if ea else None
        res["emit_video_s"] = round(ev - t_sw, 3) if ev else None
    res["written_minus_emitted_s"] = [round(s[1] - s[2], 2) for s in sh.samples if s[2]]
    # wall time since the first video write minus the emitted VIDEO media end:
    # how far the encoder's OUTPUT runs behind real time (the pause latency part)
    res["video_output_behind_wall_s"] = [round(s[5], 2) for s in sh.samples if s[2]]
    res["audio_channel_depth_blocks"] = [s[6] for s in sh.samples]
    res["audio_sendq_bytes"] = [s[3] for s in sh.samples]
    res["ffmpeg_recvq_bytes"] = [s[4] for s in sh.samples]
    res["stderr"] = stderr.strip()[-400:]
    return res


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--duration", type=float, default=30.0)
    p.add_argument("--switch-at", type=float, default=15.0,
                   help="seconds after the audio feeder starts when the tone becomes silence")
    p.add_argument("--fps", type=float, default=30.0, help="decode-seam cadence (source fps)")
    p.add_argument("--lead-ms", type=int, default=1500,
                   help="lead_ms_for(false) == AUDIO_LOOKAHEAD_MS")
    p.add_argument("--sndbuf", type=int, default=0,
                   help="SO_SNDBUF on the audio socket (0 = OS default)")
    p.add_argument("--feeder", choices=["g2", "g3"], default="g2",
                   help="g2 = the shipped 0.65.0-dev.15 feeder, g3 = the lead held in the feeder")
    p.add_argument("--stall-every", type=float, default=0.0,
                   help="seam stall period in s (0 = a steady seam)")
    p.add_argument("--stall-ms", type=int, default=0, help="seam stall length, then a burst")
    p.add_argument("--nice", action="store_true", help="run ffmpeg at nice 19")
    p.add_argument("--cpu-hogs", type=int, default=0, help="busy-loop processes for the run")
    p.add_argument("--audio-url-query", default="",
                   help="query on ffmpeg's audio tcp:// url, e.g. recv_buffer_size=16384 "
                        "(emulates a small Windows loopback receive window)")
    p.add_argument("--video-in-opt", action="append", default=[])
    p.add_argument("--audio-in-opt", action="append", default=[])
    p.add_argument("--out-opt", action="append", default=[])
    p.add_argument("--label", default="default")
    p.add_argument("--workdir", default=os.environ.get("TMPDIR", "/tmp"))
    args = p.parse_args()
    print(json.dumps(run(args), indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
