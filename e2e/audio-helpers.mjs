// Pure audio-assertion helpers for the post-deploy audio E2E specs (#206).
//
// RED stub — the real bodies land in the GREEN commit. Both live-audio
// assertions in the post-deploy suite used to read ONE sample at an arbitrary
// moment and flaked on content/state luck (3 red post-deploy jobs on
// 22.9.2026). These helpers make the assertions deterministic and are unit
// tested in `frontend.spec.ts`.

/**
 * Index of the LAST sample of the FIRST run of `n` consecutive samples that are
 * each STRICTLY greater than `threshold`, or -1 if no such run exists.
 *
 * @param {number[]} samples
 * @param {number} threshold
 * @param {number} n  required streak length (>= 1)
 * @returns {number}
 */
export function audibleStreak(samples, threshold, n) {
  throw new Error("audibleStreak not implemented");
}

/**
 * Arithmetic mean of the FINITE numeric entries of `samples`, or null when none
 * are finite (null / NaN / +/-Infinity entries are dropped).
 *
 * @param {Array<number|null|undefined>} samples
 * @returns {number|null}
 */
export function averageDb(samples) {
  throw new Error("averageDb not implemented");
}
