/**
 * The A/V gate makes cg OBS persist its restored state (#271) — stub (RED).
 */

/** The scene the gate creates and removes to make cg OBS save its collection. */
export const SAVE_SCENE = "A/V gate save";
/** How long the gate waits for the collection file to read the restored state. */
export const PERSIST_TIMEOUT_MS = 5_000;

/** What cg OBS's saved collection says, read from its JSON file. */
export interface SavedState {
  collection: string;
  program: string | null;
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
  dir: string;
  collection: string;
  program: string;
  probeInput: string;
  timeoutMs?: number;
  pollMs?: number;
}

export function savedState(_doc: unknown, _probeInput: string): SavedState | null {
  throw new Error("not implemented (#271)");
}

export function savedVerdict(_state: SavedState | null, _want: { program: string }): SavedVerdict {
  throw new Error("not implemented (#271)");
}

export function readCollection(_dir: string, _collection: string): CollectionFile | null {
  throw new Error("not implemented (#271)");
}

export async function persistCollection(_opts: PersistOptions): Promise<SavedState> {
  throw new Error("not implemented (#271)");
}
