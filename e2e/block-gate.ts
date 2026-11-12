/**
 * #265: the read-only post-deploy gate's decision on `/blok`'s "Teraz na
 * programe" area (pure; unit-tested in `block-gate.spec.ts`, used by
 * `post-deploy-block-songs.spec.ts`).
 */

/** What the page's area shows at one read. */
export interface CurrentArea {
  /** `block-current`'s `data-state`: `loading` | `nothing` | `song`. */
  state: string;
  /** `block-current-btn`'s text (`null`: no button). */
  buttonText: string | null;
  /** Whether the button is disabled. */
  buttonDisabled: boolean;
}

/** What the server says about its program, read next to the page. */
export interface ProgramFacts {
  /** `/api/v1/status.active_playlist_ids`. */
  onAir: number[];
  /**
   * The on-air playlist's kind from `/api/v1/playlists` (a row with no kind
   * is `youtube`); `null` when it is not listed there (the #228 test item's
   * playlist is hidden from the list).
   */
  kind: string | null;
  /** Its health row's `transport` is `Playing`: a song decodes on program. */
  decoding: boolean;
}

/**
 * Why the area is wrong, or `null`:
 *
 * - still `loading`: the page never learned the playlists' states;
 * - judged further only with exactly one playlist on air (none, or a fade's
 *   two, is never a reason to fail);
 * - a song decoding on program must show (`song`), unless its playlist is
 *   not listed (the test item);
 * - a song with "Zablokovať v …": enabled — its YouTube id was found among
 *   the node's songs. Only for a youtube playlist: a custom set list or the
 *   Dabing playlist may play a row no playlist's song list names (#260);
 * - a song with "Už je zablokovaná": disabled.
 */
export function currentAreaProblem(area: CurrentArea, program: ProgramFacts): string | null {
  if (area.state === "loading") return "the page never learned which song is on program";
  if (!["nothing", "song"].includes(area.state)) return `unknown area state "${area.state}"`;
  if (program.onAir.length !== 1 || program.kind === null) return null;
  if (area.state === "nothing") {
    return program.decoding ? "a song plays on program but the page shows none" : null;
  }
  if (area.buttonText === null) return "a song on program shows no block button";
  if (area.buttonText === "Už je zablokovaná") {
    return area.buttonDisabled ? null : "a blocked song's button is enabled";
  }
  if (!area.buttonText.startsWith("Zablokovať v ")) {
    return `unexpected block button text "${area.buttonText}"`;
  }
  if (area.buttonDisabled && program.kind === "youtube") {
    return "the song on program was not found among the node's songs (its block button is disabled)";
  }
  return null;
}
