/**
 * The A/V gate makes cg OBS persist its restored state (#271).
 *
 * The gate puts cg OBS on its probe scene with the probe pointed at
 * `SP-program` for a take, then restores both (`post-deploy-av-sync.spec.ts`
 * teardown). OBS saved its scene collection IN the take (SNV: 13.10. 15:58:29Z,
 * 14.10. 06:28:40Z — program = the probe scene, the probe pointed) and never
 * after the restore: neither `SetCurrentProgramScene` nor `SetInputSettings`
 * reaches OBS's `SaveProject()`. So a crash of OBS or the PC before OBS's next
 * save brought cg OBS up on the probe scene (the hard reset of 14.10.2026).
 *
 * camera-box (cg OBS's owner) read its vendored frontend: `OBSBasic::AddScene`
 * and `OBSBasic::RemoveScene` call `SaveProject()` (`OBSBasic_Scenes.cpp`),
 * deferred by Qt. So after its restore the gate creates and removes
 * [`SAVE_SCENE`] — never on program or preview, gone before the save that
 * counts — and reads the collection file back until it says cg OBS's own
 * scene with an idle probe, bounded by [`PERSIST_TIMEOUT_MS`].
 *
 * The decisions here are unit-tested in the ubuntu mock suite
 * (`obs-collection-save.spec.ts`); the spec only calls [`persistCollection`].
 */

import * as fs from "fs";
import * as path from "path";

/** The scene the gate creates and removes to make cg OBS save its collection. */
export const SAVE_SCENE = "A/V gate save";
/** How long the gate waits for the collection file to read the restored state. */
export const PERSIST_TIMEOUT_MS = 5_000;

/** What cg OBS's saved collection says, read from its JSON file. */
export interface SavedState {
  /** The collection's own `name` (what `GetSceneCollectionList` calls it). */
  collection: string;
  /** `current_program_scene`: the scene OBS puts on program when it loads. */
  program: string | null;
  /** The probe input's saved `ndi_source_name`; null when the input is not saved. */
  probeSource: string | null;
}

export type SavedVerdict = { ok: true } | { ok: false; why: string };

export interface CollectionFile {
  path: string;
  mtimeMs: number;
  doc: unknown;
}

/** The three cg OBS requests the persist step makes. */
export interface SceneRecorder {
  listScenes(): Promise<string[]>;
  createScene(sceneName: string): Promise<void>;
  removeScene(sceneName: string): Promise<void>;
}

export interface PersistOptions {
  rec: SceneRecorder;
  /** OBS's scene collection directory (`%APPDATA%\obs-studio\basic\scenes`). */
  dir: string;
  /** cg OBS's current collection (`GetSceneCollectionList.currentSceneCollectionName`). */
  collection: string;
  /** cg OBS's own program scene, the one the gate restored. */
  program: string;
  probeInput: string;
  timeoutMs?: number;
  pollMs?: number;
}

const isObject = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

/** Read an OBS scene collection document; null when it is not one. */
export function savedState(doc: unknown, probeInput: string): SavedState | null {
  if (!isObject(doc) || typeof doc.name !== "string") return null;
  const program = typeof doc.current_program_scene === "string" ? doc.current_program_scene : null;
  let probeSource: string | null = null;
  if (Array.isArray(doc.sources)) {
    const probe = doc.sources.find((s) => isObject(s) && s.name === probeInput);
    if (isObject(probe) && isObject(probe.settings)) {
      const src = probe.settings.ndi_source_name;
      probeSource = typeof src === "string" ? src : "";
    }
  }
  return { collection: doc.name, program, probeSource };
}

/** The restored state: cg OBS's own scene on program and no pointed probe. */
export function savedVerdict(state: SavedState | null, want: { program: string }): SavedVerdict {
  if (!state) return { ok: false, why: "the saved collection could not be read" };
  if (state.program !== want.program) {
    return {
      ok: false,
      why: `the saved program scene is "${state.program}", not cg OBS's own "${want.program}"`,
    };
  }
  if (state.probeSource) {
    return { ok: false, why: `the saved probe input still points at "${state.probeSource}"` };
  }
  return { ok: true };
}

/** The `*.json` in `dir` whose `name` is `collection`; null when there is none. */
export function readCollection(dir: string, collection: string): CollectionFile | null {
  let names: string[];
  try {
    names = fs.readdirSync(dir);
  } catch {
    return null;
  }
  for (const name of names) {
    if (path.extname(name) !== ".json") continue;
    const p = path.join(dir, name);
    try {
      const mtimeMs = fs.statSync(p).mtimeMs;
      const doc: unknown = JSON.parse(fs.readFileSync(p, "utf8"));
      if (isObject(doc) && doc.name === collection) return { path: p, mtimeMs, doc };
    } catch {
      // A file OBS is writing right now, or not JSON: not the collection (yet).
    }
  }
  return null;
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/**
 * Make cg OBS save its collection and read it back: create + remove
 * [`SAVE_SCENE`] (a leftover of a run that died between the two is removed
 * first), then wait until the file was rewritten AND says the restored state.
 * Throws naming what is saved when it does not within the bound.
 */
export async function persistCollection(opts: PersistOptions): Promise<SavedState> {
  const { rec, dir, collection, program, probeInput } = opts;
  const timeoutMs = opts.timeoutMs ?? PERSIST_TIMEOUT_MS;
  const pollMs = opts.pollMs ?? 100;
  const before = readCollection(dir, collection);
  if (!before) throw new Error(`no saved collection "${collection}" in ${dir}`);
  if ((await rec.listScenes()).includes(SAVE_SCENE)) await rec.removeScene(SAVE_SCENE);
  await rec.createScene(SAVE_SCENE);
  await rec.removeScene(SAVE_SCENE);
  const deadline = Date.now() + timeoutMs;
  let last = before;
  for (;;) {
    const now = readCollection(dir, collection);
    if (now) last = now;
    const state = now ? savedState(now.doc, probeInput) : null;
    if (now && now.mtimeMs > before.mtimeMs && savedVerdict(state, { program }).ok) return state!;
    if (Date.now() >= deadline) break;
    await sleep(pollMs);
  }
  const verdict = savedVerdict(savedState(last.doc, probeInput), { program });
  const rewritten = last.mtimeMs > before.mtimeMs;
  throw new Error(
    rewritten
      ? `cg OBS saved "${collection}" but not the restore within ${timeoutMs} ms: ${verdict.ok ? "" : verdict.why}`
      : `cg OBS did not save "${collection}" within ${timeoutMs} ms after the scene create + remove; ` +
          `a crash would load: ${verdict.ok ? "the restored state" : verdict.why}`,
  );
}
