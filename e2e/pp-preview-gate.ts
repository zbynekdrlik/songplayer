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
 * is live). What is on program is the OPERATOR's state, never a deploy
 * fault (`post-deploy-program-state.md`): nothing on program, or an
 * on-program playlist that does not decode, is a branch the spec asserts
 * (no card to open / the card explains why it offers no preview), never a
 * failure and never a skip. Only a pipeline that does not run fails.
 */

import type { HealthRow } from "./box-api";
import type { PlaylistRow, ProgramState } from "./program-state";

export type PreviewTarget =
  /** On program and decoding: the live preview is checked. */
  | { target: PlaylistRow }
  /** On program, not decoding: its card must explain, offering no preview. */
  | { explained: PlaylistRow; why: string }
  /** Nothing on SongPlayer's program: no card to open. */
  | { none: string }
  /** The on-program playlist's pipeline does not run: a fault. */
  | { error: string };

/** The preview slot's lines for a playlist that does not decode
 * (`sp_core::player_view::{PREVIEW_PAUSED, PREVIEW_IDLE}`). */
export const PREVIEW_NOT_DECODING = [
  "Náhľad sa zobrazí, keď playlist hrá — tento je pozastavený.",
  "Náhľad sa zobrazí, keď playlist hrá — tento nehrá.",
];

/** The card the gate opens: the on-program playlist, while it decodes. */
export function previewTarget(program: ProgramState, health: HealthRow[]): PreviewTarget {
  const target = program.onProgram[0];
  if (target === undefined) {
    return {
      none:
        `nothing is on SongPlayer's program (scene ${program.activeScene ?? "none"}): ` +
        `no card to open; the gate never switches a scene by itself`,
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
      explained: target,
      why:
        `the playlist on program, ${target.id} (${target.name}), does not decode ` +
        `(transport ${row.transport ?? "none"}, state ${row.state}): the dashboard ` +
        `offers no live preview for it (#266), and the gate never presses ▶ on ` +
        `PP's wall`,
    };
  }
  return { target };
}
