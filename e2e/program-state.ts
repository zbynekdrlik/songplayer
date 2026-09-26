/**
 * What is on the OBS program right now, as SongPlayer reports it (#184).
 *
 * The post-deploy suite runs against the live box and NEVER switches OBS
 * scenes (CLAUDE.md). After an event the operator leaves the program on
 * whatever sp-* scene the event ended with — a regular playlist scene
 * (sp-slow, sp-fast, …) or `sp-dabing`. A spec that hard-codes one of those
 * states fails for a non-product reason (#184: the run after the 25.9 event
 * found `sp-dabing` on program). So a spec reads the real state here and
 * asserts the behaviour that matches it.
 *
 * The decision (`classifyProgram`) is pure so it is unit-tested in the mock
 * suite (`program-state.spec.ts`) without the box; `readProgramState` is the
 * thin I/O wrapper over `/api/v1/status` + `/api/v1/playlists`.
 */

import { expect, APIRequestContext } from "@playwright/test";

/** The Dabing playlist's `kind` (startup_dabing.rs seeds exactly one). */
export const DABING_KIND = "dabing";

/** The subset of a `/api/v1/playlists` row this module reads. */
export interface PlaylistRow {
  id: number;
  name: string;
  ndi_output_name: string;
  kind: string;
}

/** The on-program state of the box. */
export interface ProgramState {
  /** OBS program scene name (`/api/v1/status.active_scene`). */
  activeScene: string | null;
  /** Playlist ids whose NDI output the program scene shows. */
  activePlaylistIds: number[];
  /** True when the Dabing playlist (`kind == "dabing"`) is on program. */
  dabingOnProgram: boolean;
  /** On-program playlists that are NOT the Dabing one (regular dashboard
   *  playlists), in `activePlaylistIds` order. */
  regularOnProgram: PlaylistRow[];
  /** Every on-program playlist row (regular + Dabing), same order. */
  onProgram: PlaylistRow[];
}

/** Is this playlist the Dabing one? */
export function isDabing(p: PlaylistRow): boolean {
  return p.kind === DABING_KIND;
}

/**
 * Classify the on-program playlists. An active id with no playlist row is an
 * inconsistent box state and throws — a spec must fail loudly on it rather
 * than guess which branch applies.
 */
export function classifyProgram(
  activeScene: string | null,
  activePlaylistIds: number[],
  playlists: PlaylistRow[],
): ProgramState {
  const byId = new Map<number, PlaylistRow>();
  for (const p of playlists) byId.set(p.id, p);
  const onProgram = activePlaylistIds.map((id) => {
    const row = byId.get(id);
    if (!row) {
      throw new Error(
        `playlist ${id} is on program (scene ${activeScene}) but absent from /api/v1/playlists`,
      );
    }
    return row;
  });
  return {
    activeScene,
    activePlaylistIds: [...activePlaylistIds],
    dabingOnProgram: onProgram.some(isDabing),
    regularOnProgram: onProgram.filter((p) => !isDabing(p)),
    onProgram,
  };
}

/** Read the live program state from the deployed SongPlayer. */
export async function readProgramState(request: APIRequestContext): Promise<ProgramState> {
  const statusResp = await request.get("/api/v1/status");
  expect(statusResp.status(), "GET /api/v1/status").toBe(200);
  const status = (await statusResp.json()) as {
    active_scene?: string | null;
    active_playlist_ids?: number[];
  };
  const plResp = await request.get("/api/v1/playlists");
  expect(plResp.status(), "GET /api/v1/playlists").toBe(200);
  const playlists = (await plResp.json()) as PlaylistRow[];
  return classifyProgram(
    status.active_scene ?? null,
    status.active_playlist_ids ?? [],
    playlists,
  );
}

/** One line for the test log: which scene, which playlists, which branch. */
export function describeProgram(s: ProgramState): string {
  const names = s.onProgram.map((p) => `${p.id}:${p.name}(${p.kind})`).join(", ");
  return `program scene=${s.activeScene ?? "none"} on-program=[${names}] dabing=${
    s.dabingOnProgram ? "ON" : "OFF"
  } regular=${s.regularOnProgram.length}`;
}
