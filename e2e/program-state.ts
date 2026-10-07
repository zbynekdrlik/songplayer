/**
 * What is on SongPlayer's program right now, as SongPlayer reports it (#184).
 *
 * A program-state spec runs against the live box and NEVER switches the
 * program to set itself up (other specs press scenes through the facade
 * and restore it). After an event the operator leaves the program on
 * whatever sp-* scene the event ended with — a regular playlist scene
 * (sp-slow, sp-fast, …) or `sp-dabing`. A spec that hard-codes one of those
 * states fails for a non-product reason (#184: the run after the 25.9 event
 * found `sp-dabing` on program). So a spec reads the real state here and
 * asserts the behaviour that matches it.
 *
 * The decision (`classifyProgram`) is pure so it is unit-tested in the mock
 * suite (`program-state.spec.ts`) without the box; `readProgramState` is the
 * thin I/O wrapper over `/api/v1/status` + `/api/v1/playlists`.
 *
 * The specs that DO press scenes (`post-deploy.spec.ts`, #229's
 * `post-deploy-pp.spec.ts`) wait for the engine to follow with
 * `readEngineActiveScene` / `waitEngineActiveScene`, below.
 */

import { expect, APIRequestContext } from "@playwright/test";

/** The Dabing playlist's `kind` (startup_dabing.rs seeds exactly one). */
export const DABING_KIND = "dabing";

/** The subset of a `/api/v1/playlists` row this module reads. */
export interface PlaylistRow {
  id: number;
  name: string;
  ndi_output_name: string;
  is_active: boolean;
  kind: string;
}

/** The on-program state of the box. */
export interface ProgramState {
  /** SongPlayer's own program scene name (`/api/v1/status.active_scene`,
   *  #221 L4b: the scene catalog's name for SP-program's source). */
  activeScene: string | null;
  /** SongPlayer's on-air set (`/api/v1/status.active_playlist_ids`):
   *  SP-program's playlist (#221 B4 step 6: none for "OBS manuál"). */
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

// #170: read the ENGINE's view of the on-program scene to prove SongPlayer
// actually followed a scene switch — not just that OBS reports it.
// #221 L4b: `active_scene` is SongPlayer's own program (the one resolver) and
// `active_playlist_ids` its on-air set: SP-program's playlist alone (#221 B4
// step 6: no cg OBS record joins it any more, so it never holds two). A read
// that names two playlists is reported as not settled (never a match).
export async function readEngineActiveScene(
  ctx: APIRequestContext,
): Promise<string | null> {
  try {
    const resp = await ctx.get("/api/v1/status");
    if (!resp.ok()) return null;
    const status = (await resp.json()) as {
      active_scene?: string | null;
      active_playlist_ids?: number[];
    };
    const onAir = status.active_playlist_ids ?? [];
    if (onAir.length > 1) {
      return `${status.active_scene} (not settled, on air ${JSON.stringify(onAir)})`;
    }
    return status.active_scene ?? null;
  } catch {
    return null;
  }
}

// Poll the engine's active scene until it equals `target` (or a short deadline).
export async function waitEngineActiveScene(
  ctx: APIRequestContext,
  target: string,
  timeoutMs = 5000,
): Promise<string | null> {
  const deadline = Date.now() + timeoutMs;
  let last: string | null = null;
  for (;;) {
    last = await readEngineActiveScene(ctx);
    if (last === target) return last;
    if (Date.now() >= deadline) return last;
    await new Promise((r) => setTimeout(r, 200));
  }
}

/** One line for the test log: which scene, which playlists, which branch. */
export function describeProgram(s: ProgramState): string {
  const names = s.onProgram.map((p) => `${p.id}:${p.name}(${p.kind})`).join(", ");
  return `program scene=${s.activeScene ?? "none"} on-program=[${names}] dabing=${
    s.dabingOnProgram ? "ON" : "OFF"
  } regular=${s.regularOnProgram.length}`;
}
