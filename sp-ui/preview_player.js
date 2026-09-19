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

export class PreviewPlayer {
  // `video` is the <video> element; `path` is the app-relative WS path
  // (e.g. "/api/v1/playback/1/preview.ws") — the ws:// scheme + host are
  // derived from window.location here. `startMuted` picks the initial mute
  // state: the card mounts the player from a real click (a user gesture), so
  // it starts UNMUTED (startMuted=false); if the browser still rejects the
  // unmuted play(), `_maintain` falls back to muted and the unmute button
  // remains for the user (#178 round 3).
  constructor(video, path, startMuted) {
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
    }, PUMP_INTERVAL_MS);
    this._openWs(path);
  }

  _openWs(path) {
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = scheme + '//' + location.host + path;
    try {
      this.ws = new WebSocket(url);
    } catch (e) {
      return;
    }
    this.ws.binaryType = 'arraybuffer';
    this.ws.onmessage = (ev) => {
      if (this.destroyed || !this.sb) return;
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

  _pump() {
    if (this.destroyed || !this.sb || this.sb.updating) return;
    if (this._needResync) {
      // The queue overflowed and we dropped fragments — clear the stale
      // buffered range so the next keyframe-aligned fragment starts a clean
      // timeline (#178 item 13).
      this._needResync = false;
      if (this._clearBuffered()) return; // a remove() started; resume on updateend
    }
    if (this.queue.length === 0) {
      this._maintain();
      return;
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
    // A live fMP4 fragment can begin at a non-zero media time, so the element's
    // currentTime (0 at mount) can sit BEFORE the first buffered sample — MSE
    // then never renders. Snap into the buffered range (#178 round 3).
    if (v.currentTime < start) {
      try {
        v.currentTime = start + 0.01;
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
    if (this.ws) {
      try {
        this.ws.onmessage = null;
        this.ws.onerror = null;
        this.ws.close();
      } catch (e) {
        // already closing
      }
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
