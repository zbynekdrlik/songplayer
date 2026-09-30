/**
 * The cache-layout rule of `post-deploy-flac.spec.ts` (#10, #136), as a pure
 * function over the cache directory listing so the mock suite tests it.
 *
 * Every cached song is a sidecar pair
 * `{song}_{artist}_{youtube_id}_normalized[_gf]_{video.mp4|audio.flac}`.
 * The startup self-heal (`startup::self_heal_cache`) deletes a lone half
 * (mid-download debris), EXCEPT a half a DB row records. That is a song split
 * across two names: a rename (the metadata repair, #136) moved one half and
 * could neither move the other nor move it back, and the row still plays it.
 *
 * The videos API exposes no file paths, so this recognises such a song by its
 * disk shape: ONE youtube id with no complete pair, exactly one lone video
 * half and exactly one lone audio half. Any other lone half is a failure.
 */

/** A sidecar: the youtube id is the 11 characters before `_normalized`. */
const SIDECAR_RE = /_([A-Za-z0-9_-]{11})_normalized(?:_gf)?_(video\.mp4|audio\.flac)$/;

/** A pre-FLAC single-file cache entry: `{…}_{id}_normalized[_gf].mp4` (no
 * `_video`/`_audio` suffix). The split-file migration removed them all. */
const LEGACY_SINGLE_RE = /_normalized(?:_gf)?\.mp4$/;

export interface CacheLayout {
  /** Normalized ids with neither a complete pair nor a split song. */
  missing: string[];
  /** Lone halves that are not the two halves of a split song. */
  half: string[];
  /** Ids whose song is split across two names (kept by the self-heal). */
  split: string[];
  /** Legacy single-file entries. */
  legacy: string[];
}

interface IdFiles {
  complete: number;
  loneVideo: string[];
  loneAudio: string[];
}

const videoOf = (audio: string) => audio.replace(/_audio\.flac$/, "_video.mp4");
const audioOf = (video: string) => video.replace(/_video\.mp4$/, "_audio.flac");

function isSplit(f: IdFiles): boolean {
  return f.complete === 0 && f.loneVideo.length === 1 && f.loneAudio.length === 1;
}

/** Classify the cache listing `names` against the normalized youtube ids. */
export function checkCacheLayout(names: string[], normalizedIds: Iterable<string>): CacheLayout {
  const nameSet = new Set(names);
  const byId = new Map<string, IdFiles>();
  for (const name of names) {
    const m = SIDECAR_RE.exec(name);
    if (!m) continue;
    const [, id, kind] = m;
    let files = byId.get(id);
    if (!files) {
      files = { complete: 0, loneVideo: [], loneAudio: [] };
      byId.set(id, files);
    }
    if (kind === "video.mp4") {
      if (nameSet.has(audioOf(name))) files.complete += 1;
      else files.loneVideo.push(name);
    } else if (!nameSet.has(videoOf(name))) {
      files.loneAudio.push(name);
    }
  }

  const split = [...byId].filter(([, f]) => isSplit(f)).map(([id]) => id);
  const half = [...byId.values()]
    .filter((f) => !isSplit(f))
    .flatMap((f) => [...f.loneVideo, ...f.loneAudio]);
  const missing = [...new Set(normalizedIds)].filter((id) => {
    const f = byId.get(id);
    return f === undefined || (f.complete === 0 && !isSplit(f));
  });
  const legacy = names.filter((n) => LEGACY_SINGLE_RE.test(n));
  return {
    missing: missing.sort(),
    half: half.sort(),
    split: split.sort(),
    legacy: legacy.sort(),
  };
}
