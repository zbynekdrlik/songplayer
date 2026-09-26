/**
 * Unit tests for the pure program-state classifier (#184).
 *
 * Runs in the ubuntu mock suite (playwright.config.ts): no browser, no box,
 * never touches the `page` fixture. It pins the branch decision the post-deploy
 * Dabing + preview specs rely on. The operator may leave ANY sp-* scene on
 * program after an event (`sp-dabing` on 25.9.2026), and each spec must pick
 * the branch that matches.
 */

import { test, expect } from "@playwright/test";
import { classifyProgram, describeProgram, isDabing, PlaylistRow } from "./program-state";

const PLAYLISTS: PlaylistRow[] = [
  { id: 4, name: "ytslow", ndi_output_name: "SP-slow", kind: "youtube" },
  { id: 7, name: "ytfast", ndi_output_name: "SP-fast", kind: "youtube" },
  { id: 184, name: "ytlive", ndi_output_name: "SP-live", kind: "custom" },
  { id: 648, name: "Dabing", ndi_output_name: "SP-dabing", kind: "dabing" },
];

test.describe("program-state classifier (#184)", () => {
  test("isDabing keys on kind == dabing only", () => {
    expect(isDabing(PLAYLISTS[3])).toBe(true);
    expect(isDabing(PLAYLISTS[0])).toBe(false);
    expect(isDabing(PLAYLISTS[2])).toBe(false);
  });

  test("sp-dabing alone on program: Dabing ON, no regular playlist (the 25.9 state)", () => {
    const s = classifyProgram("sp-dabing", [648], PLAYLISTS);
    expect(s.dabingOnProgram).toBe(true);
    expect(s.regularOnProgram).toEqual([]);
    expect(s.onProgram.map((p) => p.id)).toEqual([648]);
    expect(describeProgram(s)).toContain("dabing=ON");
  });

  test("a regular scene on program: Dabing OFF, that playlist is the regular one", () => {
    const s = classifyProgram("sp-slow", [4], PLAYLISTS);
    expect(s.dabingOnProgram).toBe(false);
    expect(s.regularOnProgram.map((p) => p.ndi_output_name)).toEqual(["SP-slow"]);
  });

  test("Dabing + a regular playlist both on program: both flags, regular keeps order", () => {
    const s = classifyProgram("mixed", [7, 648, 184], PLAYLISTS);
    expect(s.dabingOnProgram).toBe(true);
    expect(s.regularOnProgram.map((p) => p.id)).toEqual([7, 184]);
    expect(s.onProgram.map((p) => p.id)).toEqual([7, 648, 184]);
  });

  test("nothing on program: empty sets, Dabing OFF", () => {
    const s = classifyProgram(null, [], PLAYLISTS);
    expect(s.dabingOnProgram).toBe(false);
    expect(s.regularOnProgram).toEqual([]);
    expect(s.onProgram).toEqual([]);
    expect(describeProgram(s)).toContain("scene=none");
  });

  test("an on-program id with no playlist row throws (fail loudly, never guess)", () => {
    expect(() => classifyProgram("sp-ghost", [999], PLAYLISTS)).toThrow(/playlist 999/);
  });
});
