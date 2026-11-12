import { test, expect } from "@playwright/test";
import { currentAreaProblem } from "./block-gate";

// #265: the post-deploy gate's decision on /blok's "Teraz na programe" area
// (block-gate.ts), unit-tested here so the rule runs in CI without a box.

const area = (state: string, buttonText: string | null, buttonDisabled: boolean) => ({
  state,
  buttonText,
  buttonDisabled,
});
const on = (kind: string | null, decoding = true) => ({ onAir: [4], kind, decoding });

test("the block gate judges the song on program's area", () => {
  const enabled = area("song", "Zablokovať v snv", false);
  const disabled = area("song", "Zablokovať v snv", true);
  // Never told the states: always a failure.
  expect(currentAreaProblem(area("loading", null, false), on("youtube"))).toContain(
    "never learned",
  );
  expect(
    currentAreaProblem(area("loading", null, false), { onAir: [], kind: null, decoding: false }),
  ).toContain("never learned");
  // A youtube playlist's song: enabled = its id was found.
  expect(currentAreaProblem(enabled, on("youtube"))).toBeNull();
  expect(currentAreaProblem(disabled, on("youtube"))).toContain("not found");
  // A set list / the Dabing playlist may play a row no song list names.
  expect(currentAreaProblem(disabled, on("custom"))).toBeNull();
  expect(currentAreaProblem(disabled, on("dabing"))).toBeNull();
  // A song decoding on program must show; nothing decoding may read "nothing".
  expect(currentAreaProblem(area("nothing", null, false), on("youtube"))).toContain(
    "page shows none",
  );
  expect(currentAreaProblem(area("nothing", null, false), on("custom"))).toContain(
    "page shows none",
  );
  expect(currentAreaProblem(area("nothing", null, false), on("youtube", false))).toBeNull();
  // Not judged: nothing on air, a fade's two, an unlisted (test item) playlist.
  expect(
    currentAreaProblem(area("nothing", null, false), { onAir: [], kind: null, decoding: false }),
  ).toBeNull();
  expect(currentAreaProblem(disabled, { onAir: [4, 5], kind: "youtube", decoding: true })).toBeNull();
  expect(currentAreaProblem(disabled, on(null))).toBeNull();
  expect(currentAreaProblem(area("nothing", null, false), on(null))).toBeNull();
  // Already blocked: disabled.
  expect(currentAreaProblem(area("song", "Už je zablokovaná", true), on("youtube"))).toBeNull();
  expect(currentAreaProblem(area("song", "Už je zablokovaná", false), on("youtube"))).toContain(
    "enabled",
  );
  // Anything else.
  expect(currentAreaProblem(area("song", null, false), on("youtube"))).toContain(
    "no block button",
  );
  expect(currentAreaProblem(area("song", "Block", false), on("youtube"))).toContain("unexpected");
  expect(currentAreaProblem(area("odd", null, false), on("youtube"))).toContain(
    "unknown area state",
  );
});
