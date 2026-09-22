import express from "express";
import { WebSocketServer } from "ws";
import { createServer } from "http";
import { readFileSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// Read the workspace VERSION file so the mock's /api/v1/status `version`
// field matches the same source the WASM frontend reads via
// sp_core::config::VERSION → CARGO_PKG_VERSION → workspace VERSION. The
// dashboard version-label spec asserts the two are equal (#85).
const SP_VERSION = readFileSync(join(__dirname, "..", "VERSION"), "utf8").trim();

// #178: canned fragmented-MP4 fixture streamed over the preview WebSocket.
// Split into the init segment (ftyp+moov) + one chunk per moof..next-moof
// fragment, mirroring the real server's fMP4 relay (init first, then
// keyframe-aligned fragments). Lets the chrome-channel preview E2E drive the
// MSE <video> to readyState>=3 with an advancing currentTime.
function splitFmp4(buf) {
  const boxes = [];
  let o = 0;
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  while (o + 8 <= buf.length) {
    let size = dv.getUint32(o);
    const type = String.fromCharCode(
      buf[o + 4],
      buf[o + 5],
      buf[o + 6],
      buf[o + 7],
    );
    let header = 8;
    if (size === 1) {
      size = Number(dv.getBigUint64(o + 8));
      header = 16;
    }
    if (size < header || o + size > buf.length) break;
    boxes.push({ type, start: o, end: o + size });
    o += size;
  }
  const firstMoof = boxes.findIndex((b) => b.type === "moof");
  if (firstMoof < 0) return [buf];
  const init = buf.subarray(0, boxes[firstMoof].start);
  const frags = [];
  for (let i = firstMoof; i < boxes.length; i++) {
    if (boxes[i].type !== "moof") continue;
    let end = buf.length;
    for (let j = i + 1; j < boxes.length; j++) {
      if (boxes[j].type === "moof") {
        end = boxes[j].start;
        break;
      }
    }
    frags.push(buf.subarray(boxes[i].start, end));
  }
  return [init, ...frags];
}
const PREVIEW_FMP4 = splitFmp4(
  readFileSync(join(__dirname, "fixtures", "preview-fixture.mp4")),
);

const app = express();
app.use(express.json());

// Connected WebSocket clients, so a test-only admin endpoint can broadcast a
// server-push message (e.g. a LyricsUpdate) to the live dashboard. Populated
// in the `wss.on("connection")` handler at the bottom of this file.
const wsClients = new Set();

// Serve the built WASM frontend from dist/
const distPath = join(__dirname, "..", "dist");
app.use(express.static(distPath));

// --- Mock data ---

const playlists = [
  {
    id: 1,
    name: "Worship",
    youtube_url: "https://youtube.com/playlist?list=PLtest1",
    ndi_output_name: "SP-worship",
    playback_mode: "continuous",
    is_active: true,
    created_at: "2026-01-01 00:00:00",
    updated_at: "2026-01-01 00:00:00",
  },
  {
    id: 2,
    name: "Background",
    youtube_url: "https://youtube.com/playlist?list=PLtest2",
    ndi_output_name: "SP-background",
    playback_mode: "continuous",
    is_active: true,
    created_at: "2026-01-01 00:00:00",
    updated_at: "2026-01-01 00:00:00",
  },
  // v0.22.0 addition — /live page resolves the ytlive playlist by name,
  // so the mock must expose one for the mobile-live Playwright test.
  {
    id: 184,
    name: "ytlive",
    youtube_url: "",
    ndi_output_name: "SP-live",
    playback_mode: "continuous",
    is_active: true,
    kind: "custom",
    created_at: "2026-01-01 00:00:00",
    updated_at: "2026-01-01 00:00:00",
  },
];

// #165: opt-in 12-playlist fixture for the dashboard-redesign spec (playlist
// selector + single workspace). Playlist id 1 ("Playlist 01") is the
// currently-playing one — the WS connection handler below marks playlist 1
// Playing regardless of the fixture mode, so the selector's "playing
// preselected + ▶" behaviour is exercised with no extra WS wiring. Only
// playlist 1 carries videos (it reuses the base `videos` fixture).
const twelvePlaylists = Array.from({ length: 12 }, (_, i) => {
  const id = i + 1;
  const nn = String(id).padStart(2, "0");
  return {
    id,
    name: `Playlist ${nn}`,
    youtube_url: `https://youtube.com/playlist?list=PLmock${nn}`,
    ndi_output_name: `SP-${nn}`,
    playback_mode: "continuous",
    is_active: true,
    created_at: "2026-01-01 00:00:00",
    updated_at: "2026-01-01 00:00:00",
  };
});

// "default" → the 3-playlist fixture above (every other spec relies on it);
// "twelve" → the 12-playlist fixture. The #165 spec POSTs "twelve" in
// beforeEach and resets to "default" in afterEach so no state leaks into the
// serially-run sibling spec files (playwright.config.ts pins workers: 1).
let fixtureMode = "default";
function activePlaylists() {
  return fixtureMode === "twelve" ? twelvePlaylists : playlists;
}

// `normalized` and `gemini_failed` are required (non-`#[serde(default)]`)
// fields on sp_core::models::Video — every fixture must include them or
// the dashboard's VideoList (#134) fails to deserialize GET
// /api/v1/playlists/:id/videos entirely.
const videos = [
  {
    id: 1,
    playlist_id: 1,
    youtube_id: "dQw4w9WgXcQ",
    title: "Never Gonna Give You Up",
    artist: "Rick Astley",
    duration_ms: 213000,
    cached: true,
    normalized: true,
    gemini_failed: false,
    download_attempts: 0,
    last_download_error: null,
    // #177: stems-ready song → karaoke works; the list marker shows ● and the
    // "len so stemami" filter keeps it.
    stems_state: "ready",
  },
  {
    id: 2,
    playlist_id: 1,
    youtube_id: "abc123",
    title: "Amazing Grace",
    artist: "Traditional",
    duration_ms: 180000,
    cached: false,
    normalized: false,
    gemini_failed: false,
    // #140: exercises the VideoList "⚠" hint on an un-normalized row that
    // has failed and is backing off.
    download_attempts: 2,
    last_download_error: "yt-dlp exited with 1: Requested format is not available",
    // #177: no stems yet → the filter hides it.
    stems_state: "queued",
  },
  {
    // #136 T1: a row with the exact Gemini-failed swap shape the ytalex
    // playlist hit (song holds the wrong half, artist the initialized
    // band). Owned by video-list-edit.spec.ts so correcting it in place
    // does not disturb the id=1/id=2 play-button specs.
    id: 3,
    playlist_id: 1,
    youtube_id: "nwmrD1k6yNE",
    title: "planetboom - Break Every Chain",
    song: "planetboom",
    artist: "P. Break!",
    duration_ms: 201000,
    cached: true,
    normalized: true,
    gemini_failed: true,
    download_attempts: 0,
    last_download_error: null,
    // #177: terminal-unsupported stems.
    stems_state: "unavailable",
  },
];

const settings = {
  obs_websocket_url: "ws://127.0.0.1:4455",
  obs_websocket_password: "",
  gemini_api_key: "",
  gemini_model: "gemini-2.5-flash",
  cache_dir: "./cache",
  // #184 round C: the pinned dub voice (Nastavenia "Hlas dabingu" select).
  dub_voice: "Charon",
};

const resolumeHosts = [];
let nextResolumeId = 1;

// --- REST API ---

// Playlists
app.get("/api/v1/playlists", (_req, res) => {
  res.json(activePlaylists());
});

// #165: switch the playlists fixture between "default" (3) and "twelve" (12).
app.post("/__mock/fixture", (req, res) => {
  fixtureMode = req.body?.mode === "twelve" ? "twelve" : "default";
  res.json({ mode: fixtureMode, count: activePlaylists().length });
});

app.post("/api/v1/playlists", (req, res) => {
  const pl = { id: playlists.length + 1, ...req.body };
  playlists.push(pl);
  res.status(201).json(pl);
});

app.get("/api/v1/playlists/:id", (req, res) => {
  const pl = activePlaylists().find((p) => p.id === Number(req.params.id));
  if (pl) res.json(pl);
  else res.status(404).json({ error: "not found" });
});

app.delete("/api/v1/playlists/:id", (_req, res) => {
  res.status(204).end();
});

// Videos
app.get("/api/v1/playlists/:id/videos", (req, res) => {
  const pid = Number(req.params.id);
  res.json(videos.filter((v) => v.playlist_id === pid));
});

// #136 T1: operator metadata correction. Mirrors the real patch_video —
// rejects a whitespace-only `song` with 400, clears an empty artist to
// NULL, and mutates the in-memory fixture so the dashboard's reload shows
// the corrected value (the E2E asserts the round-trip in the real browser).
app.patch("/api/v1/videos/:id", (req, res) => {
  const v = videos.find((x) => x.id === Number(req.params.id));
  if (!v) {
    res.status(404).end();
    return;
  }
  const body = req.body || {};
  if (typeof body.song === "string" && body.song.trim() === "") {
    res.status(400).end();
    return;
  }
  if (typeof body.song === "string") v.song = body.song.trim();
  if (typeof body.artist === "string") {
    v.artist = body.artist.trim() === "" ? null : body.artist.trim();
  }
  res.status(204).end();
});

// Playlist sync
app.post("/api/v1/playlists/:id/sync", (_req, res) => {
  res.json({ status: "syncing" });
});

// Live-setlist state (custom-kind playlists). In-memory; survives within
// a single mock-api process so the /live page persistence test (#39)
// reloads and finds the items still there. Tests that need a clean slate
// POST to `/__mock/live-reset`.
let liveItems = [];

function makeLiveRow(video_id, position) {
  // Look up the source video from the catalog if available so the row
  // matches the dashboard's expectations (song / artist labels).
  const v = videos.find((x) => x.id === video_id);
  return {
    video_id,
    youtube_id: v?.youtube_id || `yt-${video_id}`,
    song: v?.title || `Song ${video_id}`,
    artist: v?.artist || "Mock Artist",
    position,
    has_lyrics: true,
  };
}

app.get("/api/v1/playlists/:id/items", (req, res) => {
  if (Number(req.params.id) === 184) {
    res.json([...liveItems]);
    return;
  }
  res.json([]);
});

app.post("/api/v1/playlists/:id/items", (req, res) => {
  const id = Number(req.params.id);
  const { video_id } = req.body || {};
  if (id !== 184 || typeof video_id !== "number") {
    res.status(400).json({ error: "expected ytlive playlist + video_id" });
    return;
  }
  if (liveItems.some((r) => r.video_id === video_id)) {
    res.json({ status: "already_present" });
    return;
  }
  const position = liveItems.length + 1;
  liveItems.push(makeLiveRow(video_id, position));
  res.json({ status: "added", position });
});

app.delete("/api/v1/playlists/:id/items/:vid", (req, res) => {
  const id = Number(req.params.id);
  const vid = Number(req.params.vid);
  if (id !== 184) {
    res.status(404).json({ error: "no such playlist" });
    return;
  }
  liveItems = liveItems.filter((r) => r.video_id !== vid);
  // Re-compact positions so the UI shows 1..N contiguous.
  liveItems = liveItems.map((r, idx) => ({ ...r, position: idx + 1 }));
  res.status(204).end();
});

app.post("/api/v1/playlists/:id/play-video", (_req, res) => {
  // No-op for the mock — the WS interval already streams NowPlaying
  // updates, so the dashboard sees activity without us replaying.
  res.status(204).end();
});

app.post("/__mock/live-reset", (_req, res) => {
  liveItems = [];
  res.json({ status: "reset" });
});

// Playback controls.
// Tests can flip individual endpoints to a fail mode via the admin
// helper below so the UI's error-handling path is exercisable.
const failModes = {
  play: false,
  pause: false,
  skip: false,
  previous: false,
  mode: false,
  preview: false,
};

function maybeFail(kind, res) {
  if (failModes[kind]) {
    res.status(500).json({ error: `mock: ${kind} fail-mode` });
    return true;
  }
  return false;
}

app.post("/api/v1/playback/:id/play", (_req, res) => {
  if (maybeFail("play", res)) return;
  res.json({ status: "playing" });
});

app.post("/api/v1/playback/:id/pause", (_req, res) => {
  if (maybeFail("pause", res)) return;
  res.json({ status: "paused" });
});

app.post("/api/v1/playback/:id/skip", (_req, res) => {
  if (maybeFail("skip", res)) return;
  res.json({ status: "skipped" });
});

app.post("/api/v1/playback/:id/previous", (_req, res) => {
  if (maybeFail("previous", res)) return;
  res.json({ status: "rewound" });
});

app.put("/api/v1/playback/:id/mode", (_req, res) => {
  if (maybeFail("mode", res)) return;
  res.json({ status: "mode_changed" });
});

// #194: the shared Player + LyricsScroller seek to a position via
// `POST /api/v1/playback/{id}/seek {position_ms}` (moved off the old
// `/api/v1/playlists/{id}/seek` route). A no-op 204 for the mock — specs that
// need the body intercept it with `page.route` before it reaches here.
app.post("/api/v1/playback/:id/seek", (_req, res) => {
  res.status(204).end();
});

// #15 part 2: live video preview. A minimal 1x1 JPEG so the dashboard <img>
// gets a decodable image (non-zero naturalWidth) for playlist 1 (which the WS
// stream marks Playing below); other playlists have no frame → 204 (idle).
const PREVIEW_JPEG = Buffer.from(
  "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRof" +
    "Hh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAAB" +
    "AAAAAAAAAAAAAAAAAAAAAv/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AfwD/2Q==",
  "base64",
);
app.get("/api/v1/playback/:id/preview.jpg", (req, res) => {
  if (maybeFail("preview", res)) return;
  if (String(req.params.id) === "1") {
    res.set("Content-Type", "image/jpeg");
    res.set("Cache-Control", "no-store");
    res.send(PREVIEW_JPEG);
  } else {
    res.status(204).end();
  }
});

// Admin: flip a playback endpoint into 500-failure mode.
// Test-only — used by Playwright specs to assert the UI's error path.
app.post("/__mock/fail-mode", (req, res) => {
  const { kind, enabled } = req.body || {};
  if (!(kind in failModes)) {
    res.status(400).json({ error: `unknown kind: ${kind}` });
    return;
  }
  failModes[kind] = !!enabled;
  res.json({ kind, enabled: failModes[kind] });
});

// Control endpoint (used by playback_controls component via WebSocket ClientMsg)
app.post("/api/v1/control", (_req, res) => {
  res.json({ status: "ok" });
});

// Settings
app.get("/api/v1/settings", (_req, res) => {
  res.json(settings);
});

app.patch("/api/v1/settings", (req, res) => {
  for (const [key, value] of Object.entries(req.body)) {
    settings[key] = value;
  }
  res.json(settings);
});

// Karaoke (#14, #177): live mode + vocal gain + stem progress + per-song
// now-playing stems state. `/__mock/mix-last` exposes the last PATCH body
// so specs can assert what the UI sent. `now_playing[]` binds the panel to the
// SELECTED playlist's song — default: playlist 1's song is stems-READY so the
// mode/fader controls are enabled (the #14/#186 specs rely on that). The #177
// spec flips it via `/__mock/karaoke-now-playing`.
let karaokeNowPlaying = [
  {
    playlist_id: 1,
    video_id: 1,
    title: "Never Gonna Give You Up",
    stems_state: "ready",
    stems_error: null,
    queue_position: null,
  },
];
// #184 round G: the ONE live mixer console. GET returns the three fader positions
// + stem progress + the per-song now-playing block; PATCH sets any subset of the
// faders. `/__mock/mix-last` exposes the last PATCH body so specs can assert which
// fields the UI sent; `/__mock/mix-reset` restores the default (1,1,1) console.
let mix = { vokaly: 1.0, podklad: 1.0, dabing: 1.0 };
let lastMixPatch = null;
const clamp01 = (x) => Math.max(0, Math.min(1, x));
app.get("/api/v1/mix", (_req, res) => {
  res.json({
    ...mix,
    stems_pending: 2,
    stems_done: 5,
    now_playing: karaokeNowPlaying,
  });
});
app.patch("/api/v1/mix", (req, res) => {
  const b = req.body || {};
  lastMixPatch = b;
  for (const k of ["vokaly", "podklad", "dabing"]) {
    if (typeof b[k] === "number" && !Number.isNaN(b[k])) mix[k] = clamp01(b[k]);
  }
  res.status(200).json({ ...mix });
});
app.get("/__mock/mix-last", (_req, res) => {
  res.json(lastMixPatch || {});
});
app.post("/__mock/mix-reset", (_req, res) => {
  mix = { vokaly: 1.0, podklad: 1.0, dabing: 1.0 };
  lastMixPatch = null;
  res.json({ ok: true });
});
// #177 admin: replace the now_playing[] array so a spec can drive the disabled
// controls + enqueue button for an unavailable / failed / queued song.
app.post("/__mock/karaoke-now-playing", (req, res) => {
  if (!Array.isArray(req.body)) {
    res.status(400).json({ error: "expected a JSON array" });
    return;
  }
  karaokeNowPlaying = req.body;
  res.json({ status: "set", count: karaokeNowPlaying.length });
});

// #177: operator "Zaradiť do fronty" — re-enqueue a song for stem separation.
// Records the last enqueued id so the spec can assert the backend effect.
let lastEnqueuedVideoId = null;
app.post("/api/v1/stems/:video_id/enqueue", (req, res) => {
  lastEnqueuedVideoId = Number(req.params.video_id);
  res.json({ status: "enqueued", queue_position: 1 });
});
app.get("/__mock/stems-enqueue-last", (_req, res) => {
  res.json({ video_id: lastEnqueuedVideoId });
});

// --- Dabing (#180) ---
// The seeded kind='dabing' playlist + an in-memory list of dub-requested
// videos. Mirrors GET /api/v1/dabing → {playlist_id, videos:[DubRow…]} and the
// import / toggle / mixer routes.
const DABING_PLAYLIST_ID = 500;
let dubRows = [];
let nextDubId = 9000;

function chainStateFor(dubStatus) {
  // The mock only ever produces `queued` rows (the chain is D3/D4); the server
  // derives the rest. Keep it in lockstep with the wire strings.
  return dubStatus === "none" ? "queued" : dubStatus;
}

app.get("/api/v1/dabing", (_req, res) => {
  res.json({ playlist_id: DABING_PLAYLIST_ID, videos: dubRows });
});

app.post("/api/v1/dabing/import", (req, res) => {
  const url = (req.body && req.body.url) || "";
  if (!url.trim()) {
    res.status(400).end();
    return;
  }
  const video_id = nextDubId++;
  const row = {
    video_id,
    playlist_id: DABING_PLAYLIST_ID,
    title: `Dabing ${video_id}`,
    dub_status: "queued",
    dub_error: null,
    dub_mix_ratio: 1.0,
    dub_file_path: null,
    stem_status: null,
    lyrics_present: false,
    chain_state: "queued",
    dub_voice: null,
  };
  // Newest first.
  dubRows.unshift(row);
  res.status(201).json({
    video_id,
    youtube_id: "mockdab0001",
    title: row.title,
  });
});

let lastDubToggle = null;
app.patch("/api/v1/videos/:id/dub", (req, res) => {
  const id = Number(req.params.id);
  const requested = !!(req.body && req.body.requested);
  lastDubToggle = { video_id: id, requested };
  if (requested) {
    if (!dubRows.some((r) => r.video_id === id)) {
      dubRows.unshift({
        video_id: id,
        playlist_id: DABING_PLAYLIST_ID,
        title: `Video ${id}`,
        dub_status: "queued",
        dub_error: null,
        dub_mix_ratio: 1.0,
        dub_file_path: null,
        stem_status: null,
        lyrics_present: false,
        chain_state: chainStateFor("queued"),
        dub_voice: null,
      });
    }
  } else {
    dubRows = dubRows.filter((r) => r.video_id !== id);
  }
  res.status(204).end();
});
app.get("/__mock/dub-toggle-last", (_req, res) => {
  res.json({ toggle: lastDubToggle });
});

// #184 round G: the per-video dub-mix route is DELETED — the mixer is now the ONE
// global console (`PATCH /api/v1/mix`, above).

// #183 round 2: push a fully-formed DubRow so a test can assert a terminal state
// (e.g. a dub-ready video WITHOUT stems — stem_status stays null — still renders
// `pripravené`, proving the 2-stream mix path is a first-class ready state).
app.post("/__mock/dabing-add", (req, res) => {
  const b = req.body || {};
  const video_id = Number(b.video_id ?? nextDubId++);
  const dub_status = b.dub_status ?? "ready";
  const row = {
    video_id,
    playlist_id: DABING_PLAYLIST_ID,
    title: b.title ?? `Dabing ${video_id}`,
    dub_status,
    dub_error: b.dub_error ?? null,
    dub_mix_ratio: b.dub_mix_ratio ?? 1.0,
    dub_file_path: b.dub_file_path ?? "/c/a_dub.flac",
    stem_status: b.stem_status ?? null,
    lyrics_present: !!b.lyrics_present,
    chain_state: b.chain_state ?? chainStateFor(dub_status),
    dub_voice: b.dub_voice ?? null,
  };
  dubRows.unshift(row);
  res.status(201).json(row);
});

app.post("/__mock/dabing-reset", (_req, res) => {
  dubRows = [];
  nextDubId = 9000;
  res.json({ ok: true });
});

// Status
app.get("/api/v1/status", (_req, res) => {
  res.json({
    version: SP_VERSION,
    obs_connected: false,
    active_scene: null,
    ytdlp_available: true,
    ffmpeg_available: true,
    playlists_count: activePlaylists().length,
    // #51: LAN sp.local advertisement — the dashboard's LanAddress component
    // reads these to show the offline-LAN URL + raw-IP fallback.
    lan_url: "http://sp.local:8920",
    lan_ip: "10.77.9.201",
  });
});

// Resolume hosts
app.get("/api/v1/resolume/hosts", (_req, res) => {
  res.json(resolumeHosts);
});

app.post("/api/v1/resolume/hosts", (req, res) => {
  const host = { id: nextResolumeId++, ...req.body };
  resolumeHosts.push(host);
  res.status(201).json(host);
});

app.delete("/api/v1/resolume/hosts/:id", (_req, res) => {
  res.status(204).end();
});

// Resolume push-chain health — polled every 5 s by the dashboard's
// ResolumeHealthCard on a spawn_local loop. An empty array means no
// configured hosts / no alerts, which keeps the quiet-by-default card
// hidden. The endpoint must exist so the frontend's async completion
// (snapshot.set) actually runs in the E2E instead of failing to
// deserialize the SPA-fallback index.html.
app.get("/api/v1/resolume/health", (_req, res) => {
  res.json([]);
});

// NDI genlock health (#150) — polled every 1 s by the dashboard's
// GlobalLockBadge (which fills store.ndi_health for the per-card LockBadges).
// Mirrors the real `GET /api/v1/ndi/health` array of PipelineHealthSnapshot:
// per output `lock_state` (#149) / `lock_reason`, `clock` (#146),
// `pacing` (#147), `audio` (#148). Mutable so a test can drive all three
// states via `POST /__mock/ndi-health`.
//
// Default fixture exercises the three badges at once:
//   - SP-worship   → LOCKED   (live, receiver present, clock ok)
//   - SP-background → DEGRADED (live, "no receiver")
//   - SP-live       → UNLOCKED (Idle → non-live, "pacing disabled")
// The global summary counts only LIVE outputs, so it resolves to
// `DEGRADED — SP-background` (the non-live UNLOCKED SP-live is ignored).
let ndiHealth = [
  {
    ndi_name: "SP-worship",
    playlist_id: 1,
    state: "Playing",
    // #201 round 2: the raw transport the API now exposes (default from state).
    transport: "Playing",
    connections: 2,
    lock_state: "LOCKED",
    lock_reason: "locked",
    clock: { is_locked: true, mode: "LOCK", offset_ns: 1200, clock_ok: true },
    pacing: {
      enabled: true,
      late_frames: 0,
      jitter_p99_us: 40,
      repeats: 0,
      resyncs: 0,
      lag_slots: 0,
    },
    audio: { residual_ppm: 1.2, underruns: 0 },
  },
  {
    ndi_name: "SP-background",
    playlist_id: 2,
    state: "Playing",
    transport: "Playing",
    connections: 0,
    lock_state: "DEGRADED",
    lock_reason: "no receiver",
    clock: { is_locked: true, mode: "LOCK", offset_ns: 950, clock_ok: true },
    pacing: {
      enabled: true,
      late_frames: 0,
      jitter_p99_us: 55,
      repeats: 0,
      resyncs: 0,
      lag_slots: 0,
    },
    audio: { residual_ppm: -0.4, underruns: 0 },
  },
  {
    ndi_name: "SP-live",
    playlist_id: 184,
    state: "Idle",
    transport: "Idle",
    connections: 0,
    lock_state: "UNLOCKED",
    lock_reason: "pacing disabled",
    clock: { is_locked: false, mode: "", offset_ns: null, clock_ok: false },
    pacing: {
      enabled: false,
      late_frames: 0,
      jitter_p99_us: 0,
      repeats: 0,
      resyncs: 0,
      lag_slots: 0,
    },
    audio: { residual_ppm: 0, underruns: 0 },
  },
];

app.get("/api/v1/ndi/health", (_req, res) => {
  res.json(ndiHealth);
});

// Admin: replace the NDI health fixture with the posted JSON array.
// Test-only — used by the frontend spec to flip the global badge to LOCKED.
app.post("/__mock/ndi-health", (req, res) => {
  if (!Array.isArray(req.body)) {
    res.status(400).json({ error: "expected a JSON array" });
    return;
  }
  ndiHealth = req.body;
  res.json({ status: "set", count: ndiHealth.length });
});

// Lyrics pipeline queue
app.get('/api/v1/lyrics/queue', (_req, res) => {
  res.json({
    bucket0_count: 2,
    bucket1_count: 12,
    bucket2_count: 187,
    pipeline_version: 2,
    processing: null,
  });
});

// #152: per-song SK translation gender override, kept mutable so the PATCH
// handler below echoes the value back in the songs list.
const translationGenders = {};

// Lyrics songs list (supports ?playlist_id=N filter)
app.get('/api/v1/lyrics/songs', (req, res) => {
  res.json([
    {
      video_id: 1,
      youtube_id: 'abc',
      title: 'Song One',
      song: 'One',
      artist: 'Artist',
      source: 'ensemble:qwen3+autosub',
      pipeline_version: 2,
      quality_score: 0.82,
      has_lyrics: true,
      is_stale: false,
      manual_priority: false,
      lyrics_reference: true,
      translation_gender: translationGenders[1] ?? null,
    },
    {
      video_id: 2,
      youtube_id: 'def',
      title: 'Song Two',
      song: 'Two',
      artist: 'Artist',
      source: null,
      pipeline_version: 0,
      quality_score: null,
      has_lyrics: false,
      is_stale: false,
      manual_priority: false,
      lyrics_reference: false,
      translation_gender: translationGenders[2] ?? null,
    },
  ]);
});

// #142: ★ reference marker feedback + admin toggle (test-only mocks —
// just acknowledge the write, the dashboard's own optimistic state update
// is what the e2e spec asserts).
app.post('/api/v1/lyrics/songs/:id/reference-feedback', (_req, res) => {
  res.status(204).end();
});
app.post('/api/v1/lyrics/songs/:id/reference', (_req, res) => {
  res.status(204).end();
});
// #152: per-song translation gender toggle. Validates m/f/null, stores it so
// the songs list echoes it, and replies 204 (mirrors the real handler).
app.patch('/api/v1/lyrics/songs/:id/translation-gender', (req, res) => {
  const gender = req.body?.gender ?? null;
  if (gender !== null && gender !== 'm' && gender !== 'f') {
    res.status(400).end();
    return;
  }
  translationGenders[Number(req.params.id)] = gender;
  res.status(204).end();
});

// Lyrics song detail
app.get('/api/v1/lyrics/songs/:id', (req, res) => {
  res.json({
    list_item: {
      video_id: Number(req.params.id),
      youtube_id: 'abc',
      song: 'Song',
      artist: 'Artist',
      source: 'ensemble:qwen3+autosub',
      pipeline_version: 2,
      quality_score: 0.82,
      has_lyrics: true,
      is_stale: false,
      manual_priority: false,
      lyrics_reference: false,
    },
    lyrics_json: { version: 2, source: 'ensemble:qwen3+autosub', lines: [] },
    audit_json: {
      providers_run: ['qwen3', 'autosub'],
      quality_metrics: { avg_confidence: 0.82 },
    },
  });
});

// #194 r3c: the song's full LyricsTrack (the shared LyricsView scroll mode in
// the Lyrics details modal fetches this). A small two-line track is enough for
// the list + active-line highlight; a missing route would 404 and trip the
// zero-console check.
const lyricsTrack = {
  version: 22,
  source: 'gemini-3-5-transcribe',
  language_source: 'en',
  language_translation: 'sk',
  lines: [
    { start_ms: 0, end_ms: 2000, en: 'Line one', sk: 'Riadok jeden' },
    { start_ms: 2000, end_ms: 4000, en: 'Line two', sk: 'Riadok dva' },
  ],
};

// #198 item 3/9: drive the shared LyricsView's loading / error / empty states.
//   "track" (default) → 200 + the 2-line track
//   "empty"           → 204 (no lyrics; the real handler's no-lyrics reply)
//   "error"           → 500
//   "slow"            → 200 after a delay so the loading state is observable
// Test-only; reset to "track" in each spec's afterEach (global in-memory state).
let lyricsMode = 'track';
app.post('/__mock/lyrics-mode', (req, res) => {
  const mode = req.body?.mode;
  if (!['track', 'empty', 'error', 'slow'].includes(mode)) {
    res.status(400).json({ error: `unknown mode: ${mode}` });
    return;
  }
  lyricsMode = mode;
  res.json({ mode: lyricsMode });
});

app.get('/api/v1/videos/:id/lyrics', (_req, res) => {
  if (lyricsMode === 'empty') {
    res.status(204).end();
    return;
  }
  if (lyricsMode === 'error') {
    res.status(500).json({ error: 'mock: lyrics fetch failed' });
    return;
  }
  if (lyricsMode === 'slow') {
    setTimeout(() => res.json(lyricsTrack), 2000);
    return;
  }
  res.json(lyricsTrack);
});

// Mutable reprocess result so tests can drive the dashboard's banner
// path for #98 (blocked_by_asr_gap surfacing). Defaults to a no-block
// outcome so existing specs keep their expectations. Both the targeted
// reprocess and the all-stale sweep share the same shape now that #101
// landed — the all-stale handler computes the asr_gap count.
const reprocessResult = { queued: 1, blocked_by_asr_gap: 0 };
const reprocessAllStaleResult = { queued: 187, blocked_by_asr_gap: 0 };

app.post('/api/v1/lyrics/reprocess', (_req, res) =>
  res.json({ ...reprocessResult }),
);
app.post('/api/v1/lyrics/reprocess-all-stale', (_req, res) =>
  res.json({ ...reprocessAllStaleResult }),
);
app.post('/api/v1/lyrics/clear-manual-queue', (_req, res) => res.json({ queued: 2 }));

// Admin: set the next reprocess response shape.
// Test-only — used by Playwright specs to drive the asr_gap banner.
// `target` selects which endpoint the override applies to:
//   omitted / "reprocess" → /api/v1/lyrics/reprocess
//   "all-stale"           → /api/v1/lyrics/reprocess-all-stale
app.post('/__mock/reprocess-result', (req, res) => {
  const { queued, blocked_by_asr_gap, target } = req.body || {};
  const slot = target === 'all-stale' ? reprocessAllStaleResult : reprocessResult;
  if (typeof queued === 'number') slot.queued = queued;
  if (typeof blocked_by_asr_gap === 'number') {
    slot.blocked_by_asr_gap = blocked_by_asr_gap;
  }
  res.json({ ...slot });
});

// Admin: broadcast a LyricsUpdate over the WebSocket to every connected
// client. Test-only — the #163 fixed-height karaoke-panel spec uses it to
// drive the current-line block with text and then with no text, without a
// real playback engine. The body IS the `data` payload of the
// `ServerMsg::LyricsUpdate` variant (`{ playlist_id, line_en, line_sk,
// prev_line_en, next_line_en, active_word_index, word_count }`; every field
// but `playlist_id` is optional — omit to send `None`).
app.post("/__mock/lyrics-update", (req, res) => {
  const data = req.body || {};
  if (typeof data.playlist_id !== "number") {
    res.status(400).json({ error: "expected a numeric playlist_id" });
    return;
  }
  const msg = JSON.stringify({ type: "LyricsUpdate", data });
  let sent = 0;
  for (const ws of wsClients) {
    if (ws.readyState === ws.OPEN) {
      ws.send(msg);
      sent += 1;
    }
  }
  res.json({ status: "sent", clients: sent });
});

// #170: flip a playlist's live playback state on demand so the dashboard-
// selector spec can prove the selector row order + identity survive a state
// change (a non-first playlist starting to play must NOT reorder to the top).
// Body: { playlist_id: <number>, state?: "Playing" | "WaitingForScene" | "Idle" }.
app.post("/__mock/set-playing", (req, res) => {
  const data = req.body || {};
  if (typeof data.playlist_id !== "number") {
    res.status(400).json({ error: "expected a numeric playlist_id" });
    return;
  }
  const state = typeof data.state === "string" ? data.state : "Playing";
  // #201: carry transport too (default Playing when state is Playing, else
  // Paused, mirroring the tick-item derivation) so a toggle assertion after
  // this helper reads the honest label — the Player now reads transport.
  const transport =
    typeof data.transport === "string"
      ? data.transport
      : state === "Playing"
        ? "Playing"
        : "Paused";
  const msg = JSON.stringify({
    type: "PlaybackStateChanged",
    data: { playlist_id: data.playlist_id, state, mode: "Continuous", transport },
  });
  let sent = 0;
  for (const ws of wsClients) {
    if (ws.readyState === ws.OPEN) {
      ws.send(msg);
      sent += 1;
    }
  }
  res.json({ status: "sent", clients: sent });
});

// #194: broadcast a `NowPlaying` for an ARBITRARY playlist so a spec can make a
// dub video "play" on the Dabing playlist (id 500). The shared Player chooses
// the dub mixer adapter when the now-playing `video_id` for the playlist matches
// a row in `GET /api/v1/dabing` `videos[]` — so `player.spec.ts`/`mixer.spec.ts`
// dabing-add a ready dub row, then broadcast its `video_id` here to surface the
// mix faders inside the Dabing Player. Body IS the NowPlaying `data`
// payload: `{playlist_id, video_id, song?, artist?, position_ms?, duration_ms?}`.
app.post("/__mock/now-playing", (req, res) => {
  const data = req.body || {};
  if (typeof data.playlist_id !== "number") {
    res.status(400).json({ error: "expected a numeric playlist_id" });
    return;
  }
  const msg = JSON.stringify({
    type: "NowPlaying",
    data: {
      playlist_id: data.playlist_id,
      video_id: typeof data.video_id === "number" ? data.video_id : 0,
      song: typeof data.song === "string" ? data.song : "",
      artist: typeof data.artist === "string" ? data.artist : "",
      position_ms: typeof data.position_ms === "number" ? data.position_ms : 0,
      duration_ms: typeof data.duration_ms === "number" ? data.duration_ms : 0,
    },
  });
  let sent = 0;
  for (const ws of wsClients) {
    if (ws.readyState === ws.OPEN) {
      ws.send(msg);
      sent += 1;
    }
  }
  res.json({ status: "sent", clients: sent });
});

// #194 hotfix: now-playing TICK mode. When enabled, the mock advances
// `position_ms` for each configured item every 500 ms and broadcasts a
// `NowPlaying`, so a spec can prove a live position tick does NOT tear down the
// Player / preview or snatch a fader / seek out from under a drag (exactly what
// the suite never exercised, so the owner's box regression went uncaught). OFF
// by default — existing specs are unaffected — and a spec toggles it per test
// via `POST /__mock/tick`, turning it OFF again in afterEach.
let tickItems = [];
let tickEnabled = false;

function tickBroadcast(obj) {
  const msg = JSON.stringify(obj);
  for (const ws of wsClients) {
    if (ws.readyState === ws.OPEN) ws.send(msg);
  }
}

function tickNowPlaying(it) {
  tickBroadcast({
    type: "NowPlaying",
    data: {
      playlist_id: it.playlist_id,
      video_id: it.video_id,
      song: it.song,
      artist: it.artist,
      position_ms: it.position_ms,
      duration_ms: it.duration_ms,
    },
  });
}

// Body: { enabled: bool, items?: [{playlist_id, video_id?, song?, artist?,
// duration_ms?, position_ms?, step_ms?, state?}] }. Enabling with items also
// pushes the initial PlaybackStateChanged + first NowPlaying immediately so
// `is_decoding` / `has_content` flip without waiting for the first 500 ms tick.
app.post("/__mock/tick", (req, res) => {
  const b = req.body || {};
  tickEnabled = !!b.enabled;
  if (tickEnabled) {
    if (Array.isArray(b.items)) {
      tickItems = b.items.map((it) => ({
        playlist_id: Number(it.playlist_id),
        video_id: typeof it.video_id === "number" ? it.video_id : 0,
        song: typeof it.song === "string" ? it.song : "",
        artist: typeof it.artist === "string" ? it.artist : "",
        duration_ms:
          typeof it.duration_ms === "number" ? it.duration_ms : 200000,
        position_ms: typeof it.position_ms === "number" ? it.position_ms : 0,
        step_ms: typeof it.step_ms === "number" ? it.step_ms : 500,
        state: typeof it.state === "string" ? it.state : "Playing",
        // #201: the pipeline's own transport, INDEPENDENT of `state`'s
        // on/off-program folding. Defaults to Playing when the scene-aware
        // `state` is Playing, else Paused — so an off-program decoding item is
        // driven with {state:"WaitingForScene", transport:"Playing"}.
        transport:
          typeof it.transport === "string"
            ? it.transport
            : it.state === "Playing"
              ? "Playing"
              : "Paused",
      }));
    }
    for (const it of tickItems) {
      tickBroadcast({
        type: "PlaybackStateChanged",
        data: {
          playlist_id: it.playlist_id,
          state: it.state,
          mode: "Continuous",
          transport: it.transport,
        },
      });
      tickNowPlaying(it);
    }
  } else {
    tickItems = [];
  }
  res.json({ status: "ok", enabled: tickEnabled, items: tickItems.length });
});

// The 500 ms position-advance broadcaster (module-global, started once). It is
// a no-op unless a spec turned tick mode on.
setInterval(() => {
  if (!tickEnabled) return;
  for (const it of tickItems) {
    it.position_ms = Math.min(it.position_ms + it.step_ms, it.duration_ms);
    tickNowPlaying(it);
  }
}, 500);

// SPA fallback — serve index.html for unmatched routes
app.get("*", (_req, res) => {
  res.sendFile(join(distPath, "index.html"));
});

// --- HTTP + WebSocket server ---

const server = createServer(app);

// Two WebSocket endpoints share one HTTP server. `noServer` + a single manual
// upgrade router dispatches by path — a path-scoped `{ server, path }` wss would
// instead `abortHandshake(400)` every non-matching upgrade (killing the preview
// socket before the second server could handle it). #178.
const wss = new WebSocketServer({ noServer: true });
const previewWss = new WebSocketServer({ noServer: true });
const PREVIEW_WS_RE = /^\/api\/v1\/playback\/(\d+)\/preview\.ws$/;

server.on("upgrade", (req, socket, head) => {
  let pathname;
  try {
    pathname = new URL(req.url, "http://localhost").pathname;
  } catch {
    socket.destroy();
    return;
  }
  if (pathname === "/api/v1/ws") {
    wss.handleUpgrade(req, socket, head, (ws) => wss.emit("connection", ws, req));
  } else if (PREVIEW_WS_RE.test(pathname)) {
    previewWss.handleUpgrade(req, socket, head, (ws) =>
      previewWss.emit("connection", ws, req),
    );
  } else {
    socket.destroy();
  }
});

// #178: stream the canned fMP4 fixture — init segment first, then each fragment
// with a small gap — so the card's MSE <video> reaches readyState>=3 and its
// currentTime advances.
previewWss.on("connection", (ws, req) => {
  const [init, ...frags] = PREVIEW_FMP4;
  // #184 round F: an optional `?lag_ms=<N>` knob on the upgrade url inflates the
  // beacon so the lag-readout E2E can force the "picture behind the wall" state.
  // Mock-only — the production dashboard never adds the flag to preview.ws.
  let lagMs = 0;
  try {
    const q = new URL(req.url, "http://localhost").searchParams.get("lag_ms");
    if (q !== null && /^\d+$/.test(q)) lagMs = Number(q);
  } catch {
    // malformed upgrade url — no knob
  }
  try {
    ws.send(init);
  } catch {
    return;
  }
  let i = 0;
  let fragsSent = 0;
  // #184 round F: the SAME 1 Hz lag beacon the real server sends — produced
  // media time = (fragments sent) × 500 ms, plus the mock lag knob. Send one
  // immediately so the readout appears without waiting a full second.
  const sendBeacon = () => {
    if (ws.readyState !== ws.OPEN) return;
    try {
      ws.send(JSON.stringify({ produced_ms: fragsSent * 500 + lagMs }));
    } catch {
      // client vanished mid-send — ignore.
    }
  };
  sendBeacon();
  const timer = setInterval(() => {
    if (ws.readyState !== ws.OPEN || i >= frags.length) {
      clearInterval(timer);
      return;
    }
    ws.send(frags[i++]);
    fragsSent++;
  }, 120);
  const beacon = setInterval(sendBeacon, 1000);
  const stop = () => {
    clearInterval(timer);
    clearInterval(beacon);
  };
  ws.on("close", stop);
  ws.on("error", stop);
});

wss.on("connection", (ws) => {
  console.log("[mock-api] WebSocket client connected");
  wsClients.add(ws);

  // #194 r3b: seed the shared HealthBar's OBS + tools segments so the strip
  // shows real values in the E2E (the real server pushes these over WS).
  try {
    ws.send(
      JSON.stringify({
        type: "ObsStatus",
        data: { connected: true, active_scene: "sp-alex" },
      }),
    );
    ws.send(
      JSON.stringify({
        type: "ToolsStatus",
        data: {
          ytdlp_available: true,
          ffmpeg_available: true,
          ytdlp_version: "2026.09.01",
          js_runtime_ok: true,
          deno_version: "2.0.0",
        },
      }),
    );
  } catch {
    // client vanished before the seed — ignore.
  }

  // #15 part 2: mark playlist 1 as Playing so its card renders the live
  // video preview <img> (playlist 1's preview.jpg serves a real JPEG above).
  // `state`/`mode` are the serde-derived variant names (`ServerMsg` uses the
  // derive, not the lowercase REST strings).
  const playingTimer = setTimeout(() => {
    if (ws.readyState === ws.OPEN) {
      ws.send(
        JSON.stringify({
          type: "PlaybackStateChanged",
          // #201: playlist 1 is on-program Playing → transport Playing so the
          // shared Player toggle reads `⏸ Pauza` on the Dashboard (the label
          // now follows transport, not the scene-aware `state`).
          data: {
            playlist_id: 1,
            state: "Playing",
            mode: "Continuous",
            transport: "Playing",
          },
        }),
      );
      // Playlist 2 gets now-playing info but stays Idle (no PlaybackStateChanged)
      // so its card renders the idle preview placeholder, not the <img>.
      ws.send(
        JSON.stringify({
          type: "NowPlaying",
          data: {
            playlist_id: 2,
            video_id: 2,
            song: "Idle Song",
            artist: "Idle Artist",
            position_ms: 0,
            duration_ms: 100000,
          },
        }),
      );
    }
  }, 100);

  // #201 round 2: the on-connect replay. The real server rebuilds a
  // `PlaybackStateChanged` per pipeline snapshot on connect, now carrying the
  // snapshot's RAW `transport`. Model it for the tick-driven off-program dub:
  // if a spec has enabled the tick, replay each item's state + transport on
  // (re)connect — with NO subsequent live message — so a page reload while an
  // off-program dub decodes reads its honest transport at once.
  const replayTimer = setTimeout(() => {
    if (ws.readyState === ws.OPEN && tickEnabled) {
      for (const it of tickItems) {
        ws.send(
          JSON.stringify({
            type: "PlaybackStateChanged",
            data: {
              playlist_id: it.playlist_id,
              state: it.state,
              mode: "Continuous",
              transport: it.transport,
            },
          }),
        );
      }
    }
  }, 120);

  // #154: one-shot LyricsQueueUpdate carrying the "waiting — wall in use"
  // worker state (a song-less processing entry). Delayed so it lands after the
  // card's initial HTTP fetch; buckets match /api/v1/lyrics/queue so the
  // bucket-count test is unaffected. Drives the idle-gate badge render.
  const badgeTimer = setTimeout(() => {
    if (ws.readyState === ws.OPEN) {
      ws.send(
        JSON.stringify({
          type: "LyricsQueueUpdate",
          data: {
            bucket0_count: 2,
            bucket1_count: 12,
            bucket2_count: 187,
            pipeline_version: 2,
            processing: {
              video_id: 0,
              youtube_id: "",
              song: "",
              artist: "",
              stage: "waiting — wall in use (SP-fast Playing)",
              provider: null,
              started_at_unix_ms: 0,
            },
          },
        }),
      );
    }
  }, 500);

  // Send a NowPlaying event periodically
  const interval = setInterval(() => {
    const msg = {
      type: "NowPlaying",
      data: {
        playlist_id: 1,
        video_id: 1,
        song: "Never Gonna Give You Up",
        artist: "Rick Astley",
        position_ms: Math.floor(Math.random() * 213000),
        duration_ms: 213000,
      },
    };
    if (ws.readyState === ws.OPEN) {
      ws.send(JSON.stringify(msg));
    }
  }, 2000);

  ws.on("message", (data) => {
    try {
      const msg = JSON.parse(data.toString());
      console.log("[mock-api] Received:", msg);
      if (msg.type === "Ping") {
        ws.send(JSON.stringify({ type: "Pong" }));
      }
    } catch {
      // ignore non-JSON messages
    }
  });

  ws.on("close", () => {
    clearInterval(interval);
    clearTimeout(badgeTimer);
    clearTimeout(playingTimer);
    clearTimeout(replayTimer);
    wsClients.delete(ws);
    console.log("[mock-api] WebSocket client disconnected");
  });
});

const PORT = 8920;
server.listen(PORT, "127.0.0.1", () => {
  console.log(`[mock-api] Mock API server running on http://127.0.0.1:${PORT}`);
  console.log(`[mock-api] Serving dist from: ${distPath}`);
});
