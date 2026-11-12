import { test, expect } from "@playwright/test";
import { currentAreaProblem } from "./block-gate";

// #265: the post-deploy gate's decision on /blok's "Teraz na programe" area
// (block-gate.ts), unit-tested here so the rule runs in CI without a box.

const area = (state: string, buttonText: string | null, buttonDisabled: boolean) => ({
  state,
  buttonText,
  buttonDisabled,
});

test("the block gate judges the song on program's area", () => {
  // Never told the states.
  expect(currentAreaProblem(area("loading", null, false), null)).toContain("never learned");
  // Nothing on program: nothing to judge.
  expect(currentAreaProblem(area("nothing", null, false), null)).toBeNull();
  // A youtube playlist's song: enabled = its id was found.
  expect(currentAreaProblem(area("song", "Zablokovať v snv", false), "youtube")).toBeNull();
  expect(currentAreaProblem(area("song", "Zablokovať v snv", true), "youtube")).toContain(
    "not found",
  );
  // An unknown kind counts as youtube (the default kind).
  expect(currentAreaProblem(area("song", "Zablokovať v snv", true), null)).toContain("not found");
  // A set list may play a row no song list names: not judged.
  expect(currentAreaProblem(area("song", "Zablokovať v snv", true), "custom")).toBeNull();
  // Already blocked: disabled.
  expect(currentAreaProblem(area("song", "Už je zablokovaná", true), "youtube")).toBeNull();
  expect(currentAreaProblem(area("song", "Už je zablokovaná", false), "youtube")).toContain(
    "enabled",
  );
  // Anything else.
  expect(currentAreaProblem(area("song", null, false), "youtube")).toContain("no block button");
  expect(currentAreaProblem(area("song", "Block", false), "youtube")).toContain("unexpected");
  expect(currentAreaProblem(area("odd", null, false), null)).toContain("unknown area state");
});
