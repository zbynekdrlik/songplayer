/**
 * Unit tests for the cache-layout rule `post-deploy-flac.spec.ts` applies on
 * the box (#136). Runs in the ubuntu mock suite (playwright.config.ts): no
 * browser and no deployed box; these `test()` blocks never touch `page`.
 *
 * The rule must agree with the startup self-heal: it deletes a lone half,
 * but keeps the two halves of a song split across two names (a row records
 * them, `startup.rs`), and the check must accept exactly that shape.
 */

import { test, expect } from "@playwright/test";
import { checkCacheLayout } from "./cache-layout";

const ID = "IYAOosrh7HY";
const OTHER = "gq-4FVRr_ow";

test.describe("cache layout check (#136)", () => {
  test("a complete pair (plain or _gf) is the normal layout", () => {
    const names = [
      `Gods Not Dead_Enjoy Worship_${ID}_normalized_video.mp4`,
      `Gods Not Dead_Enjoy Worship_${ID}_normalized_audio.flac`,
      `Old_Artist_${OTHER}_normalized_gf_video.mp4`,
      `Old_Artist_${OTHER}_normalized_gf_audio.flac`,
      `${ID}_lyrics.json`,
    ];
    expect(checkCacheLayout(names, [ID, OTHER])).toEqual({
      missing: [],
      half: [],
      split: [],
      legacy: [],
    });
  });

  test("a song split across two names is accepted, as the self-heal keeps it", () => {
    // The repair moved the audio to the new name; the video move failed and
    // could not be rolled back. The row records both halves.
    const names = [
      `Old Song_Old Artist_${ID}_normalized_gf_video.mp4`,
      `Gods Not Dead_Enjoy Worship_${ID}_normalized_audio.flac`,
    ];
    expect(checkCacheLayout(names, [ID])).toEqual({
      missing: [],
      half: [],
      split: [ID],
      legacy: [],
    });
  });

  test("a lone half with no counterpart is a failure, and its song is missing", () => {
    const lone = `Song_Artist_${ID}_normalized_audio.flac`;
    expect(checkCacheLayout([lone], [ID])).toEqual({
      missing: [ID],
      half: [lone],
      split: [],
      legacy: [],
    });
  });

  test("a lone half next to a complete pair of the same song is a failure", () => {
    const lone = `Old Song_Old Artist_${ID}_normalized_gf_video.mp4`;
    const names = [
      `Gods Not Dead_Enjoy Worship_${ID}_normalized_video.mp4`,
      `Gods Not Dead_Enjoy Worship_${ID}_normalized_audio.flac`,
      lone,
    ];
    expect(checkCacheLayout(names, [ID])).toEqual({
      missing: [],
      half: [lone],
      split: [],
      legacy: [],
    });
  });

  test("three lone halves of one song are not a split song", () => {
    const names = [
      `A_B_${ID}_normalized_video.mp4`,
      `C_D_${ID}_normalized_gf_video.mp4`,
      `E_F_${ID}_normalized_audio.flac`,
    ];
    const layout = checkCacheLayout(names, [ID]);
    expect(layout.split).toEqual([]);
    expect(layout.half).toEqual([...names].sort());
    expect(layout.missing).toEqual([ID]);
  });

  test("two lone halves of DIFFERENT songs are two failures", () => {
    const video = `A_B_${ID}_normalized_video.mp4`;
    const audio = `C_D_${OTHER}_normalized_audio.flac`;
    expect(checkCacheLayout([video, audio], [ID, OTHER])).toEqual({
      missing: [OTHER, ID].sort(),
      half: [audio, video].sort(),
      split: [],
      legacy: [],
    });
  });

  test("a normalized song with no file at all is missing; a legacy single file is reported", () => {
    const legacy = `Song_Artist_${OTHER}_normalized.mp4`;
    expect(checkCacheLayout([legacy], [ID])).toEqual({
      missing: [ID],
      half: [],
      split: [],
      legacy: [legacy],
    });
  });
});
