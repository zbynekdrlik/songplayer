---
paths:
  - "crates/sp-server/src/lyrics/heavy_containment*.rs"
  - "crates/sp-server/src/lyrics/heavy_slot.rs"
  - "crates/sp-server/src/process_start.rs"
---
# Heavy-child OS containment (#203) — Job Object CPU cap + affinity + memory priority

The heavy background children (stems separation / lyrics vocal-isolation / mtl
align / dub) already run `BELOW_NORMAL` under a 3-thread cap (#162), but a
priority class only ORDERS runnable threads — with free cores the child still
saturates the memory bus the NDI SDK's compression threads, OBS and Resolume
need. Box test 7 (#168) measured `send_video_async` at 40–115 ms/1440p frame
with a stems child resident (grid slot 33 ms) → genlock pacing collapse + the
#192 audio holes. #203 contains every heavy child at the OS level, on the box
(never relocated — owner directive 21.9.2026).

## What it does

- **Job Object CPU HARD cap** (`JobObjectCpuRateControlInformation`,
  `ENABLE | HARD_CAP`, `CpuRate = pct * 100`) — % of TOTAL machine CPU time.
- **Job Object affinity** (`JOB_OBJECT_LIMIT_AFFINITY` + `BasicLimitInformation.Affinity`),
  folded into the SAME extended-limit struct/call as the #162 memory ceiling +
  kill-on-close (one struct, one `SetInformationJobObject`).
- **Child memory priority LOW** (`SetProcessInformation(child,
  ProcessMemoryPriority, MEMORY_PRIORITY_LOW)`) so the wall's working set is
  never trimmed for the child (needs `PROCESS_SET_INFORMATION` on the OpenProcess
  handle — added alongside `PROCESS_SET_QUOTA | PROCESS_TERMINATE`).
- **SongPlayer itself → `HIGH_PRIORITY_CLASS`** at startup
  (`process_start::set_high_priority_class`, called from `lib.rs::start()`), NOT
  REALTIME (which starves the OS input/paging threads).

## Settings (read live, like the #162 kill-switches)

| key | default | format |
|---|---|---|
| `heavy_cpu_cap_pct` | **25** | integer, clamped `5..=100` (absent/invalid → 25) |
| `heavy_cpu_affinity_mask` | **top 4 logical cores** (24-core box → `f00000`; #168 round 5) | hex string, optional `0x`; zero/invalid → default |

The default affinity mask is DERIVED from the live core count
(`default_affinity_mask`), never a literal: the child gets the **TOP 4 logical
cores** (24 cores → `f00000` = cores 20–23), the wall processes (SongPlayer /
OBS / Resolume) keep the rest; a box with < 4 cores gets all of them.
`heavy_cpu_cap_pct = 100` + all-cores mask = today's behaviour (no containment).

**Why 4 logical cores, not the upper half (#168 round 5 — the measured fix).**
Round 4's paced measurement (issue #168 comment 5779505749, 22.9.2026) isolated
the channel that stalls the NDI `send_video_async` call while a separation child
is resident: core PLACEMENT, not CPU share or page-fault churn. Same child, same
work, different mask:

- **`fff000` (upper 12 logical cores)** — grid stall 30–105 ms/min, **0/12
  minutes ≤ 20 ms** (W1).
- **`f00000` (top 4 logical cores = 2 physical)** — grid held **3.8–19.3 ms,
  10/10 minutes** (W3b), child at ~1.0 core (no throughput loss vs ~0.6 on 12).
- **`c00000` (2 logical = 1 physical)** — STARVES the child (80 CPU-s in 12 min),
  so 4 logical cores is the floor.

Mechanism: the NDI SDK's unpinned compress/send threads (HIGH class) otherwise
land on a physical core whose SMT sibling runs an AVX-saturating RoFormer thread;
a 2-physical-core block leaves 10 of 12 physical cores free of the child.

## Architecture — where each piece lives

- **`lyrics/heavy_containment.rs`** is PURE + unit-tested + mutation-clean:
  `Containment { cpu_cap_pct, affinity_mask, memory_priority_low }`,
  `containment_from_settings(cap_str, mask_str, logical_cores)`,
  `default_affinity_mask(cores)` (clamped `1..=64`), `clamp_cap_pct`,
  `cpu_rate_from_pct(pct) = pct*100`, `affinity_mask_hex`. No loops, exact
  boundaries. **Do NOT add DB/Win32/globals here — it must stay pure.**
- **`lyrics/heavy_slot.rs`** holds the impure seam: a process-global
  `Mutex<Containment>` published by `refresh_containment(&pool)` (reads the two
  settings + `available_parallelism`) and read by `current_containment()` inside
  the `#[cfg(windows)]` `assign_win_job`. The 3 heavy workers
  (`stems/worker.rs`, `lyrics/worker.rs`, `dabing/worker.rs`) call
  `refresh_containment(&self.pool)` each tick BEFORE the heavy spawn — that is
  what makes a dashboard change take effect with no restart. A new heavy worker
  MUST call `refresh_containment` before its spawn, or its child runs on the last
  published (or default) containment. `current_affinity_block_cores()` exposes
  the popcount of the published mask so the cpu-idle thread cap
  (`heavy_plan::cpu_idle_threads_for(cores, block)` = `min(cores/4, block)`,
  never 0) never gives a child more torch threads than its core block — #168
  round 5: the 4-logical-core default block caps the 24-core box's quarter rule
  (6) to 4.
- **`GET /api/v1/status`** gains `heavy_containment { cap_pct, affinity_mask
  (hex), priority_class }`, resolved live from the same pure fn.

## Tier-0 (no-compile box) traps hit here

- **`cpu_rate_from_pct` + `Containment.memory_priority_low` are read ONLY inside
  `#[cfg(windows)]`** → dead_code in the Linux lib target. Both carry
  `#[cfg_attr(not(windows), allow(dead_code))]` (routes.rs reads `cpu_cap_pct` +
  `affinity_mask` cross-platform, so those two fields are fine).
- **windows-sys 0.59/0.60 symbols** (all in the already-enabled
  `Win32_System_JobObjects` + `Win32_System_Threading` features — no Cargo.toml
  change): the CpuRate value lives in the union `rate.Anonymous.CpuRate` (unsafe
  union write); `MEMORY_PRIORITY_INFORMATION { MemoryPriority: MEMORY_PRIORITY_LOW }`
  (single field); `HIGH_PRIORITY_CLASS` is a `PROCESS_CREATION_FLAGS`;
  `SetPriorityClass`/`SetProcessInformation` return `BOOL` (`== 0` = failure).
- **Redundant outer parens** around a return expression trip `unused_parens`
  under `-D warnings` (the mask expr is `((1 << top) - 1) << shift`, no outer
  wrap; the inner parens ARE precedence-required — Rust `-` binds tighter than
  `<<`). The shift RHS is bound to a `let shift = cores - top;` rather than
  inlined as `<< (cores - top)` precisely to avoid a redundant-paren warning
  (#168 round 5).

## Acceptance — round 5 (supervisor, after integration; #168 protocol)

With `heavy_cpu_affinity_mask` DELETED/cleared so the new default applies:
boot/status `heavy_containment.affinity_mask == "f00000"` (no setting), the next
`heavy child contained (pid …): cpu_cap=25% affinity=0xf00000` log line, and a
paced re-check window (pacing ON via the `genlock.md` recipe, child resident and
PRODUCTIVE ≥ 0.5 core): `submit_call_us_max ≤ 20 ms` in ≥ 8/10 minutes and `late`
≤ 100/min; then pacing OFF again (the production flip is #147's decision with a
wall soak). Verify the child is productive (`TotalProcessorTime` delta over 6 s >
0) before trusting a window — a starved child gives a false-clean grid.

## Box test 8 verdict (21.9.2026) — the containment is live [SUPERSEDED by round 4]

> **Superseded by round 4 (22.9.2026).** This section was round 3's page-fault
> theory, measured on the old `fff000` (upper-12) default. Round 4's paced
> session (see "Why 4 logical cores" above + issue #168 comment 5779505749)
> re-tested placement directly and found the stall channel is core PLACEMENT:
> confining the child to 4 logical cores (`f00000`) holds the grid (3.8–19.3 ms,
> 10/10 min) with SongPlayer's OWN page faults still at 118–160k/s in every
> window — the control window W0 (no child) held the grid WITH that churn, so
> SongPlayer's page-fault churn is NOT the stall channel. The recycling frame
> pool (#203 2b, `genlock.md`) shipped anyway (it is a real allocation win) but
> was not the fix. Keep the text below as the round-3 record.

The cap/affinity/memory-priority were confirmed applied (`heavy child contained
(pid …): cpu_cap=25% affinity=0xfff000 mem_priority_low=true`, child
`BelowNormal` on 0xFFF000, SongPlayer `High`) and the acceptance still FAILED:
`submit_call_us_max` 45–64 ms with the capped child at 0.39 core on a 5 %-busy
box, 7–26 ms without any child, 49 ms again when the child returned. Ruled out
by measurement on the box: CPU share, SongPlayer's priority class (High↔Normal
A/B), timer resolution, core parking, a bare CUDA context, held RAM, GPU.

**The mechanism is kernel memory-manager contention from page-fault churn:**
SongPlayer itself runs at 430–500k demand-zero page faults/s on the paced path
(≈ 2 GB/s of fresh pages — per-slot standby `to_vec()` at `submitter.rs:280`
for every idle pipeline + per-frame decoder/handoff clones), the heavy child adds
~136k/s, OBS's own faults go 3k → 33k/s, interrupts 72k → 105k/s. Every process
that touches fresh pages (the NDI SDK's send, DistroAV in OBS, the child) then
serializes on the MM locks / TLB-shootdown IPIs. **A CPU cap cannot reach this**
— the lever is an allocation-free steady state in SongPlayer (own the standby
frame once, recycle the decoder/handoff/SDK-holdover `Vec<u8>` through a pool).
Read `\Process(songplayer)\Page Faults/sec` BEFORE and AFTER any change on this
path; the target is a flat, near-zero steady state while playing.

## Reading the applied containment on win-resolume (no DB access)

- `GET http://127.0.0.1:8920/api/v1/status` → `heavy_containment` (the cap +
  mask the process WILL apply to the next child) + the log line `heavy child
  contained (pid …): mem_limit=…B cpu_cap=…% affinity=0x… mem_priority_low=…`
  (once per child spawn).
- Live OS truth while a child runs (`mcp__win-resolume__Shell`, ≤ 30 s):
  `Get-Process python | Select ProcessorAffinity, PriorityClass` — affinity
  should be the 4-core block (24-core box → `15728640` = `0xF00000`; #168 round
  5, was `16773120` = `0xFFF000`), class `BelowNormal`; `Get-Process SongPlayer |
  Select PriorityClass` → `High`. (Baseline before #203: children ran affinity
  `16777215` = all cores, SongPlayer `Normal`.)
