---
paths:
  - "crates/sp-server/src/lyrics/bootstrap*.rs"
---

# Lyrics venv bootstrap — the probe's reason decides, never a bare bool (#221)

`lyrics/bootstrap.rs::ensure_ready` runs once per SongPlayer start (the
lyrics worker waits for it). Its FAST PATH decides whether the ~6 min
reinstall (qwen-asr, `audio-separator[gpu]`, the cu124 torch
`--force-reinstall`, the numpy repair) runs.

## Why it is shaped like this (the 28.9.2026 blocker, #221 comment 5877957738)

`is_ready` used to return a bare `bool` with a 45 s timeout, and ANY false —
a probe that timed out or failed CUDA init while the fresh process brought up
13 NDI pipelines and OBS/Arena loaded the GPU — ran the full reinstall. From
28.9 12:08Z every deploy restart did (~6 min of pip on the live box), and the
post-deploy A/V gate, which shared the venv's numpy, failed with `No module
named 'numpy.fft'` while pip replaced it (CI run 36475215084).

## The contract (`lyrics/bootstrap_probe.rs`)

- `is_ready(python) -> Readiness`: `Ready`, `Missing` (no interpreter),
  `Timeout` (45 s, `PROBE_TIMEOUT`; the probe is killed via `kill_on_drop`),
  `Failed { code, stderr_tail }` (the last 600 characters of stderr).
  `IS_READY_PROBE` is unchanged: it exits 1 with NO traceback when
  `torch.cuda.is_available()` is false.
- `Readiness::action`: `Ready` → fast path; `Missing` or a PROVEN import
  failure (`import_failure`: the LAST non-empty stderr line starts with
  `ModuleNotFoundError` / `ImportError` — numba's "needs NumPy 2.4 or less"
  is one, the #144 repair) → install at once; everything else (a timeout,
  CUDA unavailable, an `OSError` loading torch's DLLs, a spawn error) →
  retry.
- `decide(probe, RETRY_PLAN)`: pauses 5, 10, 20, 40, 60 s (probes at 0, 5,
  15, 35, 75, 135 s when each answers at once), no pause that would end after
  180 s. After the budget: still `Timeout` → `UseAsIs` (the venv is used as
  it is — a timeout NEVER triggers the torch force-reinstall); any other
  failure → install (the CUDA repair of a CPU-only torch).
- The loop is `for probes in 1..=MAX_PROBES` (12) and breaks on the cap or
  the budget BEFORE it pauses (review round 1): a loop that ends only on a
  comparison can be made to spin by one mutant (`>` → `==` never equals the
  budget; a halved pause shrinks to 0), and a hang fails the mutation gate
  like a survivor.
- Tests run `decide` on a paused clock with a scripted probe
  (`bootstrap_probe_tests.rs`); the exact-budget boundary is pinned with a
  custom plan (135 vs 134 s), the cap with a one-day budget (12 probes,
  495 s).

## Reading the box log after a restart

- `lyrics bootstrap: the venv probe passed probe=N` then `venv already ready
  at …` (INFO) — the fast path; N > 1 means earlier probes failed or timed
  out, each logged as `the venv probe timed out` / `the venv probe failed
  code=… import_failure=… stderr_tail=…` (WARN).
- `the venv probe kept timing out — using the venv … as it is` (WARN) — no
  install; a slow GPU/driver at startup, not a broken venv.
- `the venv needs the install` (INFO) — the install path ran; the WARN just
  before it names the reason. `installing CUDA torch variant` after a restart
  is the event the #221 acceptance counts.

## The A/V gate has its own Python

The post-deploy A/V gate runs `scripts/av_sync_check.py` with
`C:\ProgramData\SongPlayer\e2e\avsync_venv` (created + verified by the E2E
step "Prepare the A/V gate's own Python (#221)"), never with `lyrics_venv`:
anything SongPlayer's bootstrap installs must not be able to break a gate
mid-run. See `obs-ndi-health.md` "Post-deploy A/V gate".
