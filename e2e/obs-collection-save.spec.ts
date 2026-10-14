/**
 * Unit tests for the A/V gate's persist of cg OBS's restored state (#271):
 * what the saved collection says, the verdict on it, finding the collection
 * file, and the create + remove of a scene that makes OBS save. Runs in the
 * ubuntu mock suite (playwright.config.ts); these `test()` blocks never touch
 * `page`, so they need no browser and no box. The fake cg OBS stands in for
 * the external OBS process: it writes the collection file the way OBS's
 * deferred `SaveProject()` does, after the RemoveScene.
 */

import { test, expect } from "@playwright/test";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { AV_PROBE_INPUT, AV_PROBE_SCENE } from "./av-sync-probe";
import {
  SAVE_SCENE,
  PERSIST_TIMEOUT_MS,
  persistCollection,
  readCollection,
  savedState,
  savedVerdict,
  type SceneRecorder,
} from "./obs-collection-save";

/** The shape of SNV's `cg_scenes.json` as OBS saved it mid-take (14.10.2026, 06:28:40Z). */
const collectionDoc = (program: string, probeSource: string) => ({
  name: "cg_scenes",
  current_program_scene: program,
  current_scene: "fotky loop",
  current_transition: "Cut",
  sources: [
    { name: "fotky loop", id: "scene", settings: { items: [] } },
    { name: AV_PROBE_SCENE, id: "scene", settings: { items: [] } },
    { name: AV_PROBE_INPUT, id: "ndi_source", settings: { ndi_source_name: probeSource } },
  ],
});

const tempDir = () => fs.mkdtempSync(path.join(os.tmpdir(), "obs-collection-save-"));

function writeDoc(dir: string, file: string, doc: unknown, mtimeS: number) {
  const p = path.join(dir, file);
  fs.writeFileSync(p, JSON.stringify(doc));
  fs.utimesSync(p, mtimeS, mtimeS);
  return p;
}

/** cg OBS as the persist step sees it: it saves the collection after a RemoveScene. */
class FakeCg implements SceneRecorder {
  calls: string[] = [];
  scenes: string[];
  constructor(
    private dir: string,
    private onRemoveSaves: { program: string; probeSource: string } | null,
    scenes: string[] = ["fotky loop", AV_PROBE_SCENE],
  ) {
    this.scenes = [...scenes];
  }
  async listScenes() {
    this.calls.push("list");
    return [...this.scenes];
  }
  async createScene(sceneName: string) {
    this.calls.push(`create ${sceneName}`);
    this.scenes.push(sceneName);
  }
  async removeScene(sceneName: string) {
    this.calls.push(`remove ${sceneName}`);
    this.scenes = this.scenes.filter((s) => s !== sceneName);
    const save = this.onRemoveSaves;
    if (!save) return;
    // OBS's SaveProject is queued by Qt: the file changes a moment later.
    setTimeout(() => {
      writeDoc(this.dir, "cg_scenes.json", collectionDoc(save.program, save.probeSource), 2_000_000_000);
    }, 20);
  }
}

test.describe("cg OBS persists the A/V gate's restore (#271)", () => {
  test("the saved collection's name, program scene and probe source are read from OBS's file", () => {
    expect(savedState(collectionDoc(AV_PROBE_SCENE, "RESOLUME-SNV (SP-program)"), AV_PROBE_INPUT)).toEqual({
      collection: "cg_scenes",
      program: AV_PROBE_SCENE,
      probeSource: "RESOLUME-SNV (SP-program)",
    });
    // A collection without the probe input (the gate never provisioned it): no source.
    const { sources: _s, ...noSources } = collectionDoc("fotky loop", "");
    expect(savedState(noSources, AV_PROBE_INPUT)).toEqual({
      collection: "cg_scenes",
      program: "fotky loop",
      probeSource: null,
    });
  });

  test("anything that is not an OBS collection reads as nothing", () => {
    expect(savedState(null, AV_PROBE_INPUT)).toBeNull();
    expect(savedState([], AV_PROBE_INPUT)).toBeNull();
    expect(savedState({ current_program_scene: "fotky loop" }, AV_PROBE_INPUT)).toBeNull();
  });

  test("the restored state passes: cg's own scene on program and an idle probe", () => {
    const own = savedState(collectionDoc("fotky loop", ""), AV_PROBE_INPUT);
    expect(savedVerdict(own, { program: "fotky loop" })).toEqual({ ok: true });
    const noProbe = { collection: "cg_scenes", program: "fotky loop", probeSource: null };
    expect(savedVerdict(noProbe, { program: "fotky loop" })).toEqual({ ok: true });
  });

  test("the mid-take state fails, naming what OBS would load after a crash", () => {
    const midTake = savedState(collectionDoc(AV_PROBE_SCENE, "RESOLUME-SNV (SP-program)"), AV_PROBE_INPUT);
    const v = savedVerdict(midTake, { program: "fotky loop" });
    expect(v.ok).toBe(false);
    expect(!v.ok && v.why).toContain(`"${AV_PROBE_SCENE}"`);
    expect(!v.ok && v.why).toContain(`"fotky loop"`);
    // The right scene but the probe still pointed: a crash would bring a live receiver back.
    const pointed = savedState(collectionDoc("fotky loop", "RESOLUME-SNV (SP-program)"), AV_PROBE_INPUT);
    const p = savedVerdict(pointed, { program: "fotky loop" });
    expect(p.ok).toBe(false);
    expect(!p.ok && p.why).toContain("RESOLUME-SNV (SP-program)");
    expect(savedVerdict(null, { program: "fotky loop" }).ok).toBe(false);
  });

  test("the collection file is the one whose name is cg OBS's current collection", () => {
    const dir = tempDir();
    writeDoc(dir, "Meeting_scene.json", { ...collectionDoc("x", ""), name: "Meeting scene" }, 1_700_000_000);
    const want = writeDoc(dir, "cg_scenes.json", collectionDoc("fotky loop", ""), 1_800_000_000);
    fs.writeFileSync(path.join(dir, "broken.json"), "{ not json");
    fs.writeFileSync(path.join(dir, "cg_scenes.json.bak"), "{}");
    const found = readCollection(dir, "cg_scenes");
    expect(found?.path).toBe(want);
    expect(found?.mtimeMs).toBe(1_800_000_000_000);
    expect(readCollection(dir, "no such collection")).toBeNull();
    expect(readCollection(path.join(dir, "missing"), "cg_scenes")).toBeNull();
  });

  test("a scene create + remove makes cg OBS save the restored state, read back from the file", async () => {
    const dir = tempDir();
    writeDoc(dir, "cg_scenes.json", collectionDoc(AV_PROBE_SCENE, "RESOLUME-SNV (SP-program)"), 1_800_000_000);
    const cg = new FakeCg(dir, { program: "fotky loop", probeSource: "" });
    const saved = await persistCollection({
      rec: cg,
      dir,
      collection: "cg_scenes",
      program: "fotky loop",
      probeInput: AV_PROBE_INPUT,
      pollMs: 10,
    });
    expect(saved).toEqual({ collection: "cg_scenes", program: "fotky loop", probeSource: "" });
    expect(cg.calls).toEqual(["list", `create ${SAVE_SCENE}`, `remove ${SAVE_SCENE}`]);
    expect(cg.scenes).not.toContain(SAVE_SCENE);
  });

  test("a save scene left by a run that died between create and remove is removed first", async () => {
    const dir = tempDir();
    writeDoc(dir, "cg_scenes.json", collectionDoc(AV_PROBE_SCENE, ""), 1_800_000_000);
    const cg = new FakeCg(dir, { program: "fotky loop", probeSource: "" }, ["fotky loop", SAVE_SCENE]);
    await persistCollection({
      rec: cg,
      dir,
      collection: "cg_scenes",
      program: "fotky loop",
      probeInput: AV_PROBE_INPUT,
      pollMs: 10,
    });
    expect(cg.calls).toEqual([
      "list",
      `remove ${SAVE_SCENE}`,
      `create ${SAVE_SCENE}`,
      `remove ${SAVE_SCENE}`,
    ]);
    expect(cg.scenes).toEqual(["fotky loop"]);
  });

  test("an OBS that never rewrites the file fails the step, naming the old save", async () => {
    const dir = tempDir();
    writeDoc(dir, "cg_scenes.json", collectionDoc(AV_PROBE_SCENE, "RESOLUME-SNV (SP-program)"), 1_800_000_000);
    const cg = new FakeCg(dir, null);
    const run = persistCollection({
      rec: cg,
      dir,
      collection: "cg_scenes",
      program: "fotky loop",
      probeInput: AV_PROBE_INPUT,
      timeoutMs: 200,
      pollMs: 10,
    });
    await expect(run).rejects.toThrow(/did not save/);
    await expect(run).rejects.toThrow(new RegExp(`"${AV_PROBE_SCENE}"`.replace(/[()/]/g, "\\$&")));
  });

  test("an OBS that saves something other than the restore fails the step", async () => {
    const dir = tempDir();
    writeDoc(dir, "cg_scenes.json", collectionDoc(AV_PROBE_SCENE, ""), 1_800_000_000);
    const cg = new FakeCg(dir, { program: AV_PROBE_SCENE, probeSource: "" });
    const run = persistCollection({
      rec: cg,
      dir,
      collection: "cg_scenes",
      program: "fotky loop",
      probeInput: AV_PROBE_INPUT,
      timeoutMs: 300,
      pollMs: 10,
    });
    await expect(run).rejects.toThrow(/"fotky loop"/);
  });

  test("a missing collection file fails at once, before cg OBS is asked anything", async () => {
    const dir = tempDir();
    const cg = new FakeCg(dir, { program: "fotky loop", probeSource: "" });
    await expect(
      persistCollection({ rec: cg, dir, collection: "cg_scenes", program: "fotky loop", probeInput: AV_PROBE_INPUT }),
    ).rejects.toThrow(/no saved collection "cg_scenes"/);
    expect(cg.calls).toEqual([]);
  });

  test("the bound covers OBS's queued save with room to spare", () => {
    // Qt runs the deferred SaveProject on its next event-loop turn (camera-box:
    // "poll the file's mtime for ~2 s"); 5 s keeps a busy OBS inside the bound.
    expect(PERSIST_TIMEOUT_MS).toBe(5_000);
  });
});
