// Pure audio-assertion helpers for the post-deploy audio E2E specs (#206).
//
// Both live-audio assertions in the post-deploy suite used to read ONE sample
// at an arbitrary moment and flaked on content/state luck (3 red post-deploy
// jobs on 22.9.2026). These helpers make the assertions deterministic and are
// unit tested in `frontend.spec.ts` (runs on ubuntu, mock-free).

/**
 * Index of the LAST sample of the FIRST run of `n` consecutive samples that are
 * each STRICTLY greater than `threshold`, or -1 if no such run exists.
 *
 * Used to wait until preview audio is provably flowing before taking a baseline:
 * a viewer that joins a running encoder session right after a song restart
 * receives the emitter's silence padding for its first seconds, so one early
 * sample is not proof of audibility.
 *
 * @param {number[]} samples
 * @param {number} threshold
 * @param {number} n  required streak length (>= 1)
 * @returns {number} index where the streak completes, or -1
 */
export function audibleStreak(samples, threshold, n) {
  if (!Number.isInteger(n) || n < 1) {
    throw new Error(`audibleStreak: n must be an integer >= 1, got ${n}`);
  }
  let run = 0;
  for (let i = 0; i < samples.length; i++) {
    if (samples[i] > threshold) {
      run += 1;
      if (run >= n) return i;
    } else {
      run = 0;
    }
  }
  return -1;
}

/**
 * Arithmetic mean of the FINITE numeric entries of `samples`, or null when none
 * are finite. `null` / `undefined` / `NaN` / `±Infinity` entries are dropped (a
 * codec-less runner or an empty spectral band reads non-finite), so a windowed
 * band average never collapses to NaN.
 *
 * @param {Array<number|null|undefined>} samples
 * @returns {number|null}
 */
export function averageDb(samples) {
  let sum = 0;
  let n = 0;
  for (const s of samples) {
    if (typeof s === "number" && Number.isFinite(s)) {
      sum += s;
      n += 1;
    }
  }
  return n > 0 ? sum / n : null;
}

// ── #184 round G2: helpers of the owner-path post-deploy spec ────────────────

/**
 * Linear RMS amplitude → dBFS. Digital silence, a negative, non-numeric or
 * non-finite read is `-Infinity` (below every threshold, never NaN), so a
 * "silent" decision is never confused by a missing read.
 *
 * @param {number|null|undefined} rms
 * @returns {number}
 */
export function rmsToDbfs(rms) {
  if (typeof rms !== "number" || !Number.isFinite(rms) || rms <= 0) {
    return Number.NEGATIVE_INFINITY;
  }
  return 20 * Math.log10(rms);
}

/**
 * Index of the LAST sample of the FIRST run of `n` consecutive samples that are
 * each STRICTLY below `thresholdDb`, or -1 if no such run exists — the mirror of
 * `audibleStreak`, used to prove a fader-to-zero reached the preview audio.
 * `-Infinity` (digital silence) is quiet; a NaN / null read is NOT proof of
 * quiet and breaks the run.
 *
 * @param {Array<number|null|undefined>} samples  dBFS readings
 * @param {number} thresholdDb
 * @param {number} n  required streak length (>= 1)
 * @returns {number} index where the streak completes, or -1
 */
export function quietStreak(samples, thresholdDb, n) {
  if (!Number.isInteger(n) || n < 1) {
    throw new Error(`quietStreak: n must be an integer >= 1, got ${n}`);
  }
  let run = 0;
  for (let i = 0; i < samples.length; i++) {
    const s = samples[i];
    if (typeof s === "number" && !Number.isNaN(s) && s < thresholdDb) {
      run += 1;
      if (run >= n) return i;
    } else {
      run = 0;
    }
  }
  return -1;
}

/**
 * The longest span (ms, `t` of the run's last sample minus `t` of its first) of
 * consecutive SILENT samples — `db` strictly below `thresholdDb`, `-Infinity`,
 * or a missing (null / NaN) read, which counts as silent here because this is
 * the "the preview never goes silent" check and no signal is no proof of sound.
 * Samples must be in time order. 0 when there is no silent sample (or only
 * single isolated ones).
 *
 * @param {Array<{t: number, db: number|null|undefined}>} samples
 * @param {number} thresholdDb
 * @returns {number}
 */
export function longestSilentRunMs(samples, thresholdDb) {
  let longest = 0;
  let runStart = null;
  for (const { t, db } of samples) {
    const silent = typeof db !== "number" || Number.isNaN(db) || db < thresholdDb;
    if (silent) {
      if (runStart === null) runStart = t;
      longest = Math.max(longest, t - runStart);
    } else {
      runStart = null;
    }
  }
  return longest;
}
