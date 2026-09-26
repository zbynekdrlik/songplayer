/**
 * Shared read-only box lookups for the post-deploy specs (#184 review).
 *
 * Every post-deploy spec that drives the Dabing output needed the same two
 * reads: a READY dub (`/api/v1/dabing`), and one playlist's row of the NDI
 * health registry (`/api/v1/ndi/health`). The specs had grown five inline
 * copies of them. They live here once, with the fields the specs actually read.
 */

import { expect, APIRequestContext } from "@playwright/test";

/** One `/api/v1/ndi/health` row (the fields the post-deploy specs read). */
export interface HealthRow {
  playlist_id: number;
  ndi_name: string;
  connections: number;
  frames_submitted_last_5s: number;
  /** Program-reconciled label: "Playing" only when the wall shows this
   *  output (the Player badge's source, sp-ui `player.rs`). */
  state: string;
  /** The pipeline's own decoding state (#201): "Playing" while it decodes,
   *  on or off program (the Player toggle's source). */
  transport?: string;
}

/** The health row of playlist `pid`, or `undefined` when it has none yet. */
export async function healthRow(
  request: APIRequestContext,
  pid: number,
): Promise<HealthRow | undefined> {
  const resp = await request.get("/api/v1/ndi/health");
  expect(resp.status(), "GET /api/v1/ndi/health").toBe(200);
  const rows = (await resp.json()) as HealthRow[];
  return rows.find((r) => r.playlist_id === pid);
}

/**
 * A ready dub on the box: `preferVideoId` when that dub is ready, else the
 * first ready one. Fails loudly when no dub is ready — that is the Dabing
 * specs' precondition.
 */
export async function readyDub(
  request: APIRequestContext,
  preferVideoId?: number,
): Promise<{ pid: number; videoId: number }> {
  const dab = await request.get("/api/v1/dabing");
  expect(dab.status()).toBe(200);
  const body = (await dab.json()) as {
    playlist_id: number;
    videos: Array<{ video_id?: number; id?: number; dub_status: string }>;
  };
  const ready = body.videos.filter((v) => v.dub_status === "ready");
  expect(ready.length, "at least one dub must be ready on the box").toBeGreaterThan(0);
  // DubRow carries the id as `video_id` (the row is keyed by the video).
  const ids = ready.map((v) => Number(v.video_id ?? v.id));
  const videoId =
    preferVideoId !== undefined && ids.includes(preferVideoId) ? preferVideoId : ids[0];
  expect(videoId, "the ready dub must carry a numeric video id").toBeGreaterThan(0);
  return { pid: body.playlist_id, videoId };
}
