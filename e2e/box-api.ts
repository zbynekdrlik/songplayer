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
  frames_submitted_last_5s: number;
  /** Program-reconciled label: "Playing" only when the wall shows this
   *  output (the Player badge's source, sp-ui `player.rs`). */
  state: string;
  /** The pipeline's own decoding state (#201): "Playing" while it decodes,
   *  on or off program (the Player toggle's source). */
  transport?: string;
}

/** Every playlist pipeline's health row (`/api/v1/ndi/health`). */
export async function healthRows(request: APIRequestContext): Promise<HealthRow[]> {
  const resp = await request.get("/api/v1/ndi/health");
  expect(resp.status(), "GET /api/v1/ndi/health").toBe(200);
  return (await resp.json()) as HealthRow[];
}

/** The health row of playlist `pid`, or `undefined` when it has none yet. */
export async function healthRow(
  request: APIRequestContext,
  pid: number,
): Promise<HealthRow | undefined> {
  return (await healthRows(request)).find((r) => r.playlist_id === pid);
}

/**
 * A ready dub on the Dabing playlist (`pickReadyDub`). Fails loudly when no dub is ready — that is the Dabing
 * specs' precondition.
 */
export async function readyDub(
  request: APIRequestContext,
  preferVideoId?: number,
): Promise<{ pid: number; videoId: number }> {
  const dab = await request.get("/api/v1/dabing");
  expect(dab.status()).toBe(200);
  const body = (await dab.json()) as DabingList;
  const videoId = pickReadyDub(body, preferVideoId);
  expect(videoId, "at least one dub must be ready on the Dabing playlist").toBeGreaterThan(0);
  return { pid: body.playlist_id, videoId: videoId as number };
}

/** `GET /api/v1/dabing` (the fields the specs read). */
export interface DabingList {
  playlist_id: number;
  videos: Array<{ video_id?: number; id?: number; dub_status: string; playlist_id: number }>;
}

/**
 * The ready dub to play, on the Dabing playlist only: `/api/v1/dabing` also lists
 * a dub asked for on another playlist's song, and ▶ on that row plays it THERE, so
 * the Dabing output would never decode. `preferVideoId` when it is ready here, else
 * the first ready one.
 */
export function pickReadyDub(body: DabingList, preferVideoId?: number): number | undefined {
  const ready = body.videos.filter(
    (v) => v.dub_status === "ready" && v.playlist_id === body.playlist_id,
  );
  // DubRow carries the id as `video_id` (the row is keyed by the video).
  const ids = ready.map((v) => Number(v.video_id ?? v.id));
  return preferVideoId !== undefined && ids.includes(preferVideoId) ? preferVideoId : ids[0];
}
