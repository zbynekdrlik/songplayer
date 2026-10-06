#!/usr/bin/env python3
"""#184 round G3 — local reproduction of the live-preview audio latency (dev tool,
NOT shipped, not run in CI).

Drives the SAME ffmpeg command line `crates/sp-server/src/playback/
preview_encoder.rs::build_ffmpeg_args` builds (libx264 path), over two loopback
TCP listeners exactly like `run_child`:

* a synthetic "decode seam" thread ticks at the video fps and offers ONE NV12
  640x360 frame (bounded queue of 4, drop-on-full — `VIDEO_CHANNEL_CAP`) plus
  ONE interleaved-f32 stereo audio block (bounded queue of 48, drop-on-full —
  `AUDIO_CHANNEL_CAP`) per tick; each frame's top-left 160x90 block carries
  the tick in its luma, so a picture change is visible in the output
  (``--source testsrc2`` puts a detailed lavfi picture under it);
* the video feeder is a 1:1 port of `preview_video_clock.rs` (#221 A1): a
  40 ms slot of the MONOTONIC clock is written only with a NEW frame (the
  newest that arrived by the slot's decision, half a slot late); with none (a
  pause) NOTHING is written, so the encoder starves; the next frame fills the
  slots decided before it arrived with the last one (at most 250), then takes
  its own slot. The encoder reads it at
  `-framerate 25` with NO wall-clock stamp; frame 0 is the origin the audio
  aligns to;
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

``--pause-at P`` stops the seam offering anything at P s (the pipeline's
pause); the report then adds the media time of the last NEW picture (the
marker's luma), how far it trails the audio's silence onset
(``pause_picture_minus_sound_s``; None while the starved video holds the
muxer's audio), when the fragment carrying it left the encoder after the
pause (``pause_last_new_picture_emit_s``, the #221 E2E bound is 3 s in the
browser), and the E2E's own criterion: how often the WHOLE decoded 64x36
picture changes after it (``pause_exact_picture_changes_after_last_new``;
a repeated picture is re-encoded and breathes, a starved encoder shows 0).
``--step-ms S --step-at A`` steps the ffmpeg child's WALL clock by S ms at A s
(an LD_PRELOAD shim over ``clock_gettime(CLOCK_REALTIME)`` / ``gettimeofday``,
compiled with ``cc`` into ``--workdir``), like the box's nightly UTC step
(+1543 ms on 6.10.2026): with both inputs counted on the monotonic clock the
step must change nothing (#221: with the old wall-clock-stamped video the
picture trailed the sound by 3.5-4.7 s after it).

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
    python3 scripts/preview_latency_repro.py --ffmpeg /usr/bin/ffmpeg --feeder g3 \\
        --duration 24 --switch-at 99 --pause-at 14 --source testsrc2 \\
        [--step-ms 1543 --step-at 6]

Measured on dev1 (2026-09-24) — see `.claude/rules/preview.md` "#184 round G3";
the clock-step / pause rows (6.10.2026): "#221 — one monotonic clock".
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
PREVIEW_FPS = 25  # preview_video_clock.rs
FRAME_US = 40_000
DECIDE_LATE_US = 20_000
MAX_GAP_FILL_SLOTS = 250
MARK_W, MARK_H = 160, 90  # the top-left block carrying the seam tick in its luma


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
            "-f", "rawvideo", "-pix_fmt", "nv12",
            "-s", f"{OUT_W}x{OUT_H}", "-framerate", str(PREVIEW_FPS), *video_in_opts,
            "-i", f"tcp://127.0.0.1:{video_port}",
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
    pause_wall: float = 0.0
    step_wall: float = 0.0
    restart_requested: bool = False
    video_stats: list = field(default_factory=lambda: [0, 0, 0, 0])  # written, repeated, skipped, max_burst
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
         stall_every: float, stall_ms: int, pause_at: float, pictures: list) -> None:
    """The decode seam: one video frame + one audio block per tick, drop-on-full.

    With ``stall_every`` > 0 the seam STALLS for ``stall_ms`` every
    ``stall_every`` seconds and then catches up by emitting the missed ticks
    back-to-back — the loaded-box decode hiccup + catch-up burst (#184 G2).
    From ``pause_at`` s (> 0) on it offers nothing: the pipeline's pause.
    ``pictures`` (``--source``) are detailed NV12 frames played in a loop under
    the marker; empty = a flat black frame."""
    flat = bytes([16]) * (OUT_W * OUT_H) + bytes([128]) * (OUT_W * OUT_H // 2)
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
        if pause_at > 0 and sh.audio_feeder_start and time.monotonic() - sh.audio_feeder_start >= pause_at:
            if not sh.pause_wall:
                sh.pause_wall = time.monotonic()
            continue
        n += 1
        f = bytearray(pictures[n % len(pictures)] if pictures else flat)
        # the tick in the marker block's luma (x264 encodes a change)
        luma = 16 + (n * 7) % 220
        for row in range(MARK_H):
            f[row * OUT_W:row * OUT_W + MARK_W] = bytes([luma]) * MARK_W
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


class VideoClock:
    """1:1 port of `preview_video_clock.rs::VideoClock` (#221 A1): a 40 ms slot
    is written only with a NEW canvas; a canvas belongs to the first slot
    decided (half a slot after its time) at or after its ARRIVAL; with none (a
    pause) nothing is written, so the encoder starves and the picture holds
    pixel-exact; the next canvas first fills the slots decided before it
    arrived with the last written one (at most MAX_GAP_FILL_SLOTS, a longer gap
    ends the encoder run), then takes its own slot once it is decided."""

    def __init__(self):
        self.start_us = None
        self.pending = None  # (frame, arrival_us)
        self.last = None
        self.written = self.repeated = self.skipped = self.max_burst = 0

    def offer(self, frame, arrival_us: int):
        if self.pending is not None:
            self.skipped += 1
        self.pending = (frame, arrival_us)

    @staticmethod
    def slots_due(elapsed_us: int) -> int:
        return max(0, elapsed_us - DECIDE_LATE_US) // FRAME_US + 1

    def pending_slot(self):
        if self.start_us is None or self.pending is None:
            return None
        elapsed = max(0, self.pending[1] - self.start_us)
        slot = 0 if elapsed == 0 else self.slots_due(elapsed - 1)
        return max(slot, self.written)

    def must_restart(self) -> bool:
        slot = self.pending_slot()
        return slot is not None and slot > self.written + MAX_GAP_FILL_SLOTS

    def take_due(self, now_us: int):
        if self.pending is None:
            return None
        if self.start_us is None:
            self.start_us = self.pending[1]
        slot = self.pending_slot()
        fill = slot - self.written
        if fill > MAX_GAP_FILL_SLOTS:
            return None
        if fill > 0 and self.last is not None:
            self.written += fill
            self.repeated += fill
            self.max_burst = max(self.max_burst, fill)
            return self.last, fill
        if self.slots_due(max(0, now_us - self.start_us)) > slot:
            self.last, self.pending = self.pending[0], None
            self.written = slot + 1
            self.max_burst = max(self.max_burst, 1)
            return self.last, 1
        return None

    def wait_us(self, now_us: int) -> int:
        if self.pending is None:
            return 200_000
        slot = self.pending_slot()
        if slot is None or slot > self.written:
            return 0
        return max(0, self.start_us + slot * FRAME_US + DECIDE_LATE_US - now_us)


def next_frame(vq: queue.Queue, timeout_s: float):
    """The next offered frame within ``timeout_s`` (0 = only one already queued),
    or None when none came — the feeder's normal idle case, not an error."""
    try:
        return vq.get(timeout=timeout_s) if timeout_s > 0 else vq.get_nowait()
    except queue.Empty:
        return None


def video_feeder(sh: Shared, sock: socket.socket, vq: queue.Queue) -> None:
    """Port of `preview_video_clock.rs::spawn_video_feeder` on the monotonic clock."""
    base = time.monotonic()

    def us() -> int:
        return int((time.monotonic() - base) * 1_000_000)

    clock = VideoClock()
    while not sh.stop.is_set():
        fr = next_frame(vq, clock.wait_us(us()) / 1e6)
        while fr is not None:  # the newest canvas takes the slot
            clock.offer(fr, us())
            fr = next_frame(vq, 0)
        now = us()
        if clock.must_restart():
            print("video feeder: a gap over 10 s — production ends the encoder run here",
                  file=sys.stderr)
            sh.restart_requested = True
            return
        for _ in range(2):  # at most the gap's fill, then the new canvas
            due = clock.take_due(now)
            if due is None:
                break
            canvas, n = due
            try:
                for _ in range(n):
                    sock.sendall(canvas)
            except OSError as e:
                print(f"video feeder: write failed ({e}) — child gone", file=sys.stderr)
                return
        if clock.start_us is not None and not sh.first_video_wall:
            sh.first_video_wall = base + clock.start_us / 1e6  # frame 0's arrival: the origin
        sh.video_stats = [clock.written, clock.repeated, clock.skipped, clock.max_burst]


STEP_SHIM_C = r"""
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/time.h>
#include <time.h>
static long long off_ns(void) {
    const char *f = getenv("STEP_FILE");
    FILE *fp = f ? fopen(f, "r") : 0;
    long long v = 0;
    if (!fp) return 0;
    if (fscanf(fp, "%lld", &v) != 1) v = 0;
    fclose(fp);
    return v;
}
int clock_gettime(clockid_t c, struct timespec *ts) {
    static int (*real)(clockid_t, struct timespec *) = 0;
    if (!real) real = (int (*)(clockid_t, struct timespec *))dlsym(RTLD_NEXT, "clock_gettime");
    int r = real(c, ts);
    if (r == 0 && c == CLOCK_REALTIME) {
        long long ns = (long long)ts->tv_sec * 1000000000LL + ts->tv_nsec + off_ns();
        ts->tv_sec = ns / 1000000000LL;
        ts->tv_nsec = ns % 1000000000LL;
    }
    return r;
}
int gettimeofday(struct timeval *tv, void *tz) {
    struct timespec ts;
    (void)tz;
    clock_gettime(CLOCK_REALTIME, &ts);
    tv->tv_sec = ts.tv_sec;
    tv->tv_usec = ts.tv_nsec / 1000;
    return 0;
}
"""


def source_pictures(args) -> list:
    """``--source``: 4 s of a lavfi source rendered to 640x360 NV12 frames."""
    if not args.source:
        return []
    raw = subprocess.run([args.ffmpeg, "-hide_banner", "-loglevel", "error", "-f", "lavfi",
                          "-i", f"{args.source}=size={OUT_W}x{OUT_H}:rate={args.fps}", "-t", "4",
                          "-pix_fmt", "nv12", "-f", "rawvideo", "-"],
                         capture_output=True, check=True).stdout
    size = OUT_W * OUT_H * 3 // 2
    return [raw[i:i + size] for i in range(0, len(raw) - size + 1, size)]


def step_env(workdir: str) -> tuple[dict, str]:
    """Build the wall-clock step shim; the child's env and the offset file."""
    src = os.path.join(workdir, "preview_step_clock.c")
    lib = os.path.join(workdir, "preview_step_clock.so")
    with open(src, "w") as fh:
        fh.write(STEP_SHIM_C)
    subprocess.run(["cc", "-shared", "-fPIC", "-O2", "-o", lib, src, "-ldl"], check=True)
    step_file = os.path.join(workdir, f"preview_step_clock-{os.getpid()}.ns")
    with open(step_file, "w") as fh:
        fh.write("0")
    return dict(os.environ, LD_PRELOAD=lib, STEP_FILE=step_file), step_file


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


# --- #184 round G3: 1:1 port of `preview_audio_hold.rs` + the G3 feeder ------
AUDIO_WRITE_AHEAD_MS = 200
SNAP_TOLERANCE_MS = 10
AFEED_POLL_S = 0.03


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
    contiguous = False  # the last write was tapped audio (not the preroll / a pad)
    try:
        sock.sendall(bytes(preroll * 4))
        written = preroll // 2
        while not sh.stop.is_set():
            now_us = us(time.monotonic())
            wait_s = AFEED_POLL_S
            if held:
                wait_s = min(AFEED_POLL_S, max(0.0, (tl.due_us(us(held[0][0])) - now_us) / 1e6))
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
                target = tl.block_target(us(arrival))
                if not contiguous and target - written > SNAP_TOLERANCE_MS * FRAMES_PER_MS:
                    sock.sendall(bytes((target - written) * 8))  # snap after silence
                    sh.padded_frames += target - written
                    written = target
                pad, skip = align_block(target, written, len(block) // 8)
                if pad:
                    sock.sendall(bytes(pad * 8))
                    written += pad
                    sh.padded_frames += pad
                    contiguous = False
                sh.skipped_frames += skip
                tail = block[skip * 8:]
                if silent and sh.silence_written_frame < 0 and tail:
                    sh.silence_written_frame = written
                if tail:
                    sock.sendall(tail)
                    contiguous = True
                written += len(tail) // 8
            pad = align_timeout(tl.position_at(us(time.monotonic())), written)
            if pad:
                sock.sendall(bytes(pad * 8))
                written += pad
                sh.padded_frames += pad
                contiguous = False
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
    env, step_file = step_env(args.workdir) if args.step_ms else (None, "")
    child = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, env=env)
    vq: queue.Queue = queue.Queue(VIDEO_CHANNEL_CAP)
    aq: queue.Queue = queue.Queue(AUDIO_CHANNEL_CAP)
    for target, targs in ((seam, (sh, vq, aq, args.fps, args.switch_at, args.stall_every,
                                  args.stall_ms, args.pause_at, source_pictures(args))),
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
        if step_file and not sh.step_wall and time.monotonic() - sh.audio_feeder_start >= args.step_at:
            with open(step_file, "w") as fh:  # the child's wall clock jumps now
                fh.write(str(args.step_ms * 1_000_000))
            sh.step_wall = time.monotonic()
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


def pause_metrics(sh: Shared, args, path: str, frags: list, vtid: int, onset) -> dict:
    """#221: the media time of the last NEW picture (the marker block's luma
    still changing), how far it trails the pause's silence onset in the audio,
    and when the fragment carrying it left the encoder after the pause."""
    out = subprocess.run([args.ffmpeg, "-hide_banner", "-i", path, "-map", "0:v", "-vf",
                          f"crop={MARK_W}:{MARK_H}:0:0,signalstats,"
                          "metadata=print:key=lavfi.signalstats.YAVG", "-f", "null", "-"],
                         capture_output=True, text=True, check=True).stderr
    frames = []
    pts = None
    for line in out.splitlines():
        if "pts_time:" in line:
            pts = float(line.split("pts_time:")[1].split()[0])
        elif "YAVG=" in line and pts is not None:
            frames.append((pts, float(line.split("YAVG=")[1])))
    last_new = next((frames[i][0] for i in range(len(frames) - 1, 0, -1)
                     if abs(frames[i][1] - frames[i - 1][1]) > 1.0), None)
    if last_new is None:
        return {"pause_last_new_picture_media_s": None}
    emit = next((w for w, r in frags if vtid in r and r[vtid][1] >= last_new), None)
    # The E2E's criterion: the WHOLE 64x36 picture bit-exact after the freeze
    # (post-deploy-preview.spec.ts frameHash). A re-encoded repeat breathes;
    # a starved encoder (#221 A1) emits nothing after the last new picture.
    md5 = subprocess.run([args.ffmpeg, "-hide_banner", "-loglevel", "error", "-i", path,
                          "-map", "0:v", "-vf", "scale=64:36:flags=bilinear", "-pix_fmt",
                          "rgb24", "-f", "framemd5", "-"],
                         capture_output=True, text=True, check=True).stdout
    rows = [r.split(",") for r in md5.splitlines() if r and not r.startswith("#")]
    tb_line = next(r for r in md5.splitlines() if r.startswith("#tb 0:"))
    num, den = tb_line.split(":")[1].strip().split("/")
    tb = int(num) / int(den)  # framemd5 pts are in this stream time base
    after = [r[-1].strip() for r in rows if int(r[2]) * tb > last_new + 1e-6]
    return {
        "pause_last_new_picture_media_s": round(last_new, 3),
        # None while the starved video holds the muxer's audio (no silence out yet)
        "pause_picture_minus_sound_s": None if onset is None else round(last_new - onset, 3),
        "pause_last_new_picture_emit_s": round(emit - sh.pause_wall, 3) if emit else None,
        "pause_frames_after_last_new": len(after),
        "pause_exact_picture_changes_after_last_new": sum(
            1 for a, b in zip(after, after[1:]) if a != b),
    }


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
        "step_ms": args.step_ms,
        "fragments": len(frags),
        "drops_video_audio": [sh.video_drops, sh.audio_drops],
        "video_written_repeated_skipped_maxburst": sh.video_stats,
        "padded_ms_skipped_ms": [sh.padded_frames // FRAMES_PER_MS, sh.skipped_frames // FRAMES_PER_MS],
        "silence_onset_media_s": None if onset is None else round(onset, 3),
        "silence_written_media_s": (round(sh.silence_written_frame / RATE, 3)
                                    if sh.silence_written_frame >= 0 else None),
    }
    if sh.pause_wall:
        res.update(pause_metrics(sh, args, path, frags, vtid, onset))
    if t_sw and onset is not None:
        res["video_media_at_switch_s"] = round(video_media_at_switch, 3)
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
    p.add_argument("--lead-ms", type=int, default=210,
                   help="the decode-seam lead: decode_seam_lead_ms() = 210 (the #184 G3 table "
                        "used 1500, the deleted SDK-clocked path's)")
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
    p.add_argument("--source", default="",
                   help="a lavfi source for detailed seam pictures, e.g. testsrc2 (empty = flat)")
    p.add_argument("--pause-at", type=float, default=0.0,
                   help="seconds after the audio feeder starts when the seam stops offering "
                        "(the pipeline's pause; 0 = never)")
    p.add_argument("--step-ms", type=int, default=0,
                   help="step the ffmpeg child's wall clock by this many ms (0 = never)")
    p.add_argument("--step-at", type=float, default=6.0,
                   help="seconds after the audio feeder starts when the wall clock steps")
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
