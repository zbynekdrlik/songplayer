import express from "express";
import { WebSocketServer } from "ws";
import { createServer } from "http";
import { fileURLToPath } from "url";
import { dirname, join } from "path";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const app = express();
app.use(express.json());

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

const videos = [
  {
    id: 1,
    playlist_id: 1,
    youtube_id: "dQw4w9WgXcQ",
    title: "Never Gonna Give You Up",
    artist: "Rick Astley",
    duration_ms: 213000,
    cached: true,
  },
  {
    id: 2,
    playlist_id: 1,
    youtube_id: "abc123",
    title: "Amazing Grace",
    artist: "Traditional",
    duration_ms: 180000,
    cached: false,
  },
];

const settings = {
  obs_websocket_url: "ws://127.0.0.1:4455",
  obs_websocket_password: "",
  gemini_api_key: "",
  gemini_model: "gemini-2.5-flash",
  cache_dir: "./cache",
};

const resolumeHosts = [];
let nextResolumeId = 1;

// --- REST API ---

// Playlists
app.get("/api/v1/playlists", (_req, res) => {
  res.json(playlists);
});

app.post("/api/v1/playlists", (req, res) => {
  const pl = { id: playlists.length + 1, ...req.body };
  playlists.push(pl);
  res.status(201).json(pl);
});

app.get("/api/v1/playlists/:id", (req, res) => {
  const pl = playlists.find((p) => p.id === Number(req.params.id));
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

// Playlist sync
app.post("/api/v1/playlists/:id/sync", (_req, res) => {
  res.json({ status: "syncing" });
});

// Playback controls
app.post("/api/v1/playback/:id/play", (_req, res) => {
  res.json({ status: "playing" });
});

app.post("/api/v1/playback/:id/pause", (_req, res) => {
  res.json({ status: "paused" });
});

app.post("/api/v1/playback/:id/skip", (_req, res) => {
  res.json({ status: "skipped" });
});

app.put("/api/v1/playback/:id/mode", (_req, res) => {
  res.json({ status: "mode_changed" });
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

// Status
app.get("/api/v1/status", (_req, res) => {
  res.json({
    obs_connected: false,
    active_scene: null,
    ytdlp_available: true,
    ffmpeg_available: true,
    playlists_count: playlists.length,
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
    },
  ]);
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
    },
    lyrics_json: { version: 2, source: 'ensemble:qwen3+autosub', lines: [] },
    audit_json: {
      providers_run: ['qwen3', 'autosub'],
      quality_metrics: { avg_confidence: 0.82 },
    },
  });
});

app.post('/api/v1/lyrics/reprocess', (_req, res) => res.json({ queued: 1 }));
app.post('/api/v1/lyrics/reprocess-all-stale', (_req, res) => res.json({ queued: 187 }));
app.post('/api/v1/lyrics/clear-manual-queue', (_req, res) => res.json({ queued: 2 }));

// SPA fallback — serve index.html for unmatched routes
app.get("*", (_req, res) => {
  res.sendFile(join(distPath, "index.html"));
});

// --- HTTP + WebSocket server ---

const server = createServer(app);

const wss = new WebSocketServer({ server, path: "/api/v1/ws" });

wss.on("connection", (ws) => {
  console.log("[mock-api] WebSocket client connected");

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
    console.log("[mock-api] WebSocket client disconnected");
  });
});

const PORT = 8920;
server.listen(PORT, "127.0.0.1", () => {
  console.log(`[mock-api] Mock API server running on http://127.0.0.1:${PORT}`);
  console.log(`[mock-api] Serving dist from: ${distPath}`);
});
