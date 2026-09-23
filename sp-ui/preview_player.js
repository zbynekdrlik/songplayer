// #178 live A/V preview — MSE shim.
//
// Owns everything the browser side of the preview needs: a MediaSource +
// SourceBuffer, the WebSocket to `/api/v1/playback/{id}/preview.ws`, an append
// queue (SourceBuffer.appendBuffer is asynchronous — one buffer at a time), a
// live-edge chase so a viewer that paused/backgrounded snaps back to "now", old
// buffer eviction, muted autoplay with one-click unmute (Chrome's autoplay
// gesture rule), and a clean teardown that closes the socket on unmount.
//
// Wrapped from Rust by `components/preview_video.rs` via wasm_bindgen. All error
// paths are swallowed (no console.error/warn) so a normal teardown or a codec
// hiccup never trips the zero-console-errors E2E gate.

const MIME = 'video/mp4; codecs="avc1.42E01E, mp4a.40.2"';
// Chase the live edge when the viewer falls more than this far behind it.
const LIVE_EDGE_MAX_S = 2;
// Drop buffered media older than this many seconds behind the playhead.
const EVICT_BEHIND_S = 30;
// Cap the append backlog: if appendBuffer keeps failing (a stuck SourceBuffer),
// drop the OLDEST fragments beyond this many chunks rather than growing without
// bound (#178 item 13).
const MAX_QUEUE = 40;
// Re-pump / maintain interval so progress continues even if `updateend` stops
// firing (a stuck append) — #178 item 13.
const PUMP_INTERVAL_MS = 250;

// #184 round G — transport lag. The preview WS on the public URL goes box →
// cloudflared → Cloudflare edge → back; when the tunnel dips below the stream
// rate a BACKLOG builds inside the tunnel that the round-F beacon cannot see
// (the beacon rides the same backlog). So the shim sends an application-level
// `{"ping": performance.now()}` every PING_INTERVAL_MS; the server echoes
// `{"pong": <same>}` on the same socket, behind the same backlog as the media,
// and `rtt = now − pong` is the real transport lag (both stamps are ours — no
// clock skew). A backlog the link can never drain is DROPPED by reconnecting
// (a new WebSocket = a new tunnel stream at the live edge).
export const PING_INTERVAL_MS = 1000;
// Two consecutive round trips over this → reconnect.
export const RTT_RECONNECT_MS = 3000;
// A ping unanswered for longer than this → reconnect.
export const NO_PONG_RECONNECT_MS = 5000;
// #184 round G2: reconnect attempts BACK OFF. Round G retried every ~12 s
// forever against an encoder that produced no init (the audio feeder starved
// ffmpeg), so the preview sat in a reconnect loop. The wait after a reconnect
// is RECONNECT_BACKOFF_BASE_MS, doubled for every further reconnect that has
// not delivered a media fragment yet, capped at RECONNECT_BACKOFF_MAX_MS
// (12 s → 24 s → 48 s → 60 s …). A socket that delivers its first media
// fragment resets the count.
export const RECONNECT_BACKOFF_BASE_MS = 12000;
export const RECONNECT_BACKOFF_MAX_MS = 60000;
// A socket that has not delivered its init segment this long after it was
// opened is lost (the server itself gives up waiting for the encoder's init
// after ~10 s and closes).
export const INIT_TIMEOUT_MS = 12000;
// How many recent round trips are kept.
const RTT_HISTORY = 5;
// A pong settles every pending ping sent up to this much after its own stamp
// (the server echoes the number verbatim; this only absorbs float noise).
const PONG_MATCH_TOLERANCE_MS = 0.5;

// Pure (#184 round G2): how long after the last reconnect the next one may
// happen, given `reconnectsWithoutMedia` — the reconnects made since a socket
// last delivered a media fragment. 0 (just reset by a healthy socket) and 1
// (the first retry) wait the base; each further fruitless retry doubles it, up
// to the cap. Anything that is not a positive integer is the base step.
export function reconnectGapMs(reconnectsWithoutMedia) {
  const n = Number.isInteger(reconnectsWithoutMedia) ? reconnectsWithoutMedia : 0;
  const doublings = Math.min(Math.max(n, 1) - 1, 16);
  return Math.min(RECONNECT_BACKOFF_BASE_MS * 2 ** doublings, RECONNECT_BACKOFF_MAX_MS);
}

// Pure (#184 round G2): the current socket is gone for good — never created
// (`hasSocket` false), closed on its own (`wsClosed`), or silent past
// INIT_TIMEOUT_MS without an init segment. Waiting for the FIRST init inside
// that window is never "lost" and never transport lag: the server sends the
// init only after the encoder child produced one (a cold start / the libx264
// fallback takes seconds).
export function socketLost({ hasSocket, wsClosed, gotInit, msSinceConnect }) {
  if (!hasSocket || wsClosed) return true;
  return !gotInit && msSinceConnect > INIT_TIMEOUT_MS;
}

// Pure reconnect decision (#184 round G), exported for the node-side table test
// in e2e/preview.spec.ts.
// - `rtts`: recent round trips (ms), oldest first.
// - `msAwaitingPong`: how long the OLDEST still-unanswered ping has been waiting
//   (0 when none is outstanding) — "no pong for N s". Measured from the ping,
//   never from the last pong, so a background tab whose timers are throttled to
//   one tick a minute does not read "no pong for 60 s" and reconnect for nothing
//   (its pongs arrive as events right after each rare ping). It also means the
//   rule cannot fire before a ping has been out for 5 s.
// - `msSinceLastReconnect`: ms since this player last reconnected, or null when
//   it never has.
// - `socketLost`: the socket closed on its own (a server restart, a relay
//   close) or never delivered its init within INIT_TIMEOUT_MS — there is
//   nothing left to measure, it is simply replaced (still rate-limited by the
//   backoff, so a server that is down is never hammered).
// - `reconnectsWithoutMedia` (#184 round G2): reconnects since a socket last
//   delivered media — picks the backoff step (`reconnectGapMs`).
export function shouldReconnect({
  rtts,
  msAwaitingPong,
  msSinceLastReconnect,
  socketLost: lost,
  reconnectsWithoutMedia,
}) {
  if (
    msSinceLastReconnect != null &&
    msSinceLastReconnect < reconnectGapMs(reconnectsWithoutMedia)
  ) {
    return false;
  }
  if (lost) return true;
  const n = rtts ? rtts.length : 0;
  if (n >= 2 && rtts[n - 1] > RTT_RECONNECT_MS && rtts[n - 2] > RTT_RECONNECT_MS) {
    return true;
  }
  return msAwaitingPong > NO_PONG_RECONNECT_MS;
}

// Pure: the ONE lag number (seconds) reported through `onLag` (#184 round G).
// The worst of what we know: the round-F beacon lag (`produced − buffered_end`,
// null when not computable), the LAST round trip, and how long the oldest
// unanswered ping has waited (a backlog is at least that deep). Taking the max
// keeps the round-F readout intact (a slow encoder side still shows) while the
// transport lag the beacon is blind to finally shows too. null = nothing
// measured yet (nothing is reported).
export function previewLagS({ beaconLagS, rtts, msAwaitingPong }) {
  const parts = [];
  if (typeof beaconLagS === 'number' && Number.isFinite(beaconLagS)) parts.push(beaconLagS);
  if (rtts && rtts.length > 0) parts.push(rtts[rtts.length - 1] / 1000);
  if (msAwaitingPong > 0) parts.push(msAwaitingPong / 1000);
  return parts.length ? Math.max(...parts) : null;
}

// Close a socket we are done with WITHOUT a console message: its handlers are
// nulled first (a late frame / close from it is ignored), and a socket that is
// still CONNECTING is closed once it opens instead — close() on a CONNECTING
// socket logs "WebSocket is closed before the connection is established"
// (#184 round G2: that line repeated every reconnect in the owner's console).
function closeQuietly(ws) {
  try {
    ws.onmessage = null;
    ws.onclose = null;
    ws.onerror = () => {};
    if (ws.readyState === WebSocket.CONNECTING) {
      ws.onopen = () => {
        try {
          ws.close();
        } catch (e) {
          // already closing
        }
      };
    } else {
      ws.close();
    }
  } catch (e) {
    // already closing
  }
}

export class PreviewPlayer {
  // `video` is the <video> element; `path` is the app-relative WS path
  // (e.g. "/api/v1/playback/1/preview.ws") — the ws:// scheme + host are
  // derived from window.location here. `startMuted` picks the initial mute
  // state: the card mounts the player from a real click (a user gesture), so
  // it starts UNMUTED (startMuted=false); if the browser still rejects the
  // unmuted play(), `_maintain` falls back to muted and the unmute button
  // remains for the user (#178 round 3).
  constructor(video, path, startMuted, onLag) {
    this.video = video;
    this.queue = [];
    this.sb = null;
    this.ws = null;
    this.destroyed = false;
    this._interval = null;
    // Set when the queue overflowed and we dropped fragments: the next _pump
    // clears the stale buffered range before appending the next keyframe
    // fragment, so the timeline resyncs cleanly (#178 item 13).
    this._needResync = false;
    // #184 round F: the server's produced media time (ms) from the latest 1 Hz
    // beacon, and the callback that reports the picture lag to the Rust side.
    this._producedMs = null;
    this.onLag = typeof onLag === 'function' ? onLag : null;
    // #184 round G: transport-lag state — the WS path (for reconnects), the 1 Hz
    // health/ping timer, the send times of pings still waiting for their pong
    // (oldest first), the recent round trips, when this player last
    // reconnected, and the current socket's lifecycle (when it was opened,
    // whether its init arrived, whether it closed on its own).
    this._path = null;
    this._pingTimer = null;
    this._pendingPings = [];
    this._rtts = [];
    this._lastReconnectAt = null;
    this._connectedAt = null;
    this._gotInit = false;
    this._wsClosed = false;
    // #184 round G2: whether the current socket delivered a media fragment (a
    // binary frame after its init), and how many reconnects were made since a
    // socket last did — the reconnect backoff step.
    this._gotMedia = false;
    this._reconnectsWithoutMedia = 0;
    // Set when a resync cleared the buffered range: the next time media is
    // buffered the playhead jumps to its START — the first sample after the
    // drop (a reconnect may land on a restarted media timeline, e.g. a new
    // encoder child, whose times are BEHIND the old playhead).
    this._snapToStart = false;

    video.muted = !!startMuted;
    video.autoplay = true;
    video.playsInline = true;

    this.ms = new MediaSource();
    this.objectUrl = URL.createObjectURL(this.ms);
    video.src = this.objectUrl;

    this._onUpdateEnd = () => this._pump();
    this.ms.addEventListener('sourceopen', () => this._onSourceOpen(path), {
      once: true,
    });
  }

  _onSourceOpen(path) {
    if (this.destroyed) return;
    try {
      this.sb = this.ms.addSourceBuffer(MIME);
    } catch (e) {
      // MSE/codec unsupported in this browser — nothing to append. Leave the
      // element as-is; the card still shows a (blank) <video>.
      return;
    }
    try {
      this.sb.mode = 'segments';
    } catch (e) {
      // Some engines pin the mode; harmless.
    }
    this.sb.addEventListener('updateend', this._onUpdateEnd);
    // Keep pumping/maintaining on a timer so a stuck append (updateend never
    // fires) still recovers, and the live-edge/mute maintenance keeps running
    // even with no incoming fragments (#178 item 13).
    this._interval = setInterval(() => {
      if (this.destroyed) return;
      this._pump();
      this._maintain();
      this._reportLag();
    }, PUMP_INTERVAL_MS);
    this._path = path;
    this._connect(path);
  }

  // Open the preview WebSocket (the ONE connect path — the first open and every
  // #184 round-G reconnect). Per-socket transport state starts fresh here, and
  // the 1 Hz health/ping timer runs for the socket's whole life (it replaces a
  // socket that dies or never delivers its init, and pings once the init came).
  _connect(path) {
    this._pendingPings = [];
    this._rtts = [];
    this._producedMs = null;
    this._connectedAt = performance.now();
    this._gotInit = false;
    this._gotMedia = false;
    this._wsClosed = false;
    this._startPinging();
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let wsPath = path;
    // Mock-E2E test seams: forward the #184 round-F `lag_ms` (inflates the
    // beacon) and round-G `pong_delay_ms` (delays every post-init frame — a
    // tunnel backlog) flags from the PAGE url to the preview WS. A normally
    // loaded dashboard never carries them, so the production preview.ws url is
    // unchanged; the mock reads them off the upgrade url.
    try {
      const params = new URLSearchParams(location.search);
      for (const key of ['lag_ms', 'pong_delay_ms']) {
        const v = params.get(key);
        if (v !== null && /^\d+$/.test(v)) {
          wsPath += (wsPath.includes('?') ? '&' : '?') + key + '=' + v;
        }
      }
    } catch (e) {
      // No URLSearchParams / a malformed search — use the plain path.
    }
    const url = scheme + '//' + location.host + wsPath;
    try {
      this.ws = new WebSocket(url);
    } catch (e) {
      // No socket at all — the health tick treats it as lost and retries.
      this.ws = null;
      return;
    }
    this.ws.binaryType = 'arraybuffer';
    // A socket that closes on its own (server restart, relay close, the
    // server's init timeout) is marked lost; the health tick replaces it. A
    // socket WE replace or tear down has this handler nulled first.
    this.ws.onclose = () => {
      this._wsClosed = true;
    };
    this.ws.onmessage = (ev) => {
      if (this.destroyed || !this.sb) return;
      // TEXT frames are control messages — the #184 round-F 1 Hz lag beacon
      // ({"produced_ms":N}) and the round-G pong ({"pong":N}); BINARY frames
      // are fMP4 fragments. Never push a text frame into the append queue, and
      // never let a non-JSON string reach the console.
      if (typeof ev.data === 'string') {
        let msg = null;
        try {
          msg = JSON.parse(ev.data);
        } catch (e) {
          // Not JSON — ignore.
        }
        if (msg && typeof msg.produced_ms === 'number') {
          this._producedMs = msg.produced_ms;
        }
        if (msg && typeof msg.pong === 'number') {
          this._onPong(msg.pong);
        }
        return;
      }
      // The first binary frame is the init segment: from here on the server's
      // WS loop is running and answers pings at once. Pinging starts HERE, not
      // at socket open — the server sends the init only after the encoder child
      // produced one (a cold start / the libx264 fallback can take seconds), and
      // that startup must never read as transport lag and trigger a reconnect.
      if (this._gotInit && !this._gotMedia) {
        // The first MEDIA fragment on this socket (the frame after its init):
        // the stream is really flowing — reset the reconnect backoff.
        this._gotMedia = true;
        this._reconnectsWithoutMedia = 0;
      }
      this._gotInit = true;
      this.queue.push(new Uint8Array(ev.data));
      if (this.queue.length > MAX_QUEUE) {
        // Drop the oldest fragments; they will never be appended fast enough.
        // The kept fragments are keyframe-aligned but now have a gap before
        // them, so resync the buffered range on the next append.
        while (this.queue.length > MAX_QUEUE) this.queue.shift();
        this._needResync = true;
      }
      this._pump();
    };
    // Swallow errors — a teardown close or a transient network blip must not
    // reach the console (zero-console-errors gate).
    this.ws.onerror = () => {};
  }

  // #184 round G: the 1 Hz health/ping loop. Each tick first asks whether the
  // socket must be replaced (hopelessly backlogged, or dead), else — once the
  // init arrived — sends `{"ping": performance.now()}` and remembers its send
  // time until the pong comes back.
  _startPinging() {
    this._stopPinging();
    this._pingTimer = setInterval(() => this._pingTick(), PING_INTERVAL_MS);
  }

  _stopPinging() {
    if (this._pingTimer) {
      clearInterval(this._pingTimer);
      this._pingTimer = null;
    }
  }

  _pingTick() {
    if (this.destroyed) return;
    if (this._maybeReconnect()) return;
    // No pings before the init (see onmessage) or on a socket that is not open.
    if (!this._gotInit || !this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const now = performance.now();
    try {
      this.ws.send(JSON.stringify({ ping: now }));
      this._pendingPings.push(now);
    } catch (e) {
      // Socket closing — the next tick (or a reconnect) handles it.
    }
  }

  _onPong(sentAt) {
    const now = performance.now();
    // Pongs come back in order on one socket, so this pong also settles every
    // older ping still pending (their answers can only be behind it).
    this._pendingPings = this._pendingPings.filter(
      (t) => t > sentAt + PONG_MATCH_TOLERANCE_MS,
    );
    this._rtts.push(now - sentAt);
    if (this._rtts.length > RTT_HISTORY) this._rtts.shift();
    this._maybeReconnect();
  }

  // How long the oldest unanswered ping has waited (ms), 0 when none is out.
  _msAwaitingPong(now) {
    return this._pendingPings.length ? now - this._pendingPings[0] : 0;
  }

  // The current socket is gone for good: never created, closed on its own, or
  // silent past INIT_TIMEOUT_MS without an init segment.
  _socketLost(now) {
    return socketLost({
      hasSocket: !!this.ws,
      wsClosed: this._wsClosed,
      gotInit: this._gotInit,
      msSinceConnect: now - this._connectedAt,
    });
  }

  _maybeReconnect() {
    const now = performance.now();
    const decide = shouldReconnect({
      rtts: this._rtts,
      msAwaitingPong: this._msAwaitingPong(now),
      msSinceLastReconnect:
        this._lastReconnectAt == null ? null : now - this._lastReconnectAt,
      socketLost: this._socketLost(now),
      reconnectsWithoutMedia: this._reconnectsWithoutMedia,
    });
    if (decide) this._reconnect(now);
    return decide;
  }

  // Replace the socket — to drop a backlog the link can never catch up with, or
  // because it died: close the old socket (its handlers nulled first so a late
  // frame / close from it is ignored), discard the queued fragments, mark the
  // buffered range for a reset before the new socket's first frame (the
  // existing resync path), and open a fresh socket on the same path — a new
  // tunnel stream that starts at the live edge. The lag readout drops to 0: the
  // stale backlog it measured is gone; the new socket's pings re-measure it.
  _reconnect(now) {
    this._lastReconnectAt = now;
    this._reconnectsWithoutMedia += 1;
    if (this.ws) {
      const old = this.ws;
      this.ws = null;
      closeQuietly(old);
    }
    this.queue = [];
    this._needResync = true;
    if (this.onLag) {
      try {
        this.onLag(0);
      } catch (e) {
        // callback gone — ignore.
      }
    }
    this._connect(this._path);
  }

  // Report the ONE picture-lag number (seconds) to the Rust side each pump
  // tick — see `previewLagS`. The round-F beacon part needs both the beacon
  // and a buffered range; the round-G parts need neither, so a backlogged
  // socket whose media has not even arrived still shows its lag.
  _reportLag() {
    if (!this.onLag) return;
    let beaconLagS = null;
    try {
      const buf = this.video.buffered;
      if (this._producedMs != null && buf.length > 0) {
        beaconLagS = this._producedMs / 1000 - buf.end(buf.length - 1);
      }
    } catch (e) {
      // element gone — no beacon part.
    }
    const lag = previewLagS({
      beaconLagS,
      rtts: this._rtts,
      msAwaitingPong: this._msAwaitingPong(performance.now()),
    });
    if (lag == null) return;
    try {
      this.onLag(lag);
    } catch (e) {
      // callback gone — ignore.
    }
  }

  _pump() {
    if (this.destroyed || !this.sb || this.sb.updating) return;
    if (this.queue.length === 0) {
      this._maintain();
      return;
    }
    if (this._needResync) {
      // The queue overflowed and we dropped fragments (#178 item 13), or a
      // #184 round-G reconnect dropped a backlog — clear the stale buffered
      // range right before the next queued frame is appended, so the new data
      // starts a clean timeline. After a reconnect that frame is the new
      // socket's init segment, so the old media plays until the new socket
      // delivers, then the picture holds its last frame until the first new
      // fragment (~0.5 s). The playhead then jumps to the first new sample.
      this._needResync = false;
      if (this._clearBuffered()) {
        this._snapToStart = true;
        return; // a remove() started; resume on updateend
      }
    }
    const chunk = this.queue.shift();
    try {
      this.sb.appendBuffer(chunk);
    } catch (e) {
      // QuotaExceededError: evict old data and retry this chunk on the next
      // updateend / maintain tick. Any other error: drop the chunk.
      if (e && e.name === 'QuotaExceededError') {
        this.queue.unshift(chunk);
        this._evict(true);
      }
    }
  }

  _maintain() {
    const v = this.video;
    const buf = v.buffered;
    if (buf.length === 0) return;
    const start = buf.start(0);
    const end = buf.end(buf.length - 1);
    // (The #184 picture-lag report lives in `_reportLag`, run each pump tick.)
    // A live fMP4 fragment can begin at a non-zero media time, so the element's
    // currentTime (0 at mount) can sit BEFORE the first buffered sample — MSE
    // then never renders. Snap into the buffered range (#178 round 3). After a
    // resync cleared the range (`_snapToStart`), snap to the first new sample
    // unconditionally: a #184 round-G reconnect can land on a RESTARTED media
    // timeline (a new encoder child starts at 0) whose times are BEHIND the old
    // playhead, which would otherwise stall past the end of everything buffered.
    // Never while the clearing remove() is still pending: `buffered` would
    // still show the OLD range and the flag would be spent on its stale start.
    const clearing = !!(this.sb && this.sb.updating);
    if ((this._snapToStart && !clearing) || v.currentTime < start) {
      try {
        v.currentTime = start + 0.01;
        // Only a snap onto the NEW range spends the flag.
        if (!clearing) this._snapToStart = false;
      } catch (e) {
        // currentTime may reject during a pending seek — retried next tick.
      }
    }
    // Live-edge chase: if we have drifted too far behind the newest buffered
    // media, jump to just behind the edge.
    if (end - v.currentTime > LIVE_EDGE_MAX_S) {
      try {
        v.currentTime = end - 0.1;
      } catch (e) {
        // currentTime may reject during a pending seek — retried next tick.
      }
    }
    if (v.paused) {
      const p = v.play();
      if (p && p.catch) {
        p.catch(() => {
          // An unmuted autoplay can be rejected without a fresh gesture — fall
          // back to muted playback and retry; the unmute button re-enables audio.
          if (!v.muted) {
            v.muted = true;
            const p2 = v.play();
            if (p2 && p2.catch) p2.catch(() => {});
          }
        });
      }
    }
    this._evict(false);
  }

  _clearBuffered() {
    // Remove the whole buffered range so a resync after a dropped-fragment gap
    // starts fresh at the next keyframe fragment. Returns true if a remove()
    // was started (the SourceBuffer is now updating → updateend re-pumps).
    if (!this.sb || this.sb.updating) return false;
    let buf;
    try {
      buf = this.sb.buffered;
    } catch (e) {
      return false;
    }
    if (buf.length === 0) return false;
    try {
      this.sb.remove(buf.start(0), buf.end(buf.length - 1));
      return true;
    } catch (e) {
      return false;
    }
  }

  _evict(force) {
    if (!this.sb || this.sb.updating) return;
    let buf;
    try {
      buf = this.sb.buffered;
    } catch (e) {
      return;
    }
    if (buf.length === 0) return;
    const start = buf.start(0);
    const cutoff = force
      ? this.video.currentTime - 1
      : this.video.currentTime - EVICT_BEHIND_S;
    if (cutoff > start) {
      try {
        this.sb.remove(start, cutoff);
      } catch (e) {
        // remove() throws only if updating — retried next tick.
      }
    }
  }

  unmute() {
    this.video.muted = false;
    const p = this.video.play();
    if (p && p.catch) p.catch(() => {});
  }

  fullscreen() {
    const v = this.video;
    const req =
      v.requestFullscreen || v.webkitRequestFullscreen || v.msRequestFullscreen;
    if (req) {
      const r = req.call(v);
      if (r && r.catch) r.catch(() => {});
    }
  }

  destroy() {
    this.destroyed = true;
    this.queue = [];
    if (this._interval) {
      clearInterval(this._interval);
      this._interval = null;
    }
    this._stopPinging();
    this._pendingPings = [];
    this._rtts = [];
    if (this.ws) {
      closeQuietly(this.ws);
      this.ws = null;
    }
    if (this.sb) {
      try {
        this.sb.removeEventListener('updateend', this._onUpdateEnd);
      } catch (e) {
        // detached
      }
    }
    try {
      if (this.ms && this.ms.readyState === 'open') this.ms.endOfStream();
    } catch (e) {
      // not open / already ended
    }
    try {
      this.video.pause();
      this.video.removeAttribute('src');
      this.video.load();
    } catch (e) {
      // element gone
    }
    try {
      if (this.objectUrl) URL.revokeObjectURL(this.objectUrl);
    } catch (e) {
      // already revoked
    }
    this.sb = null;
    this.ms = null;
    this.objectUrl = null;
  }
}
