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
