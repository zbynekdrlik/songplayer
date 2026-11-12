/**
 * #265: the read-only post-deploy gate's decision on `/blok`'s "Teraz na
 * programe" area (pure; unit-tested in `block-gate.spec.ts`, used by
 * `post-deploy-block-songs.spec.ts`).
 */

/** What the page's area shows once the gate stopped waiting. */
export interface CurrentArea {
  /** `block-current`'s `data-state`: `loading` | `nothing` | `song`. */
  state: string;
  /** `block-current-btn`'s text (`null`: no button). */
  buttonText: string | null;
  /** Whether the button is disabled. */
  buttonDisabled: boolean;
}

/**
 * Why the area is wrong, or `null`. `programKind` = the kind of the
 * playlist SongPlayer has on program (`null` when none or unknown).
 *
 * - still `loading`: the page never learned the playlists' states;
 * - a song with "Zablokovať v …": the button must be enabled — the song's
 *   YouTube id was found among the node's rows. Only checked for a youtube
 *   playlist on program: a custom set list may play a row that left its
 *   YouTube playlist (#260), which no playlist's song list names;
 * - a song with "Už je zablokovaná": disabled;
 * - `nothing`: no button to judge.
 */
export function currentAreaProblem(area: CurrentArea, programKind: string | null): string | null {
  if (area.state === "loading") return "the page never learned which song is on program";
  if (area.state === "nothing") return null;
  if (area.state !== "song") return `unknown area state "${area.state}"`;
  if (area.buttonText === null) return "a song on program shows no block button";
  if (area.buttonText === "Už je zablokovaná") {
    return area.buttonDisabled ? null : "a blocked song's button is enabled";
  }
  if (!area.buttonText.startsWith("Zablokovať v ")) {
    return `unexpected block button text "${area.buttonText}"`;
  }
  if (area.buttonDisabled && (programKind ?? "youtube") === "youtube") {
    return "the song on program was not found among the node's songs (its block button is disabled)";
  }
  return null;
}
