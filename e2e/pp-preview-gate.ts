/**
 * #266: which dashboard card PP's post-deploy preview gate opens
 * (`post-deploy-pp-preview.spec.ts`), pure so the mock suite tests it
 * (`pp-preview-gate.spec.ts`).
 *
 * The owner opened PP's dashboard, started the live preview of a PAUSED
 * playlist and got a black window, and PP had no preview check at all. The
 * gate opens the card of the playlist ON PROGRAM — what the wall shows, the
 * card the owner looks at — and needs it decoding: the dashboard offers a
 * preview only while the pipeline decodes (`sp_core::player_view::
 * preview_slot`). It never switches a scene and never presses ▶ (PP's wall
 * is live), so when nothing is on program, or the on-program playlist does
 * not decode, it fails naming why — it never skips.
 */

import type { HealthRow } from "./box-api";
import type { PlaylistRow, ProgramState } from "./program-state";

export type PreviewTarget = { target: PlaylistRow } | { error: string };

/** The card the gate opens: the on-program playlist, while it decodes. */
export function previewTarget(program: ProgramState, health: HealthRow[]): PreviewTarget {
  const target = program.onProgram[0];
  if (target === undefined) {
    return {
      error:
        `nothing is on SongPlayer's program (scene ${program.activeScene ?? "none"}): ` +
        `the preview gate opens the card of the playlist on program, and it never ` +
        `switches a scene by itself — put a playlist on program at PP`,
    };
  }
  const row = health.find((h) => h.playlist_id === target.id);
  if (row === undefined) {
    return {
      error:
        `the playlist on program, ${target.id} (${target.name}), has no row on ` +
        `/api/v1/ndi/health: its pipeline does not run`,
    };
  }
  if (row.transport !== "Playing") {
    return {
      error:
        `the playlist on program, ${target.id} (${target.name}), does not decode ` +
        `(transport ${row.transport ?? "none"}, state ${row.state}): the dashboard ` +
        `offers no live preview for a playlist that does not play (#266), and the ` +
        `gate never presses ▶ on PP's wall`,
    };
  }
  return { target };
}
