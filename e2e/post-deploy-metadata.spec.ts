/**
 * #136 post-deploy gate: every metadata provider of the production chain must
 * name a fixed real video.
 *
 * On 29.9.2026 ~110 videos played on the LED wall and the Presenter under
 * their raw YouTube title. Gemini got the whole comma-separated
 * `gemini_api_key` list as ONE key (Google refused every call), and the
 * reprocess worker had Gemini alone, so nothing repaired the rows while
 * Claude answered correctly. CI stayed green: nothing ever ran the real
 * providers, and the title-parser fallback looked plausible.
 *
 * This spec asks EACH provider of the chain the deployed build really uses
 * (`POST /api/v1/metadata/probe` — every provider on its own, nothing written
 * to the DB) to name one fixed real video. A broken key format, a retired
 * model or an unauthenticated proxy now fails the deploy. It calls the two
 * paid providers once per deploy (Claude through CLIProxyAPI, one grounded
 * Gemini search + its clean-up pass).
 *
 * API-level on purpose: the probe has no dashboard surface; the page-driven
 * specs cover the wall and the dashboard.
 */

import { test, expect } from "@playwright/test";

/** Playlist ytfast, video 258 of the #136 report — its YouTube title
 *  verbatim (YouTube oEmbed), the string the title parser shipped as song. */
const VIDEO = {
  youtube_id: "gq-4FVRr_ow",
  title: "Stand On Your Promise by The Emerging Sound (feat. Maddie Fong & Brenton Lawless)",
};
const WANT_SONG = "Stand On Your Promise";
const WANT_ARTIST_PART = "Emerging Sound";

/** The production chain, in order (`metadata::provider_chain`). */
const CHAIN = ["claude", "gemini"];

interface ProbeOutcome {
  name: string;
  ok: boolean;
  song: string | null;
  artist: string | null;
  error: string | null;
  elapsed_ms: number;
}

interface ProviderHealth {
  name: string;
  last_ok_at_ms: number | null;
  last_error: string | null;
}

test.describe("metadata provider chain (#136)", () => {
  test("every provider of the chain names the fixed real video", async ({ request }) => {
    // One grounded Gemini search + clean-up pass (≤ 90 s per request by the
    // provider's own bound) runs concurrently with Claude.
    test.setTimeout(240_000);

    const resp = await request.post("/api/v1/metadata/probe", {
      data: VIDEO,
      timeout: 220_000,
    });
    expect(resp.status(), "POST /api/v1/metadata/probe").toBe(200);
    const body = (await resp.json()) as { youtube_id: string; providers: ProbeOutcome[] };
    for (const p of body.providers) {
      console.log(
        `[#136 probe] ${p.name}: ok=${p.ok} song=${JSON.stringify(p.song)} ` +
          `artist=${JSON.stringify(p.artist)} error=${JSON.stringify(p.error)} ${p.elapsed_ms} ms`,
      );
    }

    expect(body.youtube_id).toBe(VIDEO.youtube_id);
    expect(
      body.providers.map((p) => p.name),
      "the deployed chain must be Claude, then Gemini",
    ).toEqual(CHAIN);
    for (const p of body.providers) {
      expect(p.error, `${p.name} must answer (its error)`).toBeNull();
      expect(p.ok, `${p.name} ok`).toBe(true);
      // Case-insensitive: a provider keeping another casing still names the
      // song; the raw title ("… by The Emerging Sound (feat. …)") never passes.
      expect((p.song ?? "").toLowerCase(), `${p.name} song`).toBe(WANT_SONG.toLowerCase());
      expect((p.artist ?? "").toLowerCase(), `${p.name} artist`).toContain(
        WANT_ARTIST_PART.toLowerCase(),
      );
    }

    // The status carries the same chain, and the repair-queue count is
    // readable (a number, not null). It may be > 0 right after a deploy while
    // the reprocess worker drains it.
    const status = await request.get("/api/v1/status");
    expect(status.status(), "GET /api/v1/status").toBe(200);
    const metadata = ((await status.json()) as {
      metadata: { failed_videos: number | null; providers: ProviderHealth[] };
    }).metadata;
    console.log(`[#136 status.metadata] ${JSON.stringify(metadata)}`);
    expect(metadata.providers.map((p) => p.name)).toEqual(CHAIN);
    expect(typeof metadata.failed_videos, "failed_videos must be a count").toBe("number");
    for (const p of metadata.providers) {
      expect(p.last_ok_at_ms ?? 0, `${p.name} answered the probe`).toBeGreaterThan(0);
    }
  });
});
