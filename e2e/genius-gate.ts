/**
 * The decision of the Genius live gate (#232, `post-deploy-genius.spec.ts`),
 * as pure functions over the box's answers so the mock suite tests them
 * (`genius-gate.spec.ts`).
 *
 * Genius is the lyrics worker's community-lyrics source (`gather.rs`, the
 * title search): its token (`genius_access_token`) had only a check that the
 * setting is masked, so a dead token or an unreachable API would have gone
 * unnoticed with CI green (the owner's rule, 29.9.2026: every external
 * provider gets a live post-deploy check).
 *
 * The gate takes up to [`GENIUS_ROWS`] catalog songs whose served lyrics
 * came from Genius (`source` `genius…`) and asks
 * `POST /api/v1/lyrics/probe-sources` about each: the lyrics worker's own
 * Genius fetch with the box's token. It passes on the first song Genius
 * answers with lyrics; a song that has since left Genius is only skipped.
 * A dead token or an unreachable Genius fails every one of them.
 */

/** How many Genius songs the gate asks at most: a song served from the
 * title search or with a shortened artist ("J. Traylor") is often no
 * artist-match hit for the worker's fetch (SNV 10.10.2026: 1 hit in the
 * first 3), so six keep a live token from failing the gate by chance. */
export const GENIUS_ROWS = 6;

/** One row of `GET /api/v1/lyrics/songs` (the fields the gate reads). */
export interface LyricsSongRow {
  video_id: number;
  youtube_id: string;
  song: string;
  source: string | null;
}

/** One provider's line of the probe (`lyrics::probe::ProviderProbe`). */
export interface ProviderProbe {
  provider: string;
  provider_url: string;
  available: boolean;
  line_count: number;
  note: string;
}

/** `POST /api/v1/lyrics/probe-sources`'s answer (`lyrics::probe::ProbeReport`). */
export interface ProbeReport {
  video_id: number;
  youtube_id: string;
  song: string;
  artist: string;
  probes: ProviderProbe[];
}

/** The songs to ask: those whose served lyrics came from Genius, lowest
 * row first, at most [`GENIUS_ROWS`]. */
export function geniusRows(songs: LyricsSongRow[]): LyricsSongRow[] {
  return songs
    .filter((s) => (s.source ?? "").startsWith("genius"))
    .sort((a, b) => a.video_id - b.video_id)
    .slice(0, GENIUS_ROWS);
}

/** Whether Genius answered this probe with lyrics. */
export function geniusHit(report: ProbeReport): boolean {
  const genius = report.probes.find((p) => p.provider === "genius");
  return genius !== undefined && genius.available && genius.line_count > 0;
}

/** Why the gate fails (empty = it passes): no Genius song to ask, or no
 * asked song answered from Genius — each with its probe's note. */
export function geniusGateFailures(asked: LyricsSongRow[], reports: ProbeReport[]): string[] {
  if (asked.length === 0) {
    return ["no catalog song's served lyrics came from Genius: nothing to ask"];
  }
  if (reports.some(geniusHit)) {
    return [];
  }
  return reports.map((r) => {
    const genius = r.probes.find((p) => p.provider === "genius");
    return `${r.youtube_id} "${r.song}": ${genius ? genius.note : "no genius line in the probe"}`;
  });
}
