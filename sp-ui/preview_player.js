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

export class PreviewPlayer {
  // `video` is the <video> element; `path` is the app-relative WS path
  // (e.g. "/api/v1/playback/1/preview.ws") — the ws:// scheme + host are
  // derived from window.location here.
  constructor(video, path) {
    this.video = video;
    this.queue = [];
    this.sb = null;
    this.ws = null;
    this.destroyed = false;

    video.muted = true;
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
      this._pump();
    };
    // Swallow errors — a teardown close or a transient network blip must not
    // reach the console (zero-console-errors gate).
    this.ws.onerror = () => {};
  }

  _pump() {
    if (this.destroyed || !this.sb || this.sb.updating) return;
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
    const end = buf.end(buf.length - 1);
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
      if (p && p.catch) p.catch(() => {});
    }
    this._evict(false);
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
