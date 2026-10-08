# Program Audio Outputs (VBAN list + ASIO with a drift-compensating resampler) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** SongPlayer's 48 kHz program audio reaches every destination at the same time, each in its own transport, rate and format: a list of outputs (`audio_outputs`) with VBAN entries (the fallback, each at its own rate / format / stream / delay) and ASIO entries (the core path: Dante Virtual Soundcard at SNV and PP) whose card clock is followed by a drift-compensating resampler, plus one network sample rate (`audio_network_rate`) the outputs default to.

**Architecture:** After the program's peak limiter, `ProgramOutput::serve` hands each boundary's block (shared `Arc<[f32]>`, one copy) to a fan-out (`playback/audio_out.rs`) that pushes it into every running output's own bounded, drop-oldest queue; every output has its own thread, so none can delay the boundary, the NDI submit, MAX or another output. A settings task re-reads the list every 5 s and rebuilds only the entries that changed. A VBAN output is today's paced `vban-output` thread generalized per destination (rate index, sample format, packet geometry, delay; a fixed-ratio rubato FFT converter for rates ≠ 48 kHz; 48 kHz INT24 is the untouched #210 byte path). An ASIO output (`azo`, no Steinberg SDK) is one worker thread per device that owns the driver (COM STA), runs a rubato `Async` sinc resampler whose ratio a camera-box-style servo (long-window least-squares rate + PI on the ring's latency, ±300 ppm, ≤ 5 ppm/s, re-centre on a step) adjusts, and fills an `rtrb` ring the driver's callback copies from (no allocation, lock or log in the callback).

**Tech Stack:** Rust 2024 / tokio / axum 0.8 / sqlx sqlite / serde_json (`raw_value`) / rubato `=5.0.1` (`Fft` + `Async`) / rtrb `=0.4.0` / azo `=0.2.1` (Windows only) / windows-sys 0.59 / Leptos 0.7 (sp-ui) / Playwright (mock + post-deploy) / GitHub Actions (ubuntu + windows-latest + the self-hosted win-resolume runner).

**Spec:** `docs/superpowers/specs/2026-10-07-audio-outputs-asio-design.md` (approved by the owner 7.10.2026, #233 comment 6034604457). Owner decisions: #233 comments 6033483765 (ASIO core, VBAN fallback, several outputs of each type), 6034292526 (one output list), 6034307216 (design section 1), 6034341232 (design section 2), 6034604457 (spec approved; DVS at SNV configured and patched into FOH's Ableton as **cg**; live testing through it allowed; fohabl itself untouched). Research: #233 comment 6033159389 and dev1 `~/.claude/work-products/songplayer/233-asio-research/*.md`. Executors read the spec and these comments.

## Decisions this plan takes where the spec is silent or (in the plan's reading) wrong

The main session confirms or overrides these before Lane 1 is dispatched (each is also listed in "Spec gaps" at the end):

1. **ASIO latency target = 2 grid slots (66.7 ms) + `delay_ms`, not "2–3 driver buffers".** The program hands one 1600-frame block (33.3 ms) per boundary, and the hand-off is normally 10–33 ms late (`vban-out.md`, finding 5915907311). A ring held at 2–3 driver buffers (≈ 3–8 ms at 128 frames / 96 kHz) would underrun on every block. The servo holds the boundary-to-play latency (ring fill + hand-off lateness, measured on the program wall) at `VBAN_SEND_LATENCY_100NS`, the same budget VBAN uses; the scratch model below shows 0 underruns with 30 ms jitter and ±44 ms clock steps (minimum ring fill 16.9 ms). `delay_ms` adds to it.
2. **VBAN's fixed ratio runs on rubato's synchronous `Fft` (both sides fixed), the ASIO path on `Async` sinc.** `Fft` with `FixedSync::Both` turns one 1600-frame block into exactly `rate/30` frames for every supported rate, which VBAN's fixed packet schedule needs; `Async` (FixedAsync::Input) varies its output by a frame, which only a ring tolerates. Both are rubato 5.0.1.
3. **The `vban_*` → list migration is a startup step in Rust (`audio_out_migrate.rs`), not a numbered SQL migration.** Splitting a comma list into JSON objects in a recursive CTE is not mutation-testable, and a V31 number would race #229's V30 lane. It runs in one transaction at the first start of the outputs task and is idempotent (it acts only while an old key exists).
4. **`GET /api/v1/program` loses its top-level `vban` block** (one VBAN output no longer exists); each VBAN entry carries that same telemetry under `outputs[i].vban`. Consumers checked: the mock + `settings-vban.spec.ts` (replaced here); camera-box's `scripts/lib/cg-chain-songplayer.sh` reads only `source` and `remote.program_scene`.
5. **ASIO sample types: the spec's four plus `Int32LSB16/18/20/24`** (ASIO codes 24–27, a 32-bit container). DVS's exact type is unrecorded (research gap) and DVS at SNV runs "AsioEncoding 24", which a driver may report as `Int24LSB` (17) or `Int32LSB24` (27); refusing 27 would fail the SNV gate on day one. Float64 / MSB / DSD types still refuse with a clear reason.
6. **A re-centre is a 5 ms fade-out, the inserted silence or the skipped samples, then a 5 ms fade-in** (applied after the resampler, at the device rate). No step ever reaches the audio as a click; an insert is a short gap, a drop a short skip.
7. **Ids are `out-N`** (`next_id`: the highest N + 1); migrated entries are `out-1…`. One ASIO entry per driver (DVS takes one client); at most 16 outputs, 8 VBAN, 4 ASIO.
8. **The ASIO worker thread is NOT an MMCSS thread** (iemmixer rule: a helper must never pre-empt the driver's own callback thread); it has a 66.7 ms cushion. The VBAN threads stay MMCSS "Pro Audio" as today (one per entry).

## Global Constraints

- **Tier-0 (no local cargo compile).** Locally only `cargo fmt --all` / `cargo fmt --all --check` (then `git checkout -- crates/sp-server/src/db/models.rs` if fmt reordered it, `rust-workspace.md`), `cargo metadata` (the lockfile step in Tasks 1.5 / 2.3 / 3.5), python, node/Playwright against the mock. Every "Run" step names what CI runs (`cargo test -p <crate> <filter>` in the Test job, the Windows job's `cargo test --workspace`, the Frontend E2E job, the post-deploy E2E on win-resolume). Never `cargo build/test/check/clippy` locally (hook-enforced).
- **Lane start, every lane:** `git fetch origin && git merge origin/dev`; FIRST commit = VERSION bump: next `-dev.N` in `VERSION` (strictly above `git show origin/main:VERSION`), `./scripts/sync-version.sh`, commit `chore(#233): bump version to <X>`; then post the lane's design comment on #233 (`gh issue comment 233 --body-file <file>`: the "Design comment for #233" text in the lane header) BEFORE the first code commit (the design-gate hook reads it).
- **One push per lane**, after `cargo fmt --all --check` is clean, `rustfmt --edition 2024 --check <touched sp-ui files>` is clean (never a blanket sp-ui fmt), and `wc -l` of every touched `.rs` is ≤ 1000; then monitor CI to terminal state (all jobs, incl. Deploy to win-resolume + E2E).
- **1000-line cap per `.rs`.** Near the cap: `playback/program_bus.rs` 971 (Lane 1's edit must be line-neutral: a field type, an accessor, an import), `db/models.rs` 982 (never touch), `playback/mod.rs` 936 (+ 14 `mod` lines over the plan), `api/routes.rs` 914 and `lib.rs` 884 (untouched), `playback/vban_out.rs` 871 (Lane 1 removes ~90 lines before adding), `vban_out_tests.rs` 826 (new VBAN tests go to new files), `program_output.rs` 799, `program_tests.rs` 753 (the outputs tests go to a new file).
- **Untrusted JSON only into typed structs** (`rust-workspace.md` "Untrusted input never goes through serde_json::Value's own Deserialize"): the settings PATCH is parsed through `Box<RawValue>` maps into typed fields; no `serde_json::Value` is built from input. A serde error text is never echoed (it can quote the input): errors name the entry, the id (sanitized) and the field, or give line/column.
- **sp-core stays WASM-safe** (`test-wasm` job): `audio_outputs.rs` uses serde only, no tokio / std I/O / `cfg(windows)`.
- **Mutation gate (diff-scoped).** Every strict comparison gets an exact-boundary test; clamps via `.clamp/.min/.max`; no `|` of flag bits; loops make progress structurally; a timer loop or OS glue fn gets `#[cfg_attr(test, mutants::skip)]` with a reason line. `playback/asio_win.rs` (Windows-only azo glue) is excluded in `.cargo/mutants.toml` with a rationale naming the pure modules that carry its decisions (Task 3.5). `--list` the lane's range before pushing (`rust-workspace.md`) and map each mutant to its killing test.
- **Clippy `-D warnings` on Linux (`--all-targets`)**: no unused test helpers, let-chains instead of nested `if let`, `#[must_use]` on the type only, `.contains(&x)`, test-only locks across `.await` are `tokio::sync::Mutex`, `u64::div_ceil` instead of a hand-rolled ceil.
- **The ASIO host never sets the sample rate, never selects a clock source, never changes the buffer size and never opens the control panel** (spec §3; iemmixer I2). Task 3.5 adds a CI integrity scan that fails on `set_sample_rate`, `set_clock_source` or `open_control_panel` anywhere under `crates/`.
- **Real-time callback rule (iemmixer I7):** the ASIO buffer-switch callback and the message handlers allocate, lock, log and make syscalls never (apart from the driver's own `outputReady`); they copy, convert and count in atomics.
- **FOH stays byte-identical (Lane 1).** Migrated entries are `rate: 48000` (fixed, never `"network"`), `format: int24`, stream `sp-program` (the wire name exactly as #210 sent it); the 48 kHz path bypasses every converter; a test pins the datagrams byte for byte against a verbatim copy of the 0.72.0 encoder.
- **Every PowerShell `run:` block is ASCII** (`scripts/check_workflow_ps_ascii.py`).
- **Crate pins:** `rubato = "=5.0.1"` (default features: `fft_resampler`), `rtrb = "=0.4.0"`, `azo = "=0.2.1"` under `[target.'cfg(windows)'.dependencies]`. MIT / MIT-OR-Apache; no Steinberg SDK anywhere.
- **Machines:** a lane worker never touches win-resolume (SNV), resolume-pp (PP) or fohabl. Steps marked **MAIN SESSION OPS** are done by the main session. fohabl, VB-Matrix and Ableton are never touched by anyone (spec "Out of scope"). CI's own deploy to SNV is fine.
- **Slovak operator UI** (`slovak-only.spec.ts` denylist): every visible string of the new section is Slovak; product names (VBAN, ASIO, Dante) stay.
- **Commits:** `test(#233): …` (tests first, RED where the old code exists) then `feat(#233): …` / `refactor(#233): …` / `docs(#233): …` / `ci(#233): …`; every message ends with the session's attribution trailer. Never amend/rebase/force.

## Review Focus

1. **Saving the other Nastavenia fields while an outputs list exists.** Today the form saves the whole map and REPLACES `store.settings` with what it sent; a section reading `audio_outputs` from the store would show an empty list after any other save (and the next outputs save would wipe the list). Expected: the outputs list survives a save of the other fields, and the main form's PATCH carries no `audio_outputs`. → Playwright test in Task 1.10 (`saving the other settings keeps the outputs list`).
2. **An operator adds or edits one output while FOH plays.** Expected: every unchanged entry keeps its thread, queue and frame counter (no counter reset, no gap at FOH); only the changed entry is rebuilt. → test in Task 1.8 (`apply_keeps_an_unchanged_output_when_another_is_added`).
3. **A stored list this version cannot fully read** (a rollback after Lane 3 stored an `asio` entry; a hand-edited row; an unparsable value). Expected: the readable entries run, the rest are named in `outputs_problems` and a WARN, never "all outputs off". → tests in Task 1.2 (`a_stored_entry_this_version_cannot_read_is_skipped_and_the_rest_run`) and Task 1.9 (`get_program_names_a_stored_entry_it_could_not_read`).
4. **One ASIO driver claimed twice** (two entries on DVS, or DVS held by another app). Expected: two entries on one driver are refused at save, naming the entry and `asio.driver`; a busy driver waits 2 / 10 / 30 / 60 s between attempts with its reason shown — never a reopen storm, never a thread fight. → tests in Task 3.1 (`two_asio_entries_on_one_driver_are_refused`) and Task 3.4 (`a_busy_driver_retries_after_2_10_30_then_60_s`).
5. **The driver's rate changes under SongPlayer** (Dante Controller switched 96 → 48 kHz while DVS is open). Expected: the output closes, reopens at the driver's new rate with a new resampler, and its status notes the mismatch with `audio_network_rate`. → tests in Task 3.4 (`a_rate_change_reopens_at_the_drivers_new_rate`) and Task 3.6 (`the_status_notes_a_driver_rate_off_the_network_rate`).

---

## File Structure

**New (Rust):**

| File | Responsibility | Lane |
|---|---|---|
| `crates/sp-core/src/audio_outputs.rs` (+`audio_outputs_tests.rs`) | The list model shared by server and UI: `OutputEntry`, `OutputType`, `RateChoice`, `VbanDest`, `VbanSampleFormat` (+ `AsioDest` in Lane 3), limits, `validate_entry` / `validate_list`, `ListError` (English + Slovak text), `effective_rate`, `next_id`, `new_vban`, `wire_stream_name`, `shown_id` | 1 (+3) |
| `playback/audio_out_block.rs` | `ProgramBlock` (was `VbanBlock`, samples now `Arc<[f32]>`), `is_program_block` | 1 |
| `playback/audio_out_config.rs` (+`_tests.rs`) | strict PATCH parse (field-named errors), lenient stored parse, `checked` for the settings PATCH, `load` | 1 (+3) |
| `playback/audio_out_migrate.rs` (+`_tests.rs`) | `vban_*` → entries (pure) + the one-transaction startup migration | 1 |
| `playback/vban_rate.rs` (+`_tests.rs`) | `VbanRateConverter`: 48 kHz bypass, else rubato `Fft` (FixedSync::Both) | 1 |
| `playback/audio_out.rs` (+`_tests.rs`) | the fan-out `AudioOutputs`, `RunningOutput`, `OutputSink`, `OutputStatus`, state + latency helpers | 1 (+3) |
| `playback/audio_out_task.rs` (+`_tests.rs`) | `plan` (keep / build / stop), `apply`, the 5 s settings task + DNS cadence, migration at start | 1 (+3) |
| `playback/vban_packet_tests_format.rs`, `vban_packet_tests_legacy.rs`, `vban_out_tests_dest.rs` | the per-destination VBAN tests; the 0.72.0 encoder as a test oracle | 1 |
| `api/program_tests_outputs.rs` | `outputs[]` on `GET /api/v1/program` | 1 (+3) |
| `api/settings_tests_audio.rs` | the PATCH of `audio_outputs` / `audio_network_rate` through the router | 1 (+3) |
| `playback/asrc_servo.rs` (+`_tests.rs`, `asrc_servo_sim_tests.rs`) | the drift servo (constants cited from camera-box), pure | 2 |
| `playback/asrc.rs` (+`_tests.rs`) | `Asrc` (rubato `Async` sinc, ratio from the servo) + `Splice` (fade / gap / skip re-centre) | 2 |
| `playback/asio_format.rs` (+`_tests.rs`) | `AsioSample` (driver sample types) + channel encode | 3 |
| `playback/asio_state.rs` (+`_tests.rs`) | backoff, close reasons, message replies, stall watch, latency/rate notes, ring size | 3 |
| `playback/audio_out_queue.rs` | `BlockQueue` (bounded, drop-oldest, condvar) for the ASIO output | 3 |
| `playback/asio_out.rs` (+`_tests.rs`, `asio_out_fake.rs`) | `AsioDevice` trait, `AsioOut`, `AsioWorker::step` (open / run / close / backoff), `run_asio_worker` | 3 |
| `playback/asio_win.rs` | `#[cfg(windows)]` azo glue: driver list, 4 callback slots, `WinAsioDevice`, the thread | 3 |
| `api/audio.rs` (+`audio_tests.rs`) | `GET /api/v1/audio/asio-drivers` | 3 |

**Modified (Rust):** `crates/sp-core/src/{lib.rs,config.rs}`; `crates/sp-server/Cargo.toml` (+`Cargo.lock`); `playback/{mod.rs,vban_packet.rs,vban_out.rs,program_output.rs,program_bus.rs}`; the program-output / VBAN test files that name `VbanBlock` (rename only); `api/{program.rs,program_tests.rs,settings.rs,mod.rs}`; `.cargo/mutants.toml` (Lane 3).

**sp-ui:** new `src/components/audio_outputs.rs` (+ Lane 3 ASIO fields); `src/components/{mod.rs,settings_form.rs}`; `src/pages/settings.rs`.

**e2e / CI / docs:** `e2e/mock-api.mjs`; new `e2e/settings-audio-outputs.spec.ts` (replaces `settings-vban.spec.ts`), `e2e/audio-outputs-gate.ts` + `.spec.ts`, `e2e/post-deploy-audio-outputs.spec.ts` (Lane 1), `e2e/post-deploy-audio-asio.spec.ts` (Lane 3); `.github/workflows/ci.yml` (Lane 3: the integrity scan, the ASIO gate's env); new `.claude/rules/audio-outputs.md`; `.claude/rules/vban-out.md`; `CLAUDE.md` (router line).

## Lanes (dispatch serially, in this order)

| Lane | Deliverable | Prod LoC (est.) | Depends on |
|---|---|---|---|
| 1 | Output list + migration + fan-out + VBAN per destination (rate, format, stream, delay) + "Zvukové výstupy" (VBAN) + FOH / 96 kHz live gates | ~1,250 Rust code lines (sp-core ~280, server ~970; ~1,600 with docs) + ~400 sp-ui + ~80 mock JS | — ; ops: camera-box heads-up before, `audio_network_rate` after |
| 2 | Drift servo + ASRC + splice, pure, with the closed-loop simulation | ~510 Rust | 1 |
| 3 | ASIO output (`azo`) + driver list route + ASIO UI + telemetry + live gates SNV / PP | ~1,300 Rust (sp-core ~80, pure / fake-tested ~710, Windows glue ~420, API + wiring ~90) + ~150 sp-ui + ~40 mock JS | 2; ops: DVS holder check before, entries + gate env after |

The lanes are larger than the house's ~300-line lanes because the spec fixes three lanes; each task inside is its own reviewable unit with its own test cycle.

---
## Lane 1 — Output list, migration, fan-out, VBAN per destination

Design comment for #233 (lane start): *Approach:* one setting `audio_outputs` (a JSON list; the typed model is `sp_core::audio_outputs`, shared with the dashboard) and `audio_network_rate`; a settings PATCH is parsed strictly (each error names the entry, id and field) and stored normalized; the outputs task reads the stored list leniently every 5 s and rebuilds only changed entries; at its first start it moves `vban_enabled` / `vban_stream_name` / `vban_targets` into one entry per target (`rate: 48000`, `int24`, the same wire stream name) in one transaction and deletes the old keys. `ProgramOutput::serve` hands each limited block (one `Arc<[f32]>` copy) to a fan-out that pushes it to every running output's own drop-oldest queue. A VBAN output is #210's paced thread per destination: rate index, sample format, packet geometry (the largest divisor of `rate/30` within 256 frames and the 1436-byte payload) and delay; rates ≠ 48 kHz convert through rubato's synchronous `Fft` (one 1600-frame block in, exactly `rate/30` out); 48 kHz INT24 is the untouched #210 byte path, pinned against a verbatim copy of the 0.72.0 encoder. `GET /api/v1/program` gains `outputs[]` (the per-VBAN telemetry moves under `outputs[i].vban`); Nastavenia gets "Zvukové výstupy". *Rejected:* a separate ASIO module next to an unchanged VBAN (owner option 2, rejected 6034292526); one VBAN thread sending every destination (the spec gives each output its own thread, so a slow destination cannot delay another); a SQL V31 migration (a comma list split in a recursive CTE is not mutation-testable, and the number races #229's V30); rubato `Async` for VBAN (its output varies by a frame per block; VBAN's fixed packet schedule needs exact counts). *Architektúra:* settings table (existing) + `sp_core::audio_outputs` (pure, serde) + `playback::{audio_out, audio_out_config, audio_out_migrate, audio_out_task, audio_out_block, vban_rate}` + the existing `vban_out` / `vban_packet` generalized; rubato `=5.0.1` (`Fft`, MIT/Apache) chosen over libsamplerate / speexdsp (research `asio_research_web.md` §C5: FFI or allocating / yanked).

**MAIN SESSION OPS — before this lane's push:** tell the camera-box session (it watches FOH's cg path; FOH has listened to SongPlayer's VBAN6 since 4.10, memory `project_resolume_heads_up_camera_box`): "SongPlayer's VBAN settings move into an output list at the next SNV deploy; fohabl.lan:6980 and lv1.lan:6980 stay 48 kHz INT24 `sp-program`, byte-identical; the deploy restarts SongPlayer as usual."

### Task 1.1: `sp_core::audio_outputs` — the list model and its validation

**Files:**
- Create: `crates/sp-core/src/audio_outputs.rs`, `crates/sp-core/src/audio_outputs_tests.rs`
- Modify: `crates/sp-core/src/lib.rs` (add `pub mod audio_outputs;` after `pub mod audio_level;`), `crates/sp-core/src/config.rs` (two keys + `audio_network_rate`, after `video_hw_decode`, ~line 122; its test appended to `mod tests`)

**Interfaces:**
- Consumes: `sp_core::config::DEFAULT_VBAN_STREAM_NAME`.
- Produces (used by every later task, server and UI):
  - consts `SUPPORTED_RATES: [u32; 5]`, `PROGRAM_RATE: u32 = 48_000`, `DEFAULT_NETWORK_RATE: u32 = 48_000`, `MAX_OUTPUTS = 16`, `MAX_VBAN_OUTPUTS = 8`, `MAX_DELAY_MS: u32 = 2_000`, `MAX_ID_LEN = 32`, `MAX_NAME_LEN = 64`, `MAX_HOST_LEN = 253`, `MAX_STREAM_NAME_LEN = 16`, `DEFAULT_VBAN_PORT: u16 = 6980`;
  - `enum OutputType { Vban }` (`as_str`, `parse(&str) -> Option<Self>`, `NAMES: &str`), `enum RateChoice { Network, Fixed(u32) }` (serde `"network"` | number), `enum VbanSampleFormat { Int16, Int24, Float32 }` (`as_str`, `parse`), `struct VbanDest { host, port: u16, stream_name, format }`, `struct OutputEntry { id, name, kind: OutputType /* "type" */, enabled, rate, delay_ms: u32, vban: Option<VbanDest> }` with `OutputEntry::vban(id, name, dest) -> Self` (enabled, `Network`, delay 0);
  - `enum Problem`, `struct EntryError { index, id, field: &'static str, problem }`, `enum ListError { TooMany { count }, TooManyOfType { kind: OutputType, count, max }, Entry(EntryError) }` — `Display` (English, the API's) and `sk()` (Slovak, the dashboard's);
  - `fn validate_entry(index: usize, e: &OutputEntry) -> Result<(), EntryError>`, `fn validate_list(entries: &[OutputEntry]) -> Result<(), ListError>`, `fn effective_rate(rate: RateChoice, network: u32) -> u32`, `fn next_id(entries: &[OutputEntry]) -> String`, `fn new_vban(entries: &[OutputEntry]) -> OutputEntry`, `fn wire_stream_name(name: &str) -> String`, `fn shown_id(id: &str) -> String`;
  - `sp_core::config::{SETTING_AUDIO_OUTPUTS = "audio_outputs", SETTING_AUDIO_NETWORK_RATE = "audio_network_rate", fn audio_network_rate(raw: Option<&str>) -> u32}`.
- **Rule for every later task:** an `OutputEntry` is built with `OutputEntry::vban(..)` (and Lane 3's `OutputEntry::asio(..)`) or by the server parser, never by a struct literal elsewhere, so Lane 3's new `asio` field touches only those places.

- [ ] **Step 1: Write the failing tests** — `crates/sp-core/src/audio_outputs_tests.rs`:

```rust
//! #233: the output list model — serde layout, defaults, every limit at its
//! exact edge, the error texts (English for the API, Slovak for the
//! dashboard), ids, the wire stream name.

use super::*;
use crate::config::{SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, audio_network_rate};

fn dest(host: &str, port: u16) -> VbanDest {
    VbanDest {
        host: host.into(),
        port,
        stream_name: "sp-program".into(),
        format: VbanSampleFormat::Int24,
    }
}

fn foh() -> OutputEntry {
    let mut e = OutputEntry::vban("out-1", "fohabl.lan:6980", dest("fohabl.lan", 6980));
    e.rate = RateChoice::Fixed(48_000);
    e
}

#[test]
fn the_setting_keys() {
    assert_eq!(SETTING_AUDIO_OUTPUTS, "audio_outputs");
    assert_eq!(SETTING_AUDIO_NETWORK_RATE, "audio_network_rate");
}

#[test]
fn a_vban_entry_serializes_in_the_spec_layout() {
    let text = serde_json::to_string(&foh()).unwrap();
    assert_eq!(
        text,
        r#"{"id":"out-1","name":"fohabl.lan:6980","type":"vban","enabled":true,"rate":48000,"delay_ms":0,"vban":{"host":"fohabl.lan","port":6980,"stream_name":"sp-program","format":"int24"}}"#
    );
    let mut net = foh();
    net.rate = RateChoice::Network;
    assert!(serde_json::to_string(&net).unwrap().contains(r#""rate":"network""#));
    let back: OutputEntry = serde_json::from_str(&text).unwrap();
    assert_eq!(back, foh());
}

#[test]
fn missing_optional_fields_take_their_defaults() {
    let e: OutputEntry = serde_json::from_str(
        r#"{"id":"out-2","name":"x","type":"vban","vban":{"host":"h","port":1}}"#,
    )
    .unwrap();
    assert!(e.enabled);
    assert_eq!(e.rate, RateChoice::Network);
    assert_eq!(e.delay_ms, 0);
    let v = e.vban.unwrap();
    assert_eq!(v.stream_name, "sp-program");
    assert_eq!(v.format, VbanSampleFormat::Int24);
}

#[test]
fn a_rate_is_network_or_a_whole_number() {
    let rate = |t: &str| serde_json::from_str::<RateChoice>(t);
    assert_eq!(rate(r#""network""#).unwrap(), RateChoice::Network);
    assert_eq!(rate("96000").unwrap(), RateChoice::Fixed(96_000));
    assert!(rate(r#""96000""#).is_err(), "a quoted number is not a rate");
    assert!(rate("-1").is_err());
    assert!(rate("4294967296").is_err(), "over u32");
    assert!(rate("48000.5").is_err());
}

#[test]
fn every_limit_is_pinned_at_its_edge() {
    let check = |f: &dyn Fn(&mut OutputEntry)| {
        let mut e = foh();
        f(&mut e);
        validate_entry(0, &e)
    };
    // id: 1..=32 of a-z 0-9 -
    assert!(check(&|e| e.id = "a".repeat(32)).is_ok());
    assert_eq!(check(&|e| e.id = "a".repeat(33)).unwrap_err().problem, Problem::TooLong);
    assert_eq!(check(&|e| e.id = String::new()).unwrap_err().problem, Problem::Empty);
    assert_eq!(check(&|e| e.id = "Out-1".into()).unwrap_err().problem, Problem::BadCharacters);
    assert!(check(&|e| e.id = "a-0".into()).is_ok());
    // name: 1..=64 characters, no control character, not blank
    assert!(check(&|e| e.name = "č".repeat(64)).is_ok(), "64 characters, not bytes");
    assert_eq!(check(&|e| e.name = "č".repeat(65)).unwrap_err().problem, Problem::TooLong);
    assert_eq!(check(&|e| e.name = "  ".into()).unwrap_err().problem, Problem::Empty);
    assert_eq!(check(&|e| e.name = "a\tb".into()).unwrap_err().problem, Problem::BadCharacters);
    // rate: network or a supported rate
    for hz in SUPPORTED_RATES {
        assert!(check(&|e| e.rate = RateChoice::Fixed(hz)).is_ok(), "{hz}");
    }
    assert_eq!(
        check(&|e| e.rate = RateChoice::Fixed(32_000)).unwrap_err().field,
        "rate"
    );
    // delay: 0..=2000 ms
    assert!(check(&|e| e.delay_ms = 2_000).is_ok());
    assert_eq!(check(&|e| e.delay_ms = 2_001).unwrap_err().problem, Problem::TooLarge);
    // vban.host: 1..=253 of A-Z a-z 0-9 . - _
    assert!(check(&|e| e.vban.as_mut().unwrap().host = "h".repeat(253)).is_ok());
    let long = check(&|e| e.vban.as_mut().unwrap().host = "h".repeat(254)).unwrap_err();
    assert_eq!((long.field, long.problem), ("vban.host", Problem::TooLong));
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().host = "a b".into()).unwrap_err().problem,
        Problem::BadCharacters
    );
    assert!(check(&|e| e.vban.as_mut().unwrap().host = "10.77.9.201".into()).is_ok());
    // vban.port: 1..=65535
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().port = 0).unwrap_err().field,
        "vban.port"
    );
    assert!(check(&|e| e.vban.as_mut().unwrap().port = 1).is_ok());
    // vban.stream_name: 1..=16 printable ASCII (0x20..=0x7e)
    assert!(check(&|e| e.vban.as_mut().unwrap().stream_name = "x".repeat(16)).is_ok());
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().stream_name = "x".repeat(17)).unwrap_err().problem,
        Problem::TooLong
    );
    assert!(check(&|e| e.vban.as_mut().unwrap().stream_name = " ~".into()).is_ok());
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().stream_name = "\u{1f}".into()).unwrap_err().problem,
        Problem::BadCharacters
    );
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().stream_name = "\u{7f}".into()).unwrap_err().problem,
        Problem::BadCharacters
    );
    // a vban entry needs its vban object
    let missing = check(&|e| e.vban = None).unwrap_err();
    assert_eq!((missing.field, missing.problem), ("vban", Problem::Missing));
}

#[test]
fn the_list_counts_types_and_duplicates() {
    let many: Vec<OutputEntry> = (1..=8)
        .map(|n| OutputEntry::vban(&format!("out-{n}"), "v", dest("h", 6980)))
        .collect();
    assert!(validate_list(&many).is_ok(), "8 VBAN entries");
    let mut nine = many.clone();
    nine.push(OutputEntry::vban("out-9", "v", dest("h", 6980)));
    assert_eq!(
        validate_list(&nine).unwrap_err(),
        ListError::TooManyOfType { kind: OutputType::Vban, count: 9, max: MAX_VBAN_OUTPUTS }
    );
    let seventeen: Vec<OutputEntry> = (1..=17)
        .map(|n| OutputEntry::vban(&format!("out-{n}"), "v", dest("h", 6980)))
        .collect();
    assert_eq!(validate_list(&seventeen).unwrap_err(), ListError::TooMany { count: 17 });
    let dup = vec![foh(), foh()];
    match validate_list(&dup).unwrap_err() {
        ListError::Entry(e) => assert_eq!((e.index, e.field, e.problem), (1, "id", Problem::Duplicate)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_error_names_the_entry_the_id_and_the_field() {
    let mut e = foh();
    e.vban.as_mut().unwrap().port = 0;
    let err = validate_list(&[OutputEntry::vban("out-7", "a", dest("h", 1)), e]).unwrap_err();
    assert_eq!(err.to_string(), "entry 2 (id out-1): vban.port must be 1-65535");
    assert_eq!(err.sk(), "Výstup 2 (out-1): port musí byť 1 až 65535");
    assert_eq!(
        ListError::TooMany { count: 17 }.to_string(),
        "audio_outputs has 17 entries (at most 16)"
    );
}

#[test]
fn a_shown_id_never_echoes_junk() {
    assert_eq!(shown_id("out-1"), "out-1");
    assert_eq!(shown_id("Out 1\u{0}"), "?ut?1?");
    assert_eq!(shown_id(&"a".repeat(40)).len(), MAX_ID_LEN);
}

#[test]
fn ids_count_up_from_the_highest_and_a_new_vban_entry_is_ready_to_fill() {
    assert_eq!(next_id(&[]), "out-1");
    let list = vec![
        OutputEntry::vban("out-2", "a", dest("h", 1)),
        OutputEntry::vban("x", "b", dest("h", 1)),
        OutputEntry::vban("out-10", "c", dest("h", 1)),
    ];
    assert_eq!(next_id(&list), "out-11");
    let fresh = new_vban(&list);
    assert_eq!(fresh.id, "out-11");
    assert_eq!(fresh.name, "VBAN 11");
    assert_eq!(fresh.kind, OutputType::Vban);
    assert_eq!(fresh.rate, RateChoice::Network);
    let v = fresh.vban.unwrap();
    assert_eq!((v.host.as_str(), v.port), ("", DEFAULT_VBAN_PORT));
    assert_eq!(v.stream_name, "sp-program");
}

#[test]
fn the_effective_rate_and_the_network_rate_setting() {
    assert_eq!(effective_rate(RateChoice::Network, 96_000), 96_000);
    assert_eq!(effective_rate(RateChoice::Fixed(48_000), 96_000), 48_000);
    assert_eq!(audio_network_rate(None), 48_000);
    assert_eq!(audio_network_rate(Some("96000")), 96_000);
    assert_eq!(audio_network_rate(Some(" 44100 ")), 44_100);
    assert_eq!(audio_network_rate(Some("32000")), 48_000);
    assert_eq!(audio_network_rate(Some("fast")), 48_000);
}

#[test]
fn the_wire_stream_name_is_what_vban_puts_on_the_wire() {
    assert_eq!(wire_stream_name("sp-program"), "sp-program");
    assert_eq!(wire_stream_name("abcdefghijklmnopq"), "abcdefghijklmnop");
    assert_eq!(wire_stream_name("čo\tje"), "_o_je");
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-core audio_outputs` and `cargo test -p sp-core config::tests` — Expected now: FAIL to compile (`unresolved module audio_outputs`). Locally: `cargo fmt --all --check`.

- [ ] **Step 3: Implement** — `crates/sp-core/src/audio_outputs.rs`:

```rust
//! #233: SongPlayer's program audio outputs — ONE list (`audio_outputs`
//! setting) of destinations, each in the transport it supports, and the audio
//! network's sample rate (`audio_network_rate`). WASM-safe: the dashboard
//! edits these types and runs the same validation the server runs on a
//! settings PATCH (`sp-server` `playback/audio_out_config.rs`).

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::DEFAULT_VBAN_STREAM_NAME;

/// The rates an output may run at, Hz (the VBAN rate indexes SongPlayer sends).
pub const SUPPORTED_RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 192_000];
/// The program's own rate (media is made 48 kHz offline, `normalize.rs`).
pub const PROGRAM_RATE: u32 = 48_000;
/// `audio_network_rate` when unset or unreadable.
pub const DEFAULT_NETWORK_RATE: u32 = 48_000;
pub const MAX_OUTPUTS: usize = 16;
/// VBAN entries: each is one paced MMCSS thread (#210's `VBAN_MAX_TARGETS`).
pub const MAX_VBAN_OUTPUTS: usize = 8;
pub const MAX_DELAY_MS: u32 = 2_000;
pub const MAX_ID_LEN: usize = 32;
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_HOST_LEN: usize = 253;
pub const MAX_STREAM_NAME_LEN: usize = 16;
pub const DEFAULT_VBAN_PORT: u16 = 6980;

/// The transport of an output (Lane 3 adds `Asio`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Vban,
}

impl OutputType {
    /// The values `type` accepts, for an error text.
    pub const NAMES: &'static str = "vban";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vban => "vban",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vban" => Some(Self::Vban),
            _ => None,
        }
    }
}

/// An output's rate: the network's (`audio_network_rate`) or a fixed one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RateChoice {
    #[default]
    Network,
    Fixed(u32),
}

impl Serialize for RateChoice {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Network => s.serialize_str("network"),
            Self::Fixed(hz) => s.serialize_u32(*hz),
        }
    }
}

impl<'de> Deserialize<'de> for RateChoice {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(RateVisitor)
    }
}

struct RateVisitor;

impl Visitor<'_> for RateVisitor {
    type Value = RateChoice;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"network\" or a rate in Hz")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<RateChoice, E> {
        match v {
            "network" => Ok(RateChoice::Network),
            _ => Err(E::custom("a rate is \"network\" or a number")),
        }
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<RateChoice, E> {
        u32::try_from(v)
            .map(RateChoice::Fixed)
            .map_err(|_| E::custom("a rate is at most 4294967295"))
    }
}

/// A VBAN destination's sample format (VBAN spec rev. 13, p. 9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VbanSampleFormat {
    Int16,
    #[default]
    Int24,
    Float32,
}

impl VbanSampleFormat {
    pub const NAMES: &'static str = "int16, int24 or float32";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Int16 => "int16",
            Self::Int24 => "int24",
            Self::Float32 => "float32",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "int16" => Some(Self::Int16),
            "int24" => Some(Self::Int24),
            "float32" => Some(Self::Float32),
            _ => None,
        }
    }
}

fn default_stream_name() -> String {
    DEFAULT_VBAN_STREAM_NAME.to_string()
}

fn yes() -> bool {
    true
}

/// Where a VBAN output sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VbanDest {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_stream_name")]
    pub stream_name: String,
    #[serde(default)]
    pub format: VbanSampleFormat,
}

/// One output of the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: OutputType,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub rate: RateChoice,
    #[serde(default)]
    pub delay_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vban: Option<VbanDest>,
}

impl OutputEntry {
    /// An enabled VBAN entry at the network rate, no delay.
    pub fn vban(id: &str, name: &str, dest: VbanDest) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            kind: OutputType::Vban,
            enabled: true,
            rate: RateChoice::Network,
            delay_ms: 0,
            vban: Some(dest),
        }
    }
}

/// What is wrong with one field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Empty,
    TooLong,
    BadCharacters,
    Duplicate,
    UnsupportedRate,
    TooLarge,
    Missing,
    BadPort,
}

impl Problem {
    fn text(self) -> &'static str {
        match self {
            Self::Empty => "is empty",
            Self::TooLong => "is too long",
            Self::BadCharacters => "has a character that is not allowed",
            Self::Duplicate => "is used by an earlier entry",
            Self::UnsupportedRate => {
                "must be \"network\" or 44100, 48000, 88200, 96000 or 192000"
            }
            Self::TooLarge => "is over 2000 ms",
            Self::Missing => "is missing",
            Self::BadPort => "must be 1-65535",
        }
    }

    fn sk(self) -> &'static str {
        match self {
            Self::Empty => "je prázdne",
            Self::TooLong => "je príliš dlhé",
            Self::BadCharacters => "obsahuje nepovolený znak",
            Self::Duplicate => "už má iný výstup",
            Self::UnsupportedRate => "musí byť podľa siete alebo 44100–192000 Hz",
            Self::TooLarge => "je viac ako 2000 ms",
            Self::Missing => "chýba",
            Self::BadPort => "musí byť 1 až 65535",
        }
    }
}

/// The Slovak name of a field, for the dashboard.
fn field_sk(field: &str) -> &'static str {
    match field {
        "id" => "identifikátor",
        "name" => "názov",
        "rate" => "frekvencia",
        "delay_ms" => "oneskorenie",
        "vban" => "nastavenie VBAN",
        "vban.host" => "cieľ",
        "vban.port" => "port",
        "vban.stream_name" => "názov streamu",
        _ => "pole",
    }
}

/// One entry's first problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryError {
    /// 0-based; shown 1-based.
    pub index: usize,
    pub id: String,
    pub field: &'static str,
    pub problem: Problem,
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "entry {} (id {}): {} {}",
            self.index + 1,
            shown_id(&self.id),
            self.field,
            self.problem.text()
        )
    }
}

/// Why a list is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListError {
    TooMany { count: usize },
    TooManyOfType { kind: OutputType, count: usize, max: usize },
    Entry(EntryError),
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooMany { count } => {
                write!(f, "audio_outputs has {count} entries (at most {MAX_OUTPUTS})")
            }
            Self::TooManyOfType { kind, count, max } => write!(
                f,
                "audio_outputs has {count} {} entries (at most {max})",
                kind.as_str()
            ),
            Self::Entry(e) => e.fmt(f),
        }
    }
}

impl ListError {
    /// The same refusal in Slovak, for the dashboard.
    pub fn sk(&self) -> String {
        match self {
            Self::TooMany { count } => {
                format!("Výstupov je {count}, najviac môže byť {MAX_OUTPUTS}")
            }
            Self::TooManyOfType { kind, count, max } => format!(
                "Výstupov {} je {count}, najviac môže byť {max}",
                kind.as_str().to_uppercase()
            ),
            Self::Entry(e) => format!(
                "Výstup {} ({}): {} {}",
                e.index + 1,
                shown_id(&e.id),
                field_sk(e.field),
                e.problem.sk()
            ),
        }
    }
}

/// An id as an error shows it: at most [`MAX_ID_LEN`] characters, each one
/// outside a-z 0-9 - shown as `?` (an error never echoes junk input).
pub fn shown_id(id: &str) -> String {
    id.chars()
        .take(MAX_ID_LEN)
        .map(|c| if id_char(c) { c } else { '?' })
        .collect()
}

fn id_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

fn id_problem(id: &str) -> Option<Problem> {
    if id.is_empty() {
        Some(Problem::Empty)
    } else if id.len() > MAX_ID_LEN {
        Some(Problem::TooLong)
    } else if !id.chars().all(id_char) {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

fn name_problem(name: &str) -> Option<Problem> {
    if name.trim().is_empty() {
        Some(Problem::Empty)
    } else if name.chars().count() > MAX_NAME_LEN {
        Some(Problem::TooLong)
    } else if name.chars().any(char::is_control) {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

fn host_problem(host: &str) -> Option<Problem> {
    if host.is_empty() {
        Some(Problem::Empty)
    } else if host.len() > MAX_HOST_LEN {
        Some(Problem::TooLong)
    } else if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

fn stream_problem(name: &str) -> Option<Problem> {
    if name.is_empty() {
        Some(Problem::Empty)
    } else if name.len() > MAX_STREAM_NAME_LEN {
        Some(Problem::TooLong)
    } else if !name.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

/// The first problem of entry `index` on its own (the list rules — counts,
/// duplicate ids — are [`validate_list`]'s).
pub fn validate_entry(index: usize, e: &OutputEntry) -> Result<(), EntryError> {
    let err = |field: &'static str, problem: Problem| EntryError {
        index,
        id: e.id.clone(),
        field,
        problem,
    };
    if let Some(p) = id_problem(&e.id) {
        return Err(err("id", p));
    }
    if let Some(p) = name_problem(&e.name) {
        return Err(err("name", p));
    }
    if let RateChoice::Fixed(hz) = e.rate
        && !SUPPORTED_RATES.contains(&hz)
    {
        return Err(err("rate", Problem::UnsupportedRate));
    }
    if e.delay_ms > MAX_DELAY_MS {
        return Err(err("delay_ms", Problem::TooLarge));
    }
    match e.kind {
        OutputType::Vban => {
            let Some(v) = &e.vban else {
                return Err(err("vban", Problem::Missing));
            };
            if let Some(p) = host_problem(&v.host) {
                return Err(err("vban.host", p));
            }
            if v.port == 0 {
                return Err(err("vban.port", Problem::BadPort));
            }
            if let Some(p) = stream_problem(&v.stream_name) {
                return Err(err("vban.stream_name", p));
            }
        }
    }
    Ok(())
}

/// The whole list: at most [`MAX_OUTPUTS`] entries and
/// [`MAX_VBAN_OUTPUTS`] VBAN ones, every entry valid, ids unique.
pub fn validate_list(entries: &[OutputEntry]) -> Result<(), ListError> {
    if entries.len() > MAX_OUTPUTS {
        return Err(ListError::TooMany {
            count: entries.len(),
        });
    }
    let vban = entries.iter().filter(|e| e.kind == OutputType::Vban).count();
    if vban > MAX_VBAN_OUTPUTS {
        return Err(ListError::TooManyOfType {
            kind: OutputType::Vban,
            count: vban,
            max: MAX_VBAN_OUTPUTS,
        });
    }
    for (i, e) in entries.iter().enumerate() {
        validate_entry(i, e).map_err(ListError::Entry)?;
        if entries[..i].iter().any(|p| p.id == e.id) {
            return Err(ListError::Entry(EntryError {
                index: i,
                id: e.id.clone(),
                field: "id",
                problem: Problem::Duplicate,
            }));
        }
    }
    Ok(())
}

/// The rate an output with `rate` runs at on a network at `network` Hz.
pub fn effective_rate(rate: RateChoice, network: u32) -> u32 {
    match rate {
        RateChoice::Network => network,
        RateChoice::Fixed(hz) => hz,
    }
}

fn id_number(id: &str) -> Option<u32> {
    id.strip_prefix("out-")?.parse().ok()
}

/// The next free `out-N`: one above the highest N in use.
pub fn next_id(entries: &[OutputEntry]) -> String {
    let max = entries.iter().filter_map(|e| id_number(&e.id)).max().unwrap_or(0);
    format!("out-{}", max + 1)
}

/// The entry the dashboard's "add VBAN" creates: its host is left for the
/// operator (validation refuses it empty).
pub fn new_vban(entries: &[OutputEntry]) -> OutputEntry {
    let id = next_id(entries);
    let n = id_number(&id).unwrap_or(1);
    OutputEntry::vban(
        &id,
        &format!("VBAN {n}"),
        VbanDest {
            host: String::new(),
            port: DEFAULT_VBAN_PORT,
            stream_name: default_stream_name(),
            format: VbanSampleFormat::Int24,
        },
    )
}

/// The stream name as #210 put it on the wire (`vban_packet::stream_name_bytes`):
/// its first 16 characters, each non-ASCII or control character as `_`.
pub fn wire_stream_name(name: &str) -> String {
    name.chars()
        .take(MAX_STREAM_NAME_LEN)
        .map(|c| if c.is_ascii() && !c.is_ascii_control() { c } else { '_' })
        .collect()
}

#[cfg(test)]
#[path = "audio_outputs_tests.rs"]
mod tests;
```

And in `crates/sp-core/src/config.rs`, after the `video_hw_decode` fn:

```rust
/// #233: the program's audio outputs, one JSON list (`crate::audio_outputs`).
pub const SETTING_AUDIO_OUTPUTS: &str = "audio_outputs";
/// #233: the audio network's sample rate, Hz; an output whose rate is
/// "network" runs at it.
pub const SETTING_AUDIO_NETWORK_RATE: &str = "audio_network_rate";

/// #233: the stored network rate: a supported rate, else 48 kHz.
pub fn audio_network_rate(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|r| crate::audio_outputs::SUPPORTED_RATES.contains(r))
        .unwrap_or(crate::audio_outputs::DEFAULT_NETWORK_RATE)
}
```

Check the Slovak `sk()` text of the test: "Výstup 2 (out-1): port musí byť 1 až 65535" = `field_sk("vban.port")` + `Problem::BadPort.sk()` — the expected string in the test matches this code.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-core audio_outputs` and the `test-wasm` job (`cargo check -p sp-core --target wasm32-unknown-unknown`) — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the output list model — layout, defaults, limits at their edges, error texts` (the tests file + the `mod tests` hook with a stub module would not compile, so commit tests and model together as one `feat(#233): sp_core::audio_outputs — the output list model and its validation`; there is no old code for a RED).

### Task 1.2: the settings PATCH and the stored list on the server

**Files:**
- Create: `crates/sp-server/src/playback/audio_out_config.rs`, `crates/sp-server/src/playback/audio_out_config_tests.rs`, `crates/sp-server/src/api/settings_tests_audio.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (add `pub mod audio_out_config; // #233: the outputs' settings — strict PATCH parse, lenient stored read` in alphabetical place, after `pub mod audio_grid;`), `crates/sp-server/src/api/settings.rs` (`prepare` calls `checked` first; the `#[cfg(test)] #[path = "settings_tests_audio.rs"] mod tests_audio;` hook)

**Interfaces:**
- Consumes: Task 1.1's types and validation.
- Produces: `audio_out_config::{parse_list(raw: &str) -> Result<Vec<OutputEntry>, String>, struct Stored { entries, problems: Vec<String> }, parse_stored(raw: Option<&str>) -> Stored, checked(key: &str, value: &str) -> Result<String, String>, rates_text() -> String, struct OutputsSettings { entries, network_rate: u32, problems }, async load(&SqlitePool) -> Result<OutputsSettings, sqlx::Error>}`.

- [ ] **Step 1: Write the failing tests** — `audio_out_config_tests.rs`:

```rust
//! #233: the outputs' settings on the server — the strict PATCH parse (every
//! error names the entry, the id and the field, never the input), the
//! lenient stored read (a bad entry is skipped and named, the rest run).

use super::*;
use sp_core::audio_outputs::{OutputEntry, RateChoice, VbanDest, VbanSampleFormat};

const FOH: &str = r#"{"id":"out-1","name":"FOH","type":"vban","enabled":true,"rate":48000,"delay_ms":0,"vban":{"host":"fohabl.lan","port":6980,"stream_name":"sp-program","format":"int24"}}"#;

fn foh() -> OutputEntry {
    let mut e = OutputEntry::vban(
        "out-1",
        "FOH",
        VbanDest {
            host: "fohabl.lan".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    );
    e.rate = RateChoice::Fixed(48_000);
    e
}

#[test]
fn a_valid_list_parses_and_normalizes() {
    assert_eq!(parse_list(&format!("[{FOH}]")).unwrap(), vec![foh()]);
    assert_eq!(parse_list("[]").unwrap(), Vec::<OutputEntry>::new());
}

#[test]
fn each_type_error_names_the_entry_and_the_field_never_the_value() {
    let cases = [
        (r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":"secret-ish-text"}}]"#,
         "entry 1 (id out-1): vban.port has the wrong type"),
        (r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":70000}}]"#,
         "entry 1 (id out-1): vban.port has the wrong type"),
        (r#"[{"id":"out-1","name":"a","type":"vban"}]"#,
         "entry 1 (id out-1): vban is missing"),
        (r#"[{"id":"out-1","name":"a","type":"midi","vban":{"host":"h","port":1}}]"#,
         "entry 1 (id out-1): type must be vban"),
        (r#"[{"name":"a","type":"vban"}]"#, "entry 1: id is missing"),
        (r#"[{"id":7}]"#, "entry 1: id has the wrong type"),
        (r#"[{"id":"out-1","name":"a","type":"vban","rate":"fast","vban":{"host":"h","port":1}}]"#,
         "entry 1 (id out-1): rate has the wrong type"),
        (r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":1,"format":"int8"}}]"#,
         "entry 1 (id out-1): vban.format must be int16, int24 or float32"),
        (r#"[{"id":"out-1","name":"a","type":"vban","enabled":"yes","vban":{"host":"h","port":1}}]"#,
         "entry 1 (id out-1): enabled has the wrong type"),
        (r#"[1]"#, "entry 1 is not a JSON object"),
    ];
    for (input, want) in cases {
        let err = parse_list(input).unwrap_err();
        assert!(!err.contains("secret-ish"), "no echo of the input");
        assert_eq!(err, want, "{input}");
    }
}

#[test]
fn not_a_list_gives_line_and_column_only() {
    let err = parse_list("{\"a\":1}").unwrap_err();
    assert_eq!(err, "audio_outputs is not a JSON list (line 1, column 1)");
}

#[test]
fn a_value_error_comes_from_the_shared_validation() {
    let input = r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":0}}]"#;
    assert_eq!(
        parse_list(input).unwrap_err(),
        "entry 1 (id out-1): vban.port must be 1-65535"
    );
}

#[test]
fn a_stored_entry_this_version_cannot_read_is_skipped_and_the_rest_run() {
    let raw = format!(
        r#"[{FOH},{{"id":"out-2","name":"DVS","type":"asio","asio":{{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}}},{FOH},{{"id":"out-4","name":"x","type":"vban","vban":{{"host":"","port":1}}}}]"#
    );
    let stored = parse_stored(Some(&raw));
    assert_eq!(stored.entries, vec![foh()]);
    assert_eq!(
        stored.problems,
        vec![
            "entry 2 (id out-2): type must be vban".to_string(),
            "entry 3 (id out-1): id is used by an earlier entry".to_string(),
            "entry 4 (id out-4): vban.host is empty".to_string(),
        ]
    );
}

#[test]
fn a_stored_value_that_is_not_a_list_runs_nothing_and_says_so() {
    let stored = parse_stored(Some("not json"));
    assert!(stored.entries.is_empty());
    assert_eq!(stored.problems.len(), 1);
    assert!(stored.problems[0].starts_with("audio_outputs is not a JSON list"));
    assert_eq!(parse_stored(None), Stored::default());
    assert_eq!(parse_stored(Some("  ")), Stored::default());
}

#[test]
fn the_stored_read_keeps_the_count_limits() {
    let entries: Vec<String> = (1..=10)
        .map(|n| format!(r#"{{"id":"out-{n}","name":"v","type":"vban","vban":{{"host":"h","port":1}}}}"#))
        .collect();
    let stored = parse_stored(Some(&format!("[{}]", entries.join(","))));
    assert_eq!(stored.entries.len(), 8, "the first 8 VBAN entries run");
    assert_eq!(stored.problems.len(), 2);
    assert_eq!(stored.problems[0], "entry 9 (id out-9): over the 8 vban entries");
}

#[test]
fn the_patch_check_normalizes_the_list_and_the_rate() {
    assert_eq!(
        checked("audio_outputs", &format!("[ {FOH} ]")).unwrap(),
        format!("[{FOH}]")
    );
    assert_eq!(checked("audio_outputs", "  ").unwrap(), "[]");
    assert_eq!(checked("audio_network_rate", " 96000 ").unwrap(), "96000");
    for bad in ["32000", "96k", "", "-1"] {
        assert_eq!(
            checked("audio_network_rate", bad).unwrap_err(),
            "audio_network_rate must be one of 44100, 48000, 88200, 96000, 192000"
        );
    }
    assert_eq!(checked("gemini_model", "x").unwrap(), "x", "other keys pass");
}
```

Note: `parse_list` is the strict one, so a duplicate id in a PATCH is `validate_list`'s error ("entry 2 (id out-1): id is used by an earlier entry"); the stored read produces the same text from `EntryError`'s `Display`.

`api/settings_tests_audio.rs` (through the real router; reuse the helpers of `settings_tests.rs` by making `patch`, `stored` and `body` `pub(super)` there — they are file-private today):

```rust
//! #233: `audio_outputs` / `audio_network_rate` through `PATCH /api/v1/settings`.

use axum::http::StatusCode;
use sp_core::config::{SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, SETTING_GEMINI_MODEL};

use super::tests::{body, patch, stored};
use crate::api::routes::tests::test_state;

const ONE: &str = r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":6980}}]"#;

#[tokio::test]
async fn a_valid_list_is_stored_normalized_with_its_defaults() {
    let state = test_state().await;
    let (status, _) = patch(&state, &body(&[(SETTING_AUDIO_OUTPUTS, ONE)])).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        stored(&state.pool, SETTING_AUDIO_OUTPUTS).await.unwrap(),
        r#"[{"id":"out-1","name":"a","type":"vban","enabled":true,"rate":"network","delay_ms":0,"vban":{"host":"h","port":6980,"stream_name":"sp-program","format":"int24"}}]"#
    );
}

#[tokio::test]
async fn a_bad_list_refuses_the_whole_patch_and_writes_nothing() {
    let state = test_state().await;
    let bad = r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":0}}]"#;
    let (status, text) = patch(
        &state,
        &body(&[(SETTING_AUDIO_OUTPUTS, bad), (SETTING_GEMINI_MODEL, "model-x")]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text, "entry 1 (id out-1): vban.port must be 1-65535");
    assert_eq!(stored(&state.pool, SETTING_AUDIO_OUTPUTS).await, None);
    assert_eq!(stored(&state.pool, SETTING_GEMINI_MODEL).await, None, "nothing written");
}

#[tokio::test]
async fn the_network_rate_is_checked() {
    let state = test_state().await;
    let (status, _) = patch(&state, &body(&[(SETTING_AUDIO_NETWORK_RATE, "96000")])).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(stored(&state.pool, SETTING_AUDIO_NETWORK_RATE).await.unwrap(), "96000");
    let (status, text) = patch(&state, &body(&[(SETTING_AUDIO_NETWORK_RATE, "32000")])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text.starts_with("audio_network_rate must be one of"));
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server audio_out_config` and `cargo test -p sp-server api::settings` — Expected now: FAIL to compile (`unresolved module audio_out_config`).

- [ ] **Step 3: Implement** — `crates/sp-server/src/playback/audio_out_config.rs`:

```rust
//! #233: the outputs' settings on the server. A PATCH of `audio_outputs` /
//! `audio_network_rate` is parsed strictly — every error names the entry, the
//! (sanitized) id and the field, never the value: serde's own error text can
//! quote the input — and stored normalized. The outputs task reads the stored
//! list leniently: an entry this version cannot read (a rollback, a hand-edited
//! row) is skipped and named in `problems`; the rest run. Untrusted JSON goes
//! only through `Box<RawValue>` maps into typed fields, never into
//! `serde_json::Value` (`rust-workspace.md`).

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use sp_core::audio_outputs::{
    EntryError, MAX_OUTPUTS, MAX_VBAN_OUTPUTS, OutputEntry, OutputType, Problem, RateChoice,
    SUPPORTED_RATES, VbanDest, VbanSampleFormat, shown_id, validate_entry, validate_list,
};
use sp_core::config::{SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, audio_network_rate};
use sqlx::SqlitePool;

type Fields = BTreeMap<String, Box<RawValue>>;

/// One JSON object's fields, read by name with errors that name them.
struct Reader<'a> {
    fields: Fields,
    at: &'a str,
    prefix: &'static str,
}

impl<'a> Reader<'a> {
    fn of(raw: &RawValue, at: &'a str, prefix: &'static str, what: &str) -> Result<Self, String> {
        let fields = serde_json::from_str(raw.get()).map_err(|_| format!("{what} is not a JSON object"))?;
        Ok(Self { fields, at, prefix })
    }

    fn opt<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, String> {
        self.fields
            .get(key)
            .map(|v| {
                serde_json::from_str(v.get())
                    .map_err(|_| format!("{}: {}{key} has the wrong type", self.at, self.prefix))
            })
            .transpose()
    }

    fn req<T: DeserializeOwned>(&self, key: &str) -> Result<T, String> {
        self.opt(key)?
            .ok_or_else(|| format!("{}: {}{key} is missing", self.at, self.prefix))
    }

    fn raw(&self, key: &str) -> Result<&RawValue, String> {
        self.fields
            .get(key)
            .map(|v| &**v)
            .ok_or_else(|| format!("{}: {}{key} is missing", self.at, self.prefix))
    }
}

fn list_items(raw: &str) -> Result<Vec<Box<RawValue>>, String> {
    serde_json::from_str(raw).map_err(|e| {
        format!(
            "audio_outputs is not a JSON list (line {}, column {})",
            e.line(),
            e.column()
        )
    })
}

fn vban_dest(raw: &RawValue, at: &str) -> Result<VbanDest, String> {
    let r = Reader::of(raw, at, "vban.", &format!("{at}: vban"))?;
    let format = match r.opt::<String>("format")? {
        None => VbanSampleFormat::default(),
        Some(text) => VbanSampleFormat::parse(&text).ok_or_else(|| {
            format!("{at}: vban.format must be {}", VbanSampleFormat::NAMES)
        })?,
    };
    Ok(VbanDest {
        host: r.req("host")?,
        port: r.req("port")?,
        stream_name: r
            .opt("stream_name")?
            .unwrap_or_else(|| sp_core::config::DEFAULT_VBAN_STREAM_NAME.to_string()),
        format,
    })
}

/// Entry `index` read field by field (no validation of values yet).
fn entry(index: usize, raw: &RawValue) -> Result<OutputEntry, String> {
    let first = format!("entry {}", index + 1);
    let head = Reader::of(raw, &first, "", &first)?;
    let id: String = head.req("id")?;
    let at = format!("{first} (id {})", shown_id(&id));
    // A new Reader, not `Reader { at: &at, ..head }`: functional update to a
    // different lifetime is the unstable type-changing struct update.
    let r = Reader { fields: head.fields, at: &at, prefix: "" };
    let kind_text: String = r.req("type")?;
    let kind = OutputType::parse(&kind_text)
        .ok_or_else(|| format!("{at}: type must be {}", OutputType::NAMES))?;
    let vban = match kind {
        OutputType::Vban => Some(vban_dest(r.raw("vban")?, &at)?),
    };
    Ok(OutputEntry {
        id,
        name: r.req("name")?,
        kind,
        enabled: r.opt("enabled")?.unwrap_or(true),
        rate: r.opt::<RateChoice>("rate")?.unwrap_or_default(),
        delay_ms: r.opt("delay_ms")?.unwrap_or(0),
        vban,
    })
}

/// A PATCH's list: every entry read, then the shared validation.
pub fn parse_list(raw: &str) -> Result<Vec<OutputEntry>, String> {
    let mut entries = Vec::new();
    for (i, item) in list_items(raw)?.iter().enumerate() {
        entries.push(entry(i, item)?);
    }
    validate_list(&entries).map_err(|e| e.to_string())?;
    Ok(entries)
}

/// The stored list as the outputs task runs it.
#[derive(Debug, Default, PartialEq)]
pub struct Stored {
    pub entries: Vec<OutputEntry>,
    pub problems: Vec<String>,
}

impl Stored {
    /// Keep entry `index` unless it breaks a rule the kept ones set.
    fn keep(&mut self, index: usize, e: OutputEntry) {
        let vban = self.entries.iter().filter(|k| k.kind == OutputType::Vban).count();
        let refusal = if let Err(err) = validate_entry(index, &e) {
            Some(err.to_string())
        } else if self.entries.iter().any(|k| k.id == e.id) {
            Some(
                EntryError {
                    index,
                    id: e.id.clone(),
                    field: "id",
                    problem: Problem::Duplicate,
                }
                .to_string(),
            )
        } else if self.entries.len() >= MAX_OUTPUTS {
            Some(format!("entry {} (id {}): over the {MAX_OUTPUTS} outputs", index + 1, shown_id(&e.id)))
        } else if e.kind == OutputType::Vban && vban >= MAX_VBAN_OUTPUTS {
            Some(format!(
                "entry {} (id {}): over the {MAX_VBAN_OUTPUTS} vban entries",
                index + 1,
                shown_id(&e.id)
            ))
        } else {
            None
        };
        match refusal {
            Some(problem) => self.problems.push(problem),
            None => self.entries.push(e),
        }
    }
}

/// The stored value, leniently: each unreadable or invalid entry is skipped
/// and named; a value that is no list runs nothing.
pub fn parse_stored(raw: Option<&str>) -> Stored {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Stored::default();
    };
    let items = match list_items(raw) {
        Ok(items) => items,
        Err(problem) => {
            return Stored {
                entries: Vec::new(),
                problems: vec![problem],
            };
        }
    };
    let mut out = Stored::default();
    for (i, item) in items.iter().enumerate() {
        match entry(i, item) {
            Ok(e) => out.keep(i, e),
            Err(problem) => out.problems.push(problem),
        }
    }
    out
}

/// The supported rates as an error lists them.
pub fn rates_text() -> String {
    SUPPORTED_RATES.map(|r| r.to_string()).join(", ")
}

/// The settings PATCH check of one key: the two output settings are refused
/// with the reason or normalized; every other key passes unchanged.
pub fn checked(key: &str, value: &str) -> Result<String, String> {
    match key {
        SETTING_AUDIO_OUTPUTS => {
            let entries = if value.trim().is_empty() {
                Vec::new()
            } else {
                parse_list(value)?
            };
            serde_json::to_string(&entries).map_err(|_| "audio_outputs could not be written".into())
        }
        SETTING_AUDIO_NETWORK_RATE => match value.trim().parse::<u32>() {
            Ok(rate) if SUPPORTED_RATES.contains(&rate) => Ok(rate.to_string()),
            _ => Err(format!("audio_network_rate must be one of {}", rates_text())),
        },
        _ => Ok(value.to_string()),
    }
}

/// What the outputs task runs: the stored list (leniently), the network rate.
#[derive(Debug, Default, PartialEq)]
pub struct OutputsSettings {
    pub entries: Vec<OutputEntry>,
    pub network_rate: u32,
    pub problems: Vec<String>,
}

pub async fn load(pool: &SqlitePool) -> Result<OutputsSettings, sqlx::Error> {
    use crate::db::models::get_setting;
    let stored = parse_stored(get_setting(pool, SETTING_AUDIO_OUTPUTS).await?.as_deref());
    let network_rate = audio_network_rate(get_setting(pool, SETTING_AUDIO_NETWORK_RATE).await?.as_deref());
    Ok(OutputsSettings {
        entries: stored.entries,
        network_rate,
        problems: stored.problems,
    })
}

#[cfg(test)]
#[path = "audio_out_config_tests.rs"]
mod tests;
```

`crates/sp-server/src/api/settings.rs`, in `prepare`'s loop (the module doc gains one bullet "- #233: `audio_outputs` / `audio_network_rate` are checked by `playback::audio_out_config::checked`: a bad value refuses the whole PATCH (400, the reason names the entry and the field), a good one is stored normalized."):

```rust
    for key in keys {
        let value = crate::playback::audio_out_config::checked(key, &sent[key])?;
        let value = crate::peer::config::checked(pool, key, &value, &sent).await?;
        writes.push((key.clone(), value));
    }
```

and after the existing test hook:

```rust
#[cfg(test)]
#[path = "settings_tests_audio.rs"]
mod tests_audio;
```

(`settings.rs`'s test module is `mod tests`; make its `body`, `patch` and `stored` helpers `pub(super)`.)

Mutation notes: `Ok(rate) if SUPPORTED_RATES.contains(&rate)` — the guard→`true` mutant is killed by `"32000"`; each `>=` in `keep` is killed by the 8/9 boundary test.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server audio_out_config` and `cargo test -p sp-server api::settings` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the outputs' settings — field-named PATCH errors, the lenient stored read, the router` then `feat(#233): audio_out_config — strict PATCH parse, lenient stored read, the settings check`.

### Task 1.3: the startup migration from `vban_*`

**Files:**
- Create: `crates/sp-server/src/playback/audio_out_migrate.rs`, `crates/sp-server/src/playback/audio_out_migrate_tests.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (`pub mod audio_out_migrate; // #233: vban_* → the output list, once`)

**Interfaces:**
- Consumes: `OutputEntry::vban`, `wire_stream_name`, `PROGRAM_RATE`, `MAX_NAME_LEN`, `validate_entry`; `audio_out_config::parse_list` (test only).
- Produces: `audio_out_migrate::{struct Migrated { entries, skipped: Vec<String> }, entries_from_vban(enabled: bool, stream_name: &str, targets: &str) -> Migrated, split_target(spec: &str) -> Option<(String, u16)>, enum MigrationOutcome { Nothing, Migrated(Migrated), DroppedStale { keys: usize } }, async migrate_vban_settings(&SqlitePool) -> Result<MigrationOutcome, sqlx::Error>}` (called by Task 1.8's task at its start).

- [ ] **Step 1: Write the failing tests** — `audio_out_migrate_tests.rs`:

```rust
//! #233: #210's three VBAN keys become one entry per target — 48 kHz fixed,
//! INT24, the same wire stream name — in one transaction, once.

use super::*;
use sp_core::audio_outputs::{RateChoice, VbanSampleFormat};

fn snv() -> Migrated {
    entries_from_vban(true, "sp-program", "fohabl.lan:6980,lv1.lan:6980")
}

#[test]
fn snv_keeps_both_targets_at_48k_int24_sp_program() {
    let m = snv();
    assert!(m.skipped.is_empty());
    assert_eq!(m.entries.len(), 2);
    let foh = &m.entries[0];
    assert_eq!((foh.id.as_str(), foh.name.as_str()), ("out-1", "fohabl.lan:6980"));
    assert!(foh.enabled);
    assert_eq!(foh.rate, RateChoice::Fixed(48_000), "never \"network\"");
    assert_eq!(foh.delay_ms, 0);
    let v = foh.vban.as_ref().unwrap();
    assert_eq!((v.host.as_str(), v.port), ("fohabl.lan", 6980));
    assert_eq!(v.stream_name, "sp-program");
    assert_eq!(v.format, VbanSampleFormat::Int24);
    assert_eq!(m.entries[1].id, "out-2");
    assert_eq!(m.entries[1].vban.as_ref().unwrap().host, "lv1.lan");
}

#[test]
fn the_stream_name_is_the_wire_name_210_sent() {
    let blank = entries_from_vban(true, "  ", "h:1");
    assert_eq!(blank.entries[0].vban.as_ref().unwrap().stream_name, "sp-program");
    let odd = entries_from_vban(true, " čo-je-to-za-stream ", "h:1");
    assert_eq!(odd.entries[0].vban.as_ref().unwrap().stream_name, "_o-je-to-za-stre");
}

#[test]
fn enabled_only_when_210_was_enabled() {
    assert!(!entries_from_vban(false, "", "h:1").entries[0].enabled);
}

#[test]
fn the_first_8_targets_migrate_like_210_used_them() {
    let ten: Vec<String> = (1..=10).map(|n| format!("h{n}:6980")).collect();
    let m = entries_from_vban(true, "", &ten.join(", "));
    assert_eq!(m.entries.len(), 8);
    assert_eq!(m.entries[7].id, "out-8");
    assert_eq!(
        m.skipped,
        vec![
            "h9:6980: over the 8 targets #210 used".to_string(),
            "h10:6980: over the 8 targets #210 used".to_string(),
        ]
    );
}

#[test]
fn a_target_210_never_resolved_is_skipped_and_named() {
    let m = entries_from_vban(true, "", "nohost, :6980, h:0, h:x, ok:1, a b:2");
    assert_eq!(m.entries.len(), 1);
    assert_eq!(m.entries[0].vban.as_ref().unwrap().host, "ok");
    assert_eq!(m.skipped.len(), 5);
    assert_eq!(m.skipped[0], "nohost: not host:port");
    assert_eq!(
        m.skipped[4],
        "a b:2: entry 2 (id out-2): vban.host has a character that is not allowed"
    );
}

#[test]
fn split_target_takes_the_last_colon() {
    assert_eq!(split_target("fohabl.lan:6980"), Some(("fohabl.lan".into(), 6980)));
    assert_eq!(split_target(" h : 1 "), Some(("h".into(), 1)));
    assert_eq!(split_target("h:65535"), Some(("h".into(), 65535)));
    assert_eq!(split_target("h:65536"), None);
    assert_eq!(split_target("h:0"), None);
    assert_eq!(split_target("h"), None);
    assert_eq!(split_target(":1"), None);
}

async fn pool() -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

async fn set(pool: &sqlx::SqlitePool, k: &str, v: &str) {
    crate::db::models::set_setting(pool, k, v).await.unwrap();
}

async fn get(pool: &sqlx::SqlitePool, k: &str) -> Option<String> {
    crate::db::models::get_setting(pool, k).await.unwrap()
}

#[tokio::test]
async fn the_first_start_moves_the_keys_into_the_list_once() {
    let pool = pool().await;
    set(&pool, "vban_enabled", "true").await;
    set(&pool, "vban_stream_name", "sp-program").await;
    set(&pool, "vban_targets", "fohabl.lan:6980,lv1.lan:6980").await;
    let outcome = migrate_vban_settings(&pool).await.unwrap();
    assert_eq!(outcome, MigrationOutcome::Migrated(snv()));
    let list = get(&pool, "audio_outputs").await.unwrap();
    let parsed = crate::playback::audio_out_config::parse_list(&list).unwrap();
    assert_eq!(parsed, snv().entries, "the stored list parses back strictly");
    for k in ["vban_enabled", "vban_stream_name", "vban_targets"] {
        assert_eq!(get(&pool, k).await, None, "{k} deleted");
    }
    assert_eq!(migrate_vban_settings(&pool).await.unwrap(), MigrationOutcome::Nothing);
    assert_eq!(get(&pool, "audio_outputs").await.unwrap(), list, "untouched the second time");
}

#[tokio::test]
async fn old_keys_next_to_a_list_are_dropped_and_the_list_kept() {
    let pool = pool().await;
    set(&pool, "audio_outputs", "[]").await;
    set(&pool, "vban_targets", "h:1").await;
    set(&pool, "vban_enabled", "true").await;
    assert_eq!(
        migrate_vban_settings(&pool).await.unwrap(),
        MigrationOutcome::DroppedStale { keys: 2 }
    );
    assert_eq!(get(&pool, "audio_outputs").await.unwrap(), "[]");
    assert_eq!(get(&pool, "vban_targets").await, None);
}

#[tokio::test]
async fn a_box_that_never_had_vban_gets_nothing_written() {
    let pool = pool().await;
    assert_eq!(migrate_vban_settings(&pool).await.unwrap(), MigrationOutcome::Nothing);
    assert_eq!(get(&pool, "audio_outputs").await, None);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server audio_out_migrate` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement** — `audio_out_migrate.rs`:

```rust
//! #233: the first start of the output list moves #210's three VBAN keys into
//! it — one entry per target, `rate: 48000` (fixed, never "network"), INT24 and
//! the stream name exactly as #210 put it on the wire — so FOH hears what it
//! heard before; then the old keys are deleted. One transaction; it acts only
//! while an old key exists. A list already stored wins: the old keys (a stale
//! dashboard tab can write them back) are only deleted.

use sp_core::audio_outputs::{
    MAX_NAME_LEN, OutputEntry, PROGRAM_RATE, RateChoice, VbanDest, VbanSampleFormat,
    validate_entry, wire_stream_name,
};
use sp_core::config::{DEFAULT_VBAN_STREAM_NAME, SETTING_AUDIO_OUTPUTS};
use sqlx::SqlitePool;

/// #210's keys, read once here and deleted.
const OLD_ENABLED: &str = "vban_enabled";
const OLD_STREAM: &str = "vban_stream_name";
const OLD_TARGETS: &str = "vban_targets";
/// #210's `VBAN_MAX_TARGETS`: the targets its sender used.
const OLD_MAX_TARGETS: usize = 8;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Migrated {
    pub entries: Vec<OutputEntry>,
    pub skipped: Vec<String>,
}

/// `host:port` at the last colon, a port 1..=65535.
pub fn split_target(spec: &str) -> Option<(String, u16)> {
    let (host, port) = spec.rsplit_once(':')?;
    let port = port.trim().parse::<u16>().ok().filter(|p| *p != 0)?;
    let host = host.trim();
    (!host.is_empty()).then(|| (host.to_string(), port))
}

/// #210's settings as entries (pure).
pub fn entries_from_vban(enabled: bool, stream_name: &str, targets: &str) -> Migrated {
    let name = stream_name.trim();
    let wire = wire_stream_name(if name.is_empty() { DEFAULT_VBAN_STREAM_NAME } else { name });
    let mut out = Migrated::default();
    let specs = targets.split(',').map(str::trim).filter(|s| !s.is_empty());
    for (i, spec) in specs.enumerate() {
        if i >= OLD_MAX_TARGETS {
            out.skipped.push(format!("{spec}: over the 8 targets #210 used"));
            continue;
        }
        let Some((host, port)) = split_target(spec) else {
            out.skipped.push(format!("{spec}: not host:port"));
            continue;
        };
        let id = format!("out-{}", out.entries.len() + 1);
        let label: String = spec.chars().take(MAX_NAME_LEN).collect();
        let mut entry = OutputEntry::vban(
            &id,
            &label,
            VbanDest { host, port, stream_name: wire.clone(), format: VbanSampleFormat::Int24 },
        );
        entry.enabled = enabled;
        entry.rate = RateChoice::Fixed(PROGRAM_RATE);
        match validate_entry(out.entries.len(), &entry) {
            Ok(()) => out.entries.push(entry),
            Err(e) => out.skipped.push(format!("{spec}: {e}")),
        }
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum MigrationOutcome {
    /// No old key: nothing to do (every start after the first).
    Nothing,
    Migrated(Migrated),
    /// A list was already stored: the old keys were only deleted.
    DroppedStale { keys: usize },
}

pub async fn migrate_vban_settings(pool: &SqlitePool) -> Result<MigrationOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings WHERE key IN (?, ?, ?, ?)")
            .bind(OLD_ENABLED)
            .bind(OLD_STREAM)
            .bind(OLD_TARGETS)
            .bind(SETTING_AUDIO_OUTPUTS)
            .fetch_all(&mut *tx)
            .await?;
    let get = |k: &str| rows.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let old = [OLD_ENABLED, OLD_STREAM, OLD_TARGETS]
        .iter()
        .filter(|k| get(k).is_some())
        .count();
    if old == 0 {
        return Ok(MigrationOutcome::Nothing);
    }
    let outcome = if get(SETTING_AUDIO_OUTPUTS).is_some() {
        MigrationOutcome::DroppedStale { keys: old }
    } else {
        let m = entries_from_vban(
            get(OLD_ENABLED).is_some_and(|v| v.trim() == "true"),
            get(OLD_STREAM).unwrap_or(""),
            get(OLD_TARGETS).unwrap_or(""),
        );
        let text = serde_json::to_string(&m.entries)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
            .bind(SETTING_AUDIO_OUTPUTS)
            .bind(text)
            .execute(&mut *tx)
            .await?;
        MigrationOutcome::Migrated(m)
    };
    sqlx::query("DELETE FROM settings WHERE key IN (?, ?, ?)")
        .bind(OLD_ENABLED)
        .bind(OLD_STREAM)
        .bind(OLD_TARGETS)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(outcome)
}

#[cfg(test)]
#[path = "audio_out_migrate_tests.rs"]
mod tests;
```

Check of the `a b:2` pin: `ok:1` is pushed first as `out-1`, so `a b:2` is validated as `out-2` at index 1 (shown 1-based: "entry 2"). As everywhere in this plan, re-derive every exact pin with a scratch model before the commit (`rust-workspace.md`).

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server audio_out_migrate` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the vban_* migration — SNV's two targets, wire name, 8-target cap, one transaction` then `feat(#233): audio_out_migrate — #210's keys become 48 kHz INT24 entries, once`.

### Task 1.4: the VBAN wire format per destination (rate index, sample format, packet geometry) — and the 0.72.0 bytes pinned

**Files:**
- Modify: `crates/sp-server/src/playback/vban_packet.rs` (add the format; keep every existing pub item, re-implemented on it)
- Create: `crates/sp-server/src/playback/vban_packet_tests_format.rs`, `crates/sp-server/src/playback/vban_packet_tests_legacy.rs`

**Interfaces:**
- Consumes: `sp_core::audio_outputs::VbanSampleFormat`.
- Produces: `vban_packet::{VBAN_MAX_FRAMES_PER_PACKET = 256, VBAN_FORMAT_BIT_INT16_PCM = 0x01, VBAN_FORMAT_BIT_FLOAT32_PCM = 0x04, INT16_FULL_SCALE: i16 = 32_767, sr_index_of(u32) -> Option<u8>, format_bit_of(VbanSampleFormat) -> u8, bytes_of(VbanSampleFormat) -> usize, max_packet_frames(bytes_per_sample) -> usize, largest_divisor_at_most(n, cap) -> usize, struct VbanFormat (PROGRAM, new(rate_hz, sample) -> Result<Self, String>, rate_hz, sample, sr_index, format_bit, bytes_per_sample, block_frames, packet_frames, packets_per_block, packet_len), write_header_as(fmt, out, name, counter), f32_to_int16, clean_f32, VbanEncoder::encode_into(&mut self, fmt, name, samples: Option<&[f32]>, out: &mut [u8]), empty_packets(fmt) -> Vec<u8>, packet_offset_in(fmt, k) -> i64, packet_send_at_in(fmt, due, latency, k) -> i64}`; unchanged and re-implemented: `write_header`, `encode_block`, `packet_offset_100ns`, `packet_send_at_100ns` (all = the `PROGRAM` format). `tests_legacy::legacy_encode_block` is `pub(crate)` (Task 1.6 reuses it).

- [ ] **Step 1: Write the failing tests.** First the oracle — `vban_packet_tests_legacy.rs`, today's encoder copied verbatim from `vban_packet.rs` at `c504b51f` (constants inlined; it must NOT call the code under test):

```rust
//! #233 lane 1: the #210 encoder exactly as it shipped in 0.72.0
//! (`vban_packet.rs` at c504b51f), kept as a TEST ORACLE. FOH's packets must
//! stay byte for byte what they were before the output list; this file never
//! calls the production encoder.

use super::VbanEncoder;
use super::VbanFormat;
use super::stream_name_bytes;

fn legacy_f32_to_int24(x: f32) -> i32 {
    let clamped = f64::from(x).clamp(-1.0, 1.0);
    (clamped * f64::from(8_388_607)).round() as i32
}

fn legacy_header(out: &mut [u8], name: &[u8; 16], counter: u32) {
    out[0..4].copy_from_slice(b"VBAN");
    out[4] = 3;
    out[5] = 199;
    out[6] = 1;
    out[7] = 0x02;
    out[8..24].copy_from_slice(name);
    out[24..28].copy_from_slice(&counter.to_le_bytes());
}

/// The 8 packets 0.72.0 sent for one block, advancing `counter` by 8.
pub(crate) fn legacy_encode_block(
    counter: &mut u32,
    name: &[u8; 16],
    samples: Option<&[f32]>,
) -> Vec<[u8; 1228]> {
    let samples = samples.filter(|s| s.len() == 3200);
    (0..8)
        .map(|k| {
            let mut packet = [0u8; 1228];
            legacy_header(&mut packet[..28], name, *counter);
            *counter = counter.wrapping_add(1);
            let payload = &mut packet[28..];
            match samples {
                Some(s) => {
                    let chunk = &s[k * 400..k * 400 + 400];
                    for (dst, &x) in payload.chunks_exact_mut(3).zip(chunk) {
                        let b = legacy_f32_to_int24(x).to_le_bytes();
                        dst.copy_from_slice(&[b[0], b[1], b[2]]);
                    }
                }
                None => payload.fill(0),
            }
            packet
        })
        .collect()
}

/// Blocks that exercise every INT24 branch: a ramp, clamps, NaN, tiny values,
/// a sine, a wrong length, silence.
pub(crate) fn oracle_blocks() -> Vec<Option<Vec<f32>>> {
    let ramp: Vec<f32> = (0..3200).map(|i| (i % 2000) as f32 / 1000.0 - 1.0).collect();
    let mut hot = vec![0.5f32; 3200];
    hot[0] = 1.2;
    hot[1] = -1.2;
    hot[2] = f32::NAN;
    hot[3] = 1e-9;
    hot[4] = -1e-9;
    hot[5] = 1.0;
    hot[6] = -1.0;
    let sine: Vec<f32> = (0..3200)
        .map(|i| (((i / 2) as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.9)
        .collect();
    vec![Some(ramp), Some(hot), Some(sine), Some(vec![0.25; 3199]), None]
}

#[test]
fn the_program_format_is_byte_for_byte_the_0_72_encoder() {
    let name = stream_name_bytes("sp-program");
    assert_eq!(&name, b"sp-program\0\0\0\0\0\0");
    for start in [0u32, u32::MAX - 3] {
        let mut legacy_counter = start;
        let mut enc = VbanEncoder::starting_at(start);
        for block in oracle_blocks() {
            let want: Vec<u8> = legacy_encode_block(&mut legacy_counter, &name, block.as_deref())
                .concat();
            let mut got = vec![0u8; 8 * 1228];
            enc.encode_into(VbanFormat::PROGRAM, &name, block.as_deref(), &mut got);
            assert_eq!(got, want, "start {start}");
        }
        assert_eq!(enc.next_counter(), legacy_counter);
    }
}

#[test]
fn encode_block_is_the_program_format() {
    let name = stream_name_bytes("sp-program");
    let mut a = VbanEncoder::default();
    let mut b = VbanEncoder::default();
    for block in oracle_blocks() {
        let mut packets = super::empty_block_packets();
        a.encode_block(&name, block.as_deref(), &mut packets);
        let mut flat = vec![0u8; 8 * 1228];
        b.encode_into(VbanFormat::PROGRAM, &name, block.as_deref(), &mut flat);
        assert_eq!(packets.as_flattened(), &flat[..]);
    }
}
```

Then the format tests — `vban_packet_tests_format.rs`:

```rust
//! #233: a VBAN destination's wire format — the SR index per rate, the
//! format bit per sample type, the packet geometry (the largest divisor of
//! `rate/30` within 256 frames and the 1436-byte payload), the encoders, the
//! schedule. Pins derived with a scratch model.

use super::tests::{parse_packet, ramp_block};
use super::*;
use sp_core::audio_outputs::VbanSampleFormat::{self, Float32, Int16, Int24};

struct Geometry {
    rate: u32,
    sample: VbanSampleFormat,
    frames: usize,
    packets: usize,
    len: usize,
}

const fn g(rate: u32, sample: VbanSampleFormat, frames: usize, packets: usize, len: usize) -> Geometry {
    Geometry { rate, sample, frames, packets, len }
}

const TABLE: [Geometry; 15] = [
    g(44_100, Int16, 245, 6, 1008),
    g(44_100, Int24, 210, 7, 1288),
    g(44_100, Float32, 147, 10, 1204),
    g(48_000, Int16, 200, 8, 828),
    g(48_000, Int24, 200, 8, 1228),
    g(48_000, Float32, 160, 10, 1308),
    g(88_200, Int16, 245, 12, 1008),
    g(88_200, Int24, 210, 14, 1288),
    g(88_200, Float32, 147, 20, 1204),
    g(96_000, Int16, 200, 16, 828),
    g(96_000, Int24, 200, 16, 1228),
    g(96_000, Float32, 160, 20, 1308),
    g(192_000, Int16, 256, 25, 1052),
    g(192_000, Int24, 200, 32, 1228),
    g(192_000, Float32, 160, 40, 1308),
];

#[test]
fn every_rate_and_format_has_its_packet_geometry() {
    for t in TABLE {
        let f = VbanFormat::new(t.rate, t.sample).unwrap();
        assert_eq!(f.block_frames() as u32 * 30, t.rate);
        assert_eq!(f.packet_frames(), t.frames, "{} {:?}", t.rate, t.sample);
        assert_eq!(f.packets_per_block(), t.packets, "{} {:?}", t.rate, t.sample);
        assert_eq!(f.packet_len(), t.len, "{} {:?}", t.rate, t.sample);
        assert!(f.packet_len() - VBAN_HEADER_LEN <= VBAN_DATA_MAX);
        assert!(f.packet_frames() <= VBAN_MAX_FRAMES_PER_PACKET);
    }
    assert_eq!(VbanFormat::new(48_000, Int24).unwrap(), VbanFormat::PROGRAM);
}

#[test]
fn the_sr_index_and_format_bit_follow_the_spec() {
    assert_eq!(sr_index_of(48_000), Some(3));
    assert_eq!(sr_index_of(96_000), Some(4));
    assert_eq!(sr_index_of(192_000), Some(5));
    assert_eq!(sr_index_of(44_100), Some(16));
    assert_eq!(sr_index_of(88_200), Some(17));
    assert_eq!(sr_index_of(32_000), None);
    assert!(VbanFormat::new(32_000, Int24).is_err());
    assert_eq!(format_bit_of(Int16), 0x01);
    assert_eq!(format_bit_of(Int24), 0x02);
    assert_eq!(format_bit_of(Float32), 0x04);
    assert_eq!((bytes_of(Int16), bytes_of(Int24), bytes_of(Float32)), (2, 3, 4));
}

#[test]
fn the_divisor_and_payload_caps_at_their_edges() {
    assert_eq!(max_packet_frames(2), 256, "1436/4 = 359, capped at 256");
    assert_eq!(max_packet_frames(3), 239);
    assert_eq!(max_packet_frames(4), 179);
    assert_eq!(largest_divisor_at_most(1600, 239), 200);
    assert_eq!(largest_divisor_at_most(1600, 200), 200);
    assert_eq!(largest_divisor_at_most(1600, 199), 160);
    assert_eq!(largest_divisor_at_most(7, 3), 1);
    assert_eq!(largest_divisor_at_most(6, 9), 6);
    assert_eq!(largest_divisor_at_most(0, 5), 1);
}

#[test]
fn headers_carry_each_destinations_rate_and_format() {
    let name = stream_name_bytes("x");
    let header = |rate, sample| {
        let mut h = [0u8; VBAN_HEADER_LEN];
        write_header_as(VbanFormat::new(rate, sample).unwrap(), &mut h, &name, 5);
        h
    };
    let h = header(96_000, Int24);
    assert_eq!((h[4], h[5], h[6], h[7]), (4, 199, 1, 0x02));
    let h = header(44_100, Int24);
    assert_eq!((h[4], h[5]), (16, 209));
    let h = header(88_200, Int16);
    assert_eq!((h[4], h[5], h[7]), (17, 244, 0x01));
    let h = header(192_000, Int16);
    assert_eq!((h[4], h[5], h[7]), (5, 255, 0x01));
    let h = header(48_000, Float32);
    assert_eq!((h[4], h[5], h[7]), (3, 159, 0x04));
    assert_eq!(&h[24..28], &5u32.to_le_bytes());
}

#[test]
fn int16_and_float32_samples_are_exact() {
    assert_eq!(f32_to_int16(1.0), 32_767);
    assert_eq!(f32_to_int16(-1.0), -32_767);
    assert_eq!(f32_to_int16(0.5), 16_384, "16383.5 rounds away from zero");
    assert_eq!(f32_to_int16(-0.25), -8_192);
    assert_eq!(f32_to_int16(2.0), 32_767);
    assert_eq!(f32_to_int16(f32::NAN), 0);
    assert_eq!(clean_f32(1.5), 1.0);
    assert_eq!(clean_f32(-1.5), -1.0);
    assert_eq!(clean_f32(-0.25), -0.25);
    assert_eq!(clean_f32(f32::NAN), 0.0);
    assert_eq!(clean_f32(f32::INFINITY), 0.0);
}

#[test]
fn a_96k_block_is_16_packets_of_200_frames_in_order() {
    let f = VbanFormat::new(96_000, Int24).unwrap();
    let block: Vec<f32> = ramp_block().iter().chain(ramp_block().iter()).copied().collect();
    let mut out = empty_packets(f);
    assert_eq!(out.len(), 16 * 1228);
    let mut enc = VbanEncoder::default();
    enc.encode_into(f, &stream_name_bytes("sp-e2e-96k"), Some(&block), &mut out);
    for (k, packet) in out.chunks_exact(1228).enumerate() {
        let p = parse_packet(packet);
        assert_eq!((p.format_sr, p.nbs, p.counter), (4, 199, k as u32));
        let want: Vec<i32> = block[k * 400..k * 400 + 400].iter().map(|&x| f32_to_int24(x)).collect();
        assert_eq!(p.samples, want);
    }
    assert_eq!(enc.next_counter(), 16);
}

#[test]
fn float32_and_int16_payloads_are_little_endian() {
    let f = VbanFormat::new(48_000, Float32).unwrap();
    let mut out = empty_packets(f);
    VbanEncoder::default().encode_into(f, &stream_name_bytes("x"), Some(&[0.25; 3200]), &mut out);
    assert_eq!(&out[28..32], &0.25f32.to_le_bytes());
    assert_eq!(out.len(), 10 * 1308);
    let f = VbanFormat::new(48_000, Int16).unwrap();
    let mut out = empty_packets(f);
    VbanEncoder::default().encode_into(f, &stream_name_bytes("x"), Some(&[0.5; 3200]), &mut out);
    assert_eq!(&out[28..30], &16_384i16.to_le_bytes());
}

#[test]
fn a_wrong_length_block_is_silence_at_any_rate() {
    let f = VbanFormat::new(96_000, Int24).unwrap();
    let mut out = vec![0xAAu8; f.packet_len() * f.packets_per_block()];
    VbanEncoder::default().encode_into(f, &stream_name_bytes("x"), Some(&[0.5; 3200]), &mut out);
    assert!(out.chunks_exact(1228).all(|p| p[28..].iter().all(|&b| b == 0)));
}

#[test]
fn packets_are_spread_evenly_over_the_slot() {
    let f = VbanFormat::new(96_000, Int24).unwrap();
    let offsets: Vec<i64> = (0..=16).map(|k| packet_offset_in(f, k)).collect();
    assert_eq!(&offsets[..4], &[0, 20_833, 41_666, 62_500]);
    assert_eq!(offsets[16], 333_333, "one grid slot");
    for k in 0..=8 {
        assert_eq!(packet_offset_in(VbanFormat::PROGRAM, k), packet_offset_100ns(k));
    }
    assert_eq!(packet_send_at_in(f, 1_000, 7, 2), 1_000 + 7 + 41_666);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server vban_packet` — Expected now: FAIL to compile (`VbanFormat` missing).

- [ ] **Step 3: Implement** in `vban_packet.rs`. Add `use sp_core::audio_outputs::VbanSampleFormat;`. Module doc gains a paragraph: "#233: every destination has its own format (`VbanFormat`): the SR index of its rate (48 kHz = 3, 96 = 4, 192 = 5, 44.1 = 16, 88.2 = 17), its sample type (INT16 0x01, INT24 0x02, FLOAT32 0x04) and its packet geometry: the largest divisor of `rate/30` frames within the spec's 256 frames and 1436-byte payload, so one boundary's block is whole packets, sent evenly over the slot. `PROGRAM` (48 kHz INT24, 8 × 200) is #210's format, byte for byte (`vban_packet_tests_legacy.rs`)." Then:

```rust
/// #233: the spec's frames-per-packet maximum (`format_nbs` = samples − 1, a byte).
pub const VBAN_MAX_FRAMES_PER_PACKET: usize = 256;
/// `format_bit`: INT16 (index 1) and FLOAT32 (index 4), codec PCM.
pub const VBAN_FORMAT_BIT_INT16_PCM: u8 = 0x01;
pub const VBAN_FORMAT_BIT_FLOAT32_PCM: u8 = 0x04;
/// Full scale of the INT16 conversion: `±1.0 → ±32767` (symmetric, like INT24).
pub const INT16_FULL_SCALE: i16 = 32_767;

/// #233: the SR index of a rate SongPlayer sends (spec rev. 13, p. 8).
pub fn sr_index_of(rate_hz: u32) -> Option<u8> {
    match rate_hz {
        48_000 => Some(VBAN_FORMAT_SR_48K_AUDIO),
        96_000 => Some(4),
        192_000 => Some(5),
        44_100 => Some(16),
        88_200 => Some(17),
        _ => None,
    }
}

pub fn format_bit_of(sample: VbanSampleFormat) -> u8 {
    match sample {
        VbanSampleFormat::Int16 => VBAN_FORMAT_BIT_INT16_PCM,
        VbanSampleFormat::Int24 => VBAN_FORMAT_BIT_INT24_PCM,
        VbanSampleFormat::Float32 => VBAN_FORMAT_BIT_FLOAT32_PCM,
    }
}

pub fn bytes_of(sample: VbanSampleFormat) -> usize {
    match sample {
        VbanSampleFormat::Int16 => 2,
        VbanSampleFormat::Int24 => VBAN_BYTES_PER_SAMPLE,
        VbanSampleFormat::Float32 => 4,
    }
}

/// The most stereo frames one packet may carry at `bytes_per_sample`.
pub fn max_packet_frames(bytes_per_sample: usize) -> usize {
    (VBAN_DATA_MAX / (VBAN_CHANNELS * bytes_per_sample)).min(VBAN_MAX_FRAMES_PER_PACKET)
}

/// The largest divisor of `n` that is at most `cap` (1 when there is none).
pub fn largest_divisor_at_most(n: usize, cap: usize) -> usize {
    (1..=cap.min(n)).rev().find(|d| n % d == 0).unwrap_or(1)
}

/// #233: one VBAN destination's wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VbanFormat {
    rate_hz: u32,
    sr_index: u8,
    sample: VbanSampleFormat,
}

impl VbanFormat {
    /// #210's format (FOH's): 48 kHz INT24, 8 packets of 200 frames.
    pub const PROGRAM: Self = Self {
        rate_hz: 48_000,
        sr_index: VBAN_FORMAT_SR_48K_AUDIO,
        sample: VbanSampleFormat::Int24,
    };

    /// A destination's format; a rate with no SR index is refused.
    pub fn new(rate_hz: u32, sample: VbanSampleFormat) -> Result<Self, String> {
        let sr_index = sr_index_of(rate_hz).ok_or_else(|| format!("VBAN carries no {rate_hz} Hz"))?;
        Ok(Self { rate_hz, sr_index, sample })
    }

    pub fn rate_hz(self) -> u32 {
        self.rate_hz
    }

    pub fn sample(self) -> VbanSampleFormat {
        self.sample
    }

    pub fn sr_index(self) -> u8 {
        self.sr_index
    }

    pub fn format_bit(self) -> u8 {
        format_bit_of(self.sample)
    }

    pub fn bytes_per_sample(self) -> usize {
        bytes_of(self.sample)
    }

    /// Stereo frames per program boundary at this rate (`rate / 30`).
    pub fn block_frames(self) -> usize {
        (i64::from(self.rate_hz) / GENLOCK_GRID_FPS) as usize
    }

    pub fn packet_frames(self) -> usize {
        largest_divisor_at_most(self.block_frames(), max_packet_frames(self.bytes_per_sample()))
    }

    pub fn packets_per_block(self) -> usize {
        self.block_frames() / self.packet_frames()
    }

    pub fn packet_len(self) -> usize {
        VBAN_HEADER_LEN + self.packet_frames() * VBAN_CHANNELS * self.bytes_per_sample()
    }
}

/// One f32 sample as INT16: clamped, scaled by [`INT16_FULL_SCALE`], rounded
/// half away from zero; NaN is silence.
pub fn f32_to_int16(x: f32) -> i16 {
    (f64::from(x).clamp(-1.0, 1.0) * f64::from(INT16_FULL_SCALE)).round() as i16
}

/// One f32 sample as VBAN FLOAT32 carries it: clamped to ±1, a non-finite
/// value as silence.
pub fn clean_f32(x: f32) -> f32 {
    if x.is_finite() { x.clamp(-1.0, 1.0) } else { 0.0 }
}

fn write_sample(sample: VbanSampleFormat, dst: &mut [u8], x: f32) {
    match sample {
        VbanSampleFormat::Int16 => dst.copy_from_slice(&f32_to_int16(x).to_le_bytes()),
        VbanSampleFormat::Int24 => dst.copy_from_slice(&int24_le(f32_to_int24(x))),
        VbanSampleFormat::Float32 => dst.copy_from_slice(&clean_f32(x).to_le_bytes()),
    }
}

/// The 28-byte header of one packet in `fmt`.
pub fn write_header_as(
    fmt: VbanFormat,
    out: &mut [u8],
    name: &[u8; VBAN_STREAM_NAME_LEN],
    counter: u32,
) {
    out[0..4].copy_from_slice(&VBAN_MAGIC);
    out[4] = fmt.sr_index();
    out[5] = (fmt.packet_frames() - 1) as u8;
    out[6] = (VBAN_CHANNELS - 1) as u8;
    out[7] = fmt.format_bit();
    out[8..24].copy_from_slice(name);
    out[24..28].copy_from_slice(&counter.to_le_bytes());
}

/// A zeroed buffer for one block's packets in `fmt`.
pub fn empty_packets(fmt: VbanFormat) -> Vec<u8> {
    vec![0; fmt.packet_len() * fmt.packets_per_block()]
}

/// Offset of packet `k` from the block's first packet (100 ns): `k` slots of
/// `1 / (30 · packets)` s, floored.
pub fn packet_offset_in(fmt: VbanFormat, k: usize) -> i64 {
    k as i64 * UNITS_PER_SECOND / (fmt.packets_per_block() as i64 * GENLOCK_GRID_FPS)
}

pub fn packet_send_at_in(fmt: VbanFormat, due_100ns: i64, latency_100ns: i64, k: usize) -> i64 {
    due_100ns + latency_100ns + packet_offset_in(fmt, k)
}
```

Re-implement the existing items on it (same signatures, same docs + "= the `PROGRAM` format"): `write_header(out, name, counter)` → `write_header_as(VbanFormat::PROGRAM, out, name, counter)`; `packet_offset_100ns(k)` → `packet_offset_in(VbanFormat::PROGRAM, k)`; `packet_send_at_100ns(due, latency, k)` → `packet_send_at_in(VbanFormat::PROGRAM, due, latency, k)`. In `VbanEncoder`, add `encode_into` and make `encode_block` call it:

```rust
    /// #233: encode one block in `fmt` into `out` (`fmt.packets_per_block()`
    /// packets of `fmt.packet_len()` bytes): packet `k` carries interleaved
    /// samples `k·n .. (k+1)·n` (`n` = packet frames × 2), or silence when
    /// `samples` is `None` or not exactly one block at `fmt`'s rate.
    pub fn encode_into(
        &mut self,
        fmt: VbanFormat,
        name: &[u8; VBAN_STREAM_NAME_LEN],
        samples: Option<&[f32]>,
        out: &mut [u8],
    ) {
        let per_packet = fmt.packet_frames() * VBAN_CHANNELS;
        let bytes = fmt.bytes_per_sample();
        let samples = samples.filter(|s| s.len() == fmt.block_frames() * VBAN_CHANNELS);
        let packets = out.chunks_exact_mut(fmt.packet_len()).take(fmt.packets_per_block());
        for (k, packet) in packets.enumerate() {
            write_header_as(fmt, &mut packet[..VBAN_HEADER_LEN], name, self.counter);
            self.counter = self.counter.wrapping_add(1);
            let payload = &mut packet[VBAN_HEADER_LEN..];
            match samples {
                Some(s) => {
                    let first = k * per_packet;
                    let chunk = &s[first..first + per_packet];
                    for (dst, &x) in payload.chunks_exact_mut(bytes).zip(chunk) {
                        write_sample(fmt.sample(), dst, x);
                    }
                }
                None => payload.fill(0),
            }
        }
    }

    /// Encode one 48 kHz INT24 block (the `PROGRAM` format) into `out`.
    pub fn encode_block(
        &mut self,
        name: &[u8; VBAN_STREAM_NAME_LEN],
        samples: Option<&[f32]>,
        out: &mut VbanBlockPackets,
    ) {
        self.encode_into(VbanFormat::PROGRAM, name, samples, out.as_flattened_mut());
    }
```

and the two new test hooks after the existing one:

```rust
#[cfg(test)]
#[path = "vban_packet_tests_format.rs"]
mod tests_format;
#[cfg(test)]
#[path = "vban_packet_tests_legacy.rs"]
pub(crate) mod tests_legacy;
```

`VBAN_PACKETS_PER_SECOND` stays (its existing test pins it); nothing else calls it now.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server vban_packet` — Expected: PASS, including the unchanged `vban_packet_tests.rs`.

- [ ] **Step 5: Commit** — `test(#233): VBAN per-destination wire format + the 0.72.0 encoder as a byte oracle` then `feat(#233): VbanFormat — rate index, sample format, packet geometry; #210's format byte for byte`.

### Task 1.5: the VBAN fixed-ratio converter (rubato `Fft`)

**Files:**
- Modify: `crates/sp-server/Cargo.toml` (dependency), `Cargo.lock` (resolved, see Step 3), `crates/sp-server/src/playback/mod.rs` (`pub mod vban_rate; // #233: a VBAN destination's fixed-ratio rate conversion (rubato Fft)`)
- Create: `crates/sp-server/src/playback/vban_rate.rs`, `crates/sp-server/src/playback/vban_rate_tests.rs`

**Interfaces:**
- Consumes: `vban_packet::{VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS}`, `sp_core::audio_outputs::PROGRAM_RATE`.
- Produces: `vban_rate::{struct VbanRateConverter, VbanRateConverter::new(rate_hz: u32) -> Self, convert<'a>(&'a mut self, block: Option<&'a [f32]>) -> Option<&'a [f32]>, delay_frames(&self) -> usize, fft_delay_frames(rate_hz: u32) -> usize, failed(&self) -> Option<&str>}`.

- [ ] **Step 1: Write the failing tests** — `vban_rate_tests.rs`:

```rust
//! #233: a VBAN destination's rate conversion — 48 kHz passes untouched
//! (FOH's bytes), every other rate gives exactly rate/30 frames per block,
//! keeps a tone's pitch, and reports its delay.

use super::*;

fn tone(blocks: usize) -> Vec<Vec<f32>> {
    (0..blocks)
        .map(|b| {
            (0..1600)
                .flat_map(|i| {
                    let n = (b * 1600 + i) as f32;
                    let x = (n * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.5;
                    [x, x]
                })
                .collect()
        })
        .collect()
}

#[test]
fn the_48k_destination_is_passed_through_untouched() {
    let mut c = VbanRateConverter::new(48_000);
    let block = vec![0.3f32; 3200];
    let out = c.convert(Some(&block)).unwrap();
    assert!(std::ptr::eq(out.as_ptr(), block.as_ptr()), "the same samples, no copy");
    assert_eq!(c.convert(None), None, "silence stays the encoder's zeros");
    assert_eq!(c.delay_frames(), 0);
    assert_eq!(c.failed(), None);
}

#[test]
fn every_other_rate_gives_exactly_rate_over_30_frames_per_block() {
    for rate in [44_100u32, 88_200, 96_000, 192_000] {
        let mut c = VbanRateConverter::new(rate);
        assert_eq!(c.failed(), None, "{rate}");
        for block in tone(30) {
            let out = c.convert(Some(&block)).unwrap();
            assert_eq!(out.len(), rate as usize / 30 * 2, "{rate}");
        }
        let silence = c.convert(None).unwrap().len();
        assert_eq!(silence, rate as usize / 30 * 2, "silence goes through the filter too");
        assert_eq!(c.delay_frames(), fft_delay_frames(rate));
        assert_eq!(fft_delay_frames(rate), rate as usize / 60, "half the block FFT");
    }
    assert_eq!(fft_delay_frames(48_000), 0);
}

#[test]
fn a_1khz_tone_stays_1khz_at_96k() {
    let mut c = VbanRateConverter::new(96_000);
    let mut left = Vec::new();
    for (b, block) in tone(30).iter().enumerate() {
        let out = c.convert(Some(block)).unwrap();
        if b >= 3 {
            left.extend(out.iter().step_by(2).copied());
        }
    }
    let rising = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    assert!((898..=902).contains(&rising), "{rising} rising zero crossings in 0.9 s");
}

#[test]
fn silence_in_is_silence_out() {
    let mut c = VbanRateConverter::new(96_000);
    for _ in 0..5 {
        let out = c.convert(Some(&[0.0; 3200])).unwrap();
        assert!(out.iter().all(|x| x.abs() < 1e-6));
    }
}

#[test]
fn a_converter_rubato_refuses_sends_silence_and_says_why() {
    let mut c = VbanRateConverter::new(0);
    assert!(c.failed().is_some());
    assert_eq!(c.convert(Some(&[0.5; 3200])), None);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server vban_rate` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement.** `crates/sp-server/Cargo.toml`, after `if-addrs`:

```toml
# #233: the outputs' sample-rate conversion — rubato's synchronous `Fft`
# (a VBAN destination's fixed ratio) and, from lane 2, its `Async` sinc (the
# ASIO drift servo's ratio). Pure Rust, MIT OR Apache-2.0, pinned exactly.
rubato = "=5.0.1"
```

Lockfile (Tier-0-allowed, no compile): `cargo metadata --format-version 1 > /dev/null`, then `git diff --stat Cargo.lock` and `git diff Cargo.lock | grep '^-version'` — only ADDED `[[package]]` blocks (rubato 5.0.1, audioadapter 5.0.0, audioadapter-buffers 5.x, realfft, rustfft, num-complex, num-integer, primal-check, strength_reduce, transpose, visibility, windowfunctions, …) and the new line in sp-server's `dependencies` list may appear. If any existing package's version changed, `git checkout -- Cargo.lock` and commit without it (CI builds without `--locked`, `rust-workspace.md`), and say so in the commit message.

`vban_rate.rs`:

```rust
//! #233: a VBAN destination's rate conversion. The program is 48 kHz; a
//! destination at another rate gets each boundary's block through rubato's
//! synchronous FFT resampler with BOTH sides fixed: one 1600-frame block in,
//! exactly `rate / 30` frames out (the packet schedule needs whole packets per
//! boundary; rubato's `Async` output varies by a frame). The 48 kHz
//! destination (FOH's) is passed through untouched: no copy, no filter, the
//! #210 bytes. A silent block still goes through the filter (its state stays
//! continuous). The delay is half the block FFT (`rate / 60` frames, 8.3 ms).
//! If rubato refuses the converter (never for a supported rate) the
//! destination sends silence and says why (`failed`).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use sp_core::audio_outputs::PROGRAM_RATE;
use sp_core::genlock::GENLOCK_GRID_FPS;

use crate::playback::vban_packet::{VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS};

/// The converter's delay at `rate_hz`, frames of the destination's rate.
pub fn fft_delay_frames(rate_hz: u32) -> usize {
    if rate_hz == PROGRAM_RATE {
        0
    } else {
        rate_hz as usize / 60
    }
}

pub struct VbanRateConverter {
    fft: Option<Fft<f32>>,
    out: Vec<f32>,
    zeros: Vec<f32>,
    out_frames: usize,
    failed: Option<String>,
}

impl VbanRateConverter {
    pub fn new(rate_hz: u32) -> Self {
        let bypass = Self { fft: None, out: Vec::new(), zeros: Vec::new(), out_frames: 0, failed: None };
        if rate_hz == PROGRAM_RATE {
            return bypass;
        }
        let out_frames = (i64::from(rate_hz) / GENLOCK_GRID_FPS) as usize;
        let built = Fft::<f32>::new(
            PROGRAM_RATE as usize,
            rate_hz as usize,
            VBAN_BLOCK_FRAMES,
            VBAN_CHANNELS,
            FixedSync::Both,
        )
        .map_err(|e| e.to_string())
        .and_then(|fft| {
            if fft.input_frames_next() == VBAN_BLOCK_FRAMES && fft.output_frames_next() == out_frames {
                Ok(fft)
            } else {
                Err(format!(
                    "the {rate_hz} Hz converter takes {} frames for {}",
                    fft.input_frames_next(),
                    fft.output_frames_next()
                ))
            }
        });
        match built {
            Ok(fft) => Self {
                fft: Some(fft),
                out: vec![0.0; out_frames * VBAN_CHANNELS],
                zeros: vec![0.0; VBAN_BLOCK_SAMPLES],
                out_frames,
                failed: None,
            },
            Err(e) => Self { failed: Some(e), ..bypass },
        }
    }

    /// One boundary's block (`None` = silence) at the destination's rate:
    /// the block itself at 48 kHz, else the converted frames (`None` only
    /// when the converter failed).
    pub fn convert<'a>(&'a mut self, block: Option<&'a [f32]>) -> Option<&'a [f32]> {
        if self.failed.is_some() {
            return None;
        }
        let Self { fft, out, zeros, out_frames, .. } = self;
        let Some(fft) = fft.as_mut() else {
            return block;
        };
        let input = block.filter(|b| b.len() == VBAN_BLOCK_SAMPLES).unwrap_or(zeros.as_slice());
        let adapter_in = InterleavedSlice::new(input, VBAN_CHANNELS, VBAN_BLOCK_FRAMES).ok()?;
        let mut adapter_out = InterleavedSlice::new_mut(&mut out[..], VBAN_CHANNELS, *out_frames).ok()?;
        fft.process_into_buffer(&adapter_in, &mut adapter_out, None).ok()?;
        Some(&out[..])
    }

    pub fn delay_frames(&self) -> usize {
        self.fft.as_ref().map_or(0, |f| f.output_delay())
    }

    /// Why the converter could not be built (the destination sends silence).
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }
}

#[cfg(test)]
#[path = "vban_rate_tests.rs"]
mod tests;
```

(`Self { failed: Some(e), ..bypass }` is a same-type update: fine. `fft.as_mut()` borrows the destructured field; `adapter_out` borrows `out` mutably and is dropped before `Some(&out[..])`: put the adapters in a block if the borrow checker asks.)

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server vban_rate`; the Windows job builds rubato too — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): VBAN rate conversion — 48 kHz untouched, exact frames, pitch, delay` then `feat(#233): vban_rate — rubato Fft, both sides fixed, 48 kHz bypass` (with `Cargo.toml` + `Cargo.lock`).

### Task 1.6: a VBAN output per destination (format, converter, delay, one target)

**Files:**
- Create: `crates/sp-server/src/playback/audio_out_block.rs`, `crates/sp-server/src/playback/vban_out_tests_dest.rs`
- Modify: `crates/sp-server/src/playback/vban_out.rs`, `crates/sp-server/src/playback/vban_out_tests.rs`, `crates/sp-server/src/playback/mod.rs` (`pub mod audio_out_block; // #233: one program boundary's audio block for the outputs`); every file that names `VbanBlock` (rename to `ProgramBlock`, import from `audio_out_block`): `program_output.rs`, `program_output_tests_order.rs`, `program_output_tests_limit.rs`, `program_output_tests_max.rs`, `vban_out_tests_regrid.rs`, `api/program_tests.rs` (Task 1.9 rewrites its VBAN tests anyway)

**Interfaces:**
- Consumes: `VbanFormat`, `packet_send_at_in`, `empty_packets`, `VbanRateConverter`, `OutputEntry`, `VbanDest`, `effective_rate`.
- Produces:
  - `audio_out_block::{struct ProgramBlock { due_100ns: i64, samples: Option<Arc<[f32]>>, substituted: bool }, ProgramBlock::{silence(due), copied(due, &[AudioFrame])}, is_program_block(&AudioFrame) -> bool}` (moved from `vban_out`; `copied` makes the one `Arc<[f32]>` copy of the boundary);
  - `vban_out::{VbanOut::for_destination(format: VbanFormat, delay_100ns: i64) -> Self, VbanOut::for_entry(&OutputEntry, network_rate: u32) -> Result<Self, String>, VbanOut::{format, delay_100ns, bound}, queue_bound(delay_100ns: i64) -> usize, SLOT_100NS: u64 = 333_333, VbanConfig::for_dest(&VbanDest, enabled: bool, targets: Vec<VbanTarget>) -> Self, target_spec(&VbanDest) -> String, async resolve_dest(dest: VbanDest, enabled: bool, previous: Vec<VbanTarget>) -> VbanConfig, VbanSender::for_out(&VbanOut) -> Self, VbanStatus.blocks_sent: u64, spawn_vban_thread(out: Arc<VbanOut>, id: String)}` (Windows);
  - REMOVED: `VbanSettings`, `load_vban_settings`, `resolve_config`, `run_vban_config_task`, `VbanBlock` (→ `ProgramBlock`), `VbanConfig::new(&VbanSettings, …)`. `VBAN_MAX_TARGETS` stays only as the VBAN-entry cap's documentation alias: `pub const VBAN_MAX_TARGETS: usize = sp_core::audio_outputs::MAX_VBAN_OUTPUTS;`.

- [ ] **Step 1: Write the failing tests** — `vban_out_tests_dest.rs` (wire with `#[cfg(test)] #[path = "vban_out_tests_dest.rs"] mod tests_dest;` after the existing hooks):

```rust
//! #233: a VBAN output per destination — its rate, format and delay on the
//! wire and on the schedule; the queue holds the delay; a migrated FOH entry
//! sends exactly the 0.72.0 datagrams.

use super::tests::{FakeClock, RecordingSink, active_config};
use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_migrate::entries_from_vban;
use crate::playback::vban_packet::tests::parse_packet;
use crate::playback::vban_packet::tests_legacy::{legacy_encode_block, oracle_blocks};
use crate::playback::vban_packet::{VBAN_SEND_LATENCY_100NS, VbanFormat, stream_name_bytes};
use sp_core::audio_outputs::{RateChoice, VbanSampleFormat};
use std::sync::Arc;

const D: i64 = 17_900_000_000_000_000;
const L: i64 = VBAN_SEND_LATENCY_100NS;

fn block(due: i64, samples: Option<Vec<f32>>) -> ProgramBlock {
    ProgramBlock { due_100ns: due, samples: samples.map(Into::into), substituted: false }
}

fn send(out: &VbanOut, blocks: &[ProgramBlock]) -> Vec<(i64, Vec<u8>)> {
    let mut clock = FakeClock::at(D);
    let mut sink = RecordingSink::on(&clock);
    let mut sender = VbanSender::for_out(out);
    for b in blocks {
        sender.send_block(out, b, &mut sink, &mut clock);
    }
    sink.sent.into_iter().map(|(t, _, p)| (t, p)).collect()
}

#[test]
fn a_migrated_foh_entry_sends_the_0_72_datagrams() {
    let entry = entries_from_vban(true, "sp-program", "fohabl.lan:6980").entries.remove(0);
    let out = VbanOut::for_entry(&entry, 96_000).expect("a FOH output");
    assert_eq!(out.format(), VbanFormat::PROGRAM, "fixed 48 kHz INT24 on a 96 kHz network");
    assert_eq!(out.delay_100ns(), 0);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let blocks: Vec<ProgramBlock> = oracle_blocks()
        .into_iter()
        .enumerate()
        .map(|(i, s)| block(D + i as i64 * 333_333, s))
        .collect();
    let sent = send(&out, &blocks);
    let name = stream_name_bytes("sp-program");
    let mut counter = 0u32;
    let want: Vec<[u8; 1228]> = blocks
        .iter()
        .flat_map(|b| legacy_encode_block(&mut counter, &name, b.samples.as_deref()))
        .collect();
    assert_eq!(sent.len(), want.len());
    for (i, ((_, got), want)) in sent.iter().zip(&want).enumerate() {
        assert_eq!(&got[..], &want[..], "datagram {i}");
    }
    let firsts: Vec<i64> = sent.iter().step_by(8).map(|(t, _)| *t).collect();
    assert_eq!(firsts[0], D + L, "the #210 schedule");
}

#[test]
fn a_96k_destination_sends_16_packets_a_slot_with_index_4() {
    let out = VbanOut::for_destination(VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap(), 0);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let sent = send(&out, &[block(D, Some(vec![0.25; 3200]))]);
    assert_eq!(sent.len(), 16);
    let times: Vec<i64> = sent.iter().map(|(t, _)| *t - D - L).collect();
    assert_eq!(&times[..4], &[0, 20_833, 41_666, 62_500]);
    for (k, (_, p)) in sent.iter().enumerate() {
        let parsed = parse_packet(p);
        assert_eq!((parsed.format_sr, parsed.nbs, parsed.counter), (4, 199, k as u32));
    }
    assert_eq!(out.status().blocks_sent, 1);
}

#[test]
fn a_delay_moves_every_packet_by_the_delay() {
    let out = VbanOut::for_destination(VbanFormat::PROGRAM, 2_500_000);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let sent = send(&out, &[block(D, Some(vec![0.25; 3200]))]);
    assert_eq!(sent[0].0, D + L + 2_500_000);
    assert_eq!(sent[1].0, D + L + 2_500_000 + 41_666);
}

#[test]
fn the_queue_holds_the_delay() {
    assert_eq!(SLOT_100NS as i64, sp_core::genlock::UNITS_PER_SECOND / sp_core::genlock::GENLOCK_GRID_FPS);
    assert_eq!(queue_bound(0), VBAN_QUEUE_BOUND);
    assert_eq!(queue_bound(1), VBAN_QUEUE_BOUND + 1);
    assert_eq!(queue_bound(333_333), VBAN_QUEUE_BOUND + 1);
    assert_eq!(queue_bound(333_334), VBAN_QUEUE_BOUND + 2);
    assert_eq!(queue_bound(-5), VBAN_QUEUE_BOUND);
    let out = VbanOut::for_destination(VbanFormat::PROGRAM, 20_000_000);
    assert_eq!(out.bound(), VBAN_QUEUE_BOUND + 61);
    for i in 0..out.bound() as i64 {
        out.push(ProgramBlock::silence(D + i));
    }
    assert_eq!(out.status().blocks_dropped, 0, "2 s of delay is all queued");
    out.push(ProgramBlock::silence(D + 999));
    assert_eq!(out.status().blocks_dropped, 1);
}

#[test]
fn an_entry_at_the_network_rate_follows_the_network() {
    let mut entry = entries_from_vban(true, "", "h:1").entries.remove(0);
    entry.rate = RateChoice::Network;
    entry.delay_ms = 40;
    let out = VbanOut::for_entry(&entry, 96_000).unwrap();
    assert_eq!(out.format().rate_hz(), 96_000);
    assert_eq!(out.delay_100ns(), 400_000);
}

#[tokio::test]
async fn resolve_dest_builds_one_target_and_disabled_sends_nothing() {
    let dest = sp_core::audio_outputs::VbanDest {
        host: "127.0.0.1".into(),
        port: 6980,
        stream_name: "foh-test".into(),
        format: VbanSampleFormat::Int24,
    };
    let cfg = resolve_dest(dest.clone(), true, Vec::new()).await;
    assert!(cfg.is_active());
    assert_eq!(cfg.stream_name, "foh-test");
    assert_eq!(cfg.targets.len(), 1);
    assert_eq!(cfg.targets[0].spec, "127.0.0.1:6980");
    assert_eq!(target_spec(&dest), "127.0.0.1:6980");
    let off = resolve_dest(dest, false, cfg.targets.clone()).await;
    assert!(!off.is_active(), "disabled sends nothing");
}

#[test]
fn the_config_carries_the_wire_name_and_the_status_its_target() {
    let dest = sp_core::audio_outputs::VbanDest {
        host: "fohabl.lan".into(),
        port: 6980,
        stream_name: "é-a-very-long-stream".into(),
        format: VbanSampleFormat::Int24,
    };
    let cfg = VbanConfig::for_dest(
        &dest,
        true,
        vec![VbanTarget {
            spec: "fohabl.lan:6980".into(),
            addr: Some("10.77.7.30:6980".parse().unwrap()),
            error: None,
        }],
    );
    assert_eq!(cfg.stream_name, "_-a-very-long-st", "what goes on the wire");
    let out = VbanOut::new();
    out.set_config(cfg);
    let st = out.status();
    assert!(st.enabled);
    assert_eq!(st.targets[0].target, "fohabl.lan:6980");
    assert_eq!(st.targets[0].addr.as_deref(), Some("10.77.7.30:6980"));
    let def = VbanConfig::default();
    assert!(!def.enabled);
    assert_eq!(def.stream_name, "sp-program");
}

#[test]
fn a_block_shares_its_samples_between_outputs() {
    let frame = sp_ndi::AudioFrame { data: vec![0.5; 3200], channels: 2, sample_rate: 48_000, timecode_100ns: None };
    let b = ProgramBlock::copied(D, &[frame]);
    let c = b.clone();
    assert!(Arc::ptr_eq(b.samples.as_ref().unwrap(), c.samples.as_ref().unwrap()));
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server vban_out` — Expected now: FAIL to compile (`VbanOut::for_entry`, `audio_out_block` missing).

- [ ] **Step 3: Implement.**

`audio_out_block.rs` — the old `VbanBlock` + `is_program_block`, moved out of `vban_out.rs` and renamed (the doc keeps #210's text):

```rust
//! #233: one program boundary's audio as every output receives it (#210's
//! `VbanBlock`, renamed and shared). `ProgramOutput::serve` makes ONE copy of
//! the limited block (`copied`: the NDI submit still borrows the pair after
//! the hand-off) and the fan-out (`audio_out.rs`) hands each output an `Arc`
//! of it. Anything that is not one 48 kHz stereo 1600-frame block is sent as
//! silence and marked `substituted`.

use std::sync::Arc;

use sp_ndi::AudioFrame;

use crate::playback::vban_packet::{VBAN_BLOCK_SAMPLES, VBAN_CHANNELS, VBAN_SAMPLE_RATE_HZ};

#[derive(Clone, Debug, PartialEq)]
pub struct ProgramBlock {
    /// The boundary the block belongs to (the pair's video stamp, 100 ns).
    pub due_100ns: i64,
    /// 3200 interleaved stereo samples; `None` = silence.
    pub samples: Option<Arc<[f32]>>,
    /// The pair's audio was not one program block and is sent as silence.
    pub substituted: bool,
}

impl ProgramBlock {
    pub fn silence(due_100ns: i64) -> Self {
        Self { due_100ns, samples: None, substituted: false }
    }

    /// A pair's audio, COPIED once into a shared block.
    pub fn copied(due_100ns: i64, frames: &[AudioFrame]) -> Self {
        let samples: Option<Arc<[f32]>> = match frames {
            [frame] if is_program_block(frame) => Some(Arc::from(frame.data.as_slice())),
            _ => None,
        };
        Self { due_100ns, substituted: samples.is_none(), samples }
    }
}

/// `frame` is one program audio block: 48 kHz, stereo, 1600 frames.
pub fn is_program_block(frame: &AudioFrame) -> bool {
    frame.channels as usize == VBAN_CHANNELS
        && i64::from(frame.sample_rate) == VBAN_SAMPLE_RATE_HZ
        && frame.data.len() == VBAN_BLOCK_SAMPLES
}
```

`vban_out.rs` (module doc: replace the settings paragraph with "#233: one `VbanOut` per VBAN entry of the output list (`audio_out_task.rs` builds it from the entry, resolves its target and spawns its thread): its own format (`VbanFormat`: rate index, sample type, packet geometry), rate converter (`vban_rate.rs`, bypassed at 48 kHz), delay (added to the send latency; the queue bound grows with it) and ONE target. The settings task re-resolves its DNS every `VBAN_RESOLVE_EVERY`."). The changes, in order:
1. Delete `VbanBlock`, `is_program_block` (moved), `VbanSettings` + impl, `load_vban_settings`, `resolve_config`, `run_vban_config_task`, and the `sp_core::config::{SETTING_VBAN_*}` imports. Keep `DEFAULT_VBAN_STREAM_NAME`.
2. Add:

```rust
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_packet::{VbanFormat, empty_packets, packet_send_at_in};
use crate::playback::vban_rate::VbanRateConverter;
use sp_core::audio_outputs::{OutputEntry, VbanDest, effective_rate};

/// #233: the VBAN-entry cap (one paced thread each; #210's target cap).
pub const VBAN_MAX_TARGETS: usize = sp_core::audio_outputs::MAX_VBAN_OUTPUTS;

/// One grid slot in 100 ns (a literal: `UNITS_PER_SECOND / GENLOCK_GRID_FPS`,
/// pinned by `the_queue_holds_the_delay`).
pub const SLOT_100NS: u64 = 333_333;

/// #233: the queue bound of an output delayed by `delay_100ns`: the program
/// queue's bound plus the slots the delay holds back.
pub fn queue_bound(delay_100ns: i64) -> usize {
    VBAN_QUEUE_BOUND + (delay_100ns.max(0) as u64).div_ceil(SLOT_100NS) as usize
}

/// `host:port` of a destination (the target's spec and status label).
pub fn target_spec(dest: &VbanDest) -> String {
    format!("{}:{}", dest.host, dest.port)
}
```

3. `VbanConfig`: replace `new(&VbanSettings, …)` with

```rust
    /// #233: the config of one destination over its resolved target.
    pub fn for_dest(dest: &VbanDest, enabled: bool, targets: Vec<VbanTarget>) -> Self {
        Self::named(enabled, &dest.stream_name, targets)
    }

    fn named(enabled: bool, stream_name: &str, targets: Vec<VbanTarget>) -> Self {
        let name_bytes = stream_name_bytes(stream_name);
        let stream_name = name_bytes.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
        Self { enabled, stream_name, name_bytes, targets }
    }
```

and `Default` → `Self::named(false, DEFAULT_VBAN_STREAM_NAME, Vec::new())`.

4. `resolve_dest` replaces `resolve_config`:

```rust
/// #233: resolve one destination on the blocking pool (std DNS); a failed
/// re-resolve keeps the last good address (`resolve_targets`).
pub async fn resolve_dest(dest: VbanDest, enabled: bool, previous: Vec<VbanTarget>) -> VbanConfig {
    let joined = tokio::task::spawn_blocking(move || {
        let mut resolve = system_resolve;
        let targets = resolve_targets(&[target_spec(&dest)], &previous, &mut resolve);
        VbanConfig::for_dest(&dest, enabled, targets)
    })
    .await;
    joined.unwrap_or_else(|e| {
        warn!(%e, "vban output: the resolve task failed — output disabled");
        VbanConfig::default()
    })
}
```

5. `VbanOut` gains `format: VbanFormat, delay_100ns: i64, bound: usize`; `new()` = `Self::for_destination(VbanFormat::PROGRAM, 0)`;

```rust
    /// #233: an output for one destination's format, delayed by `delay_100ns`.
    pub fn for_destination(format: VbanFormat, delay_100ns: i64) -> Self {
        let bound = queue_bound(delay_100ns);
        Self {
            queue: Mutex::new(VbanQueue { blocks: VecDeque::with_capacity(bound + 1), stop: false }),
            ready: Condvar::new(),
            config: Mutex::new(Arc::new(VbanConfig::default())),
            stats: Mutex::new(VbanCounters::default()),
            running: AtomicBool::new(false),
            format,
            delay_100ns,
            bound,
        }
    }

    /// #233: the output of a VBAN entry at `network_rate`.
    pub fn for_entry(entry: &OutputEntry, network_rate: u32) -> Result<Self, String> {
        let dest = entry.vban.as_ref().ok_or_else(|| "not a VBAN entry".to_string())?;
        let format = VbanFormat::new(effective_rate(entry.rate, network_rate), dest.format)?;
        Ok(Self::for_destination(format, i64::from(entry.delay_ms) * 10_000))
    }

    pub fn format(&self) -> VbanFormat { self.format }
    pub fn delay_100ns(&self) -> i64 { self.delay_100ns }
    pub fn bound(&self) -> usize { self.bound }
```

`push(&self, block: ProgramBlock)` uses `self.bound` where it used `VBAN_QUEUE_BOUND`; `VbanQueue.blocks` / `VbanTake::Block` carry `ProgramBlock`. `VbanCounters` + `VbanStatus` gain `blocks_sent: u64` (copied in `status()`), counted by a new `fn record_block(&self)`.

6. `VbanSender`:

```rust
pub struct VbanSender {
    format: VbanFormat,
    latency_100ns: i64,
    converter: VbanRateConverter,
    encoder: VbanEncoder,
    packets: Vec<u8>,
    last_send_100ns: Option<i64>,
}

impl Default for VbanSender {
    /// The `PROGRAM` format, no delay (#210's sender).
    fn default() -> Self {
        Self::new(VbanFormat::PROGRAM, 0)
    }
}

impl VbanSender {
    fn new(format: VbanFormat, delay_100ns: i64) -> Self {
        Self {
            format,
            latency_100ns: VBAN_SEND_LATENCY_100NS + delay_100ns,
            converter: VbanRateConverter::new(format.rate_hz()),
            encoder: VbanEncoder::default(),
            packets: empty_packets(format),
            last_send_100ns: None,
        }
    }

    /// #233: the sender of `out`'s destination.
    pub fn for_out(out: &VbanOut) -> Self {
        Self::new(out.format(), out.delay_100ns())
    }
```

and in `send_block`, between the `is_active` check and the loop:

```rust
        let samples = self.converter.convert(block.samples.as_deref());
        let first = self.encoder.next_counter();
        self.encoder
            .encode_into(self.format, &cfg.name_bytes, samples, &mut self.packets);
        for (k, packet) in self.packets.chunks_exact(self.format.packet_len()).enumerate() {
            let at = packet_send_at_in(self.format, block.due_100ns, self.latency_100ns, k);
```

(the rest of the loop body unchanged), after the loop `out.record_block();` and return `self.format.packets_per_block()`. The substitution WARN text drops "48 kHz" → "a program pair's audio was not one program block — sent silence".

7. `run_vban_loop`: `let mut sender = VbanSender::for_out(out);` (was `VbanSender::default()`). `spawn_vban_thread(out: Arc<VbanOut>, id: String)` logs `info!(id = %id, rate = out.format().rate_hz(), …, "vban output thread started")`; the rest unchanged (MMCSS "Pro Audio", `WallVbanClock::slewing`).

`vban_out_tests.rs` (refactor commit, no assertion weakened):
- `block()` builds `ProgramBlock { …, samples: Some(vec![v; VBAN_BLOCK_SAMPLES].into()), … }`; every `VbanBlock` → `ProgramBlock`; a `samples` compared with a `Vec` compares slices (`&s[..]`).
- `active_config(addrs)` builds `VbanConfig::for_dest(&VbanDest { host: "test".into(), port: 6980, stream_name: "sp-program".into(), format: VbanSampleFormat::Int24 }, true, targets)` with the same `targets` as today.
- DELETE `the_settings_load_with_defaults_and_stored_values`, `at_most_8_targets_are_used` (the keys are gone; Task 1.1 pins the 8-entry cap, Task 1.3 the 8-target migration), `resolve_config_builds_the_config_from_the_settings` and `the_config_carries_the_wire_name_and_the_status_its_targets` (replaced in `vban_out_tests_dest.rs` above). Say so in the commit message.

The other renamed files (`program_output_tests_*.rs`, `vban_out_tests_regrid.rs`): `VbanBlock` → `ProgramBlock`, `Some(v)` → `Some(v.into())` in a literal, `&s[..]` in a comparison — mechanical, no assertion changes.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server vban_out` and `cargo test -p sp-server program_output` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): a VBAN output per destination — 0.72 datagrams for a migrated FOH entry, 96 kHz, delay, queue` then `refactor(#233): VbanBlock is the shared ProgramBlock (Arc samples) — test literals adapted, no assertion changed` then `feat(#233): VbanOut per destination — format, converter, delay, one target; the vban_* settings code removed`.

### Task 1.7: the fan-out, wired into `ProgramOutput` and `ProgramBus`

**Files:**
- Create: `crates/sp-server/src/playback/audio_out.rs`, `crates/sp-server/src/playback/audio_out_tests.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (`pub mod audio_out; // #233: the program audio's fan-out to its outputs (one queue + thread each)`), `crates/sp-server/src/playback/program_output.rs`, `crates/sp-server/src/playback/program_bus.rs` (line-neutral), `crates/sp-server/src/playback/program_output_tests_order.rs` (one test)

**Interfaces:**
- Consumes: `ProgramBlock`, `VbanOut` (+ `status`, `is_running`, `config`), `VbanStatus`, `fft_delay_frames`, `VBAN_SEND_LATENCY_100NS`, `OutputEntry`.
- Produces: `audio_out::{enum OutputSink { Vban(Arc<VbanOut>) } (push, stop), struct RunningOutput { entry: OutputEntry, built_rate: u32, sink: Option<OutputSink>, error: Option<String> } (status), struct AudioOutputs (new, offer(&ProgramBlock), running() -> Arc<Vec<RunningOutput>>, replace(Vec<RunningOutput>), stop_all, network_rate/set_network_rate, problems/set_problems, status() -> Vec<OutputStatus>), struct OutputStatus { id, kind ("type"), name, enabled, state, reason, rate, format, channels, delay_ms, latency_ms, blocks_sent, blocks_dropped, vban: Option<VbanStatus> }, STATE_{RUNNING,OPENING,WAITING,DISABLED}, vban_state(enabled, build_error: Option<&str>, thread_running, addressed, resolve_error: Option<&str>) -> (&'static str, Option<String>), vban_latency_ms(delay_ms, rate_hz) -> f64}`; `tests::running_vban(id, Arc<VbanOut>) -> RunningOutput` (`pub(crate)`); `ProgramBus::outputs() -> &Arc<AudioOutputs>` (replaces `vban()`); `ProgramOutput::with_outputs(Arc<AudioOutputs>)` (replaces `with_vban`, which stays as a `#[cfg(test)]` shim over `AudioOutputs::single_vban`).

- [ ] **Step 1: Write the failing tests** — `audio_out_tests.rs`:

```rust
//! #233: the fan-out — every running output gets the same shared block, a
//! disabled entry nothing, a full queue never costs another output a block;
//! the state and latency each output reports.

use super::*;
use crate::playback::vban_out::VbanTake;
use crate::playback::vban_packet::VbanFormat;
use sp_core::audio_outputs::{RateChoice, VbanDest, VbanSampleFormat};
use std::time::Duration;

pub(crate) fn entry(id: &str) -> OutputEntry {
    OutputEntry::vban(
        id,
        id,
        VbanDest {
            host: "127.0.0.1".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    )
}

pub(crate) fn running_vban(id: &str, out: Arc<VbanOut>) -> RunningOutput {
    RunningOutput { entry: entry(id), built_rate: 48_000, sink: Some(OutputSink::Vban(out)), error: None }
}

fn take(out: &VbanOut) -> ProgramBlock {
    match out.take_timeout(Duration::ZERO) {
        VbanTake::Block(b) => b,
        other => panic!("no block: {other:?}"),
    }
}

#[test]
fn every_running_output_gets_the_same_block() {
    let (a, b) = (Arc::new(VbanOut::new()), Arc::new(VbanOut::new()));
    let outputs = AudioOutputs::new();
    let mut off = entry("out-3");
    off.enabled = false;
    outputs.replace(vec![
        running_vban("out-1", a.clone()),
        running_vban("out-2", b.clone()),
        RunningOutput { entry: off, built_rate: 48_000, sink: None, error: None },
    ]);
    let block = ProgramBlock { due_100ns: 7, samples: Some(vec![0.5; 3200].into()), substituted: false };
    outputs.offer(&block);
    let (ga, gb) = (take(&a), take(&b));
    assert_eq!(ga, block);
    assert!(Arc::ptr_eq(ga.samples.as_ref().unwrap(), gb.samples.as_ref().unwrap()), "one copy, shared");
}

#[test]
fn a_full_queue_on_one_output_never_costs_another_a_block() {
    let (stuck, live) = (Arc::new(VbanOut::new()), Arc::new(VbanOut::new()));
    let outputs = AudioOutputs::new();
    outputs.replace(vec![running_vban("out-1", stuck.clone()), running_vban("out-2", live.clone())]);
    for i in 0..15 {
        outputs.offer(&ProgramBlock::silence(i));
        assert_eq!(take(&live), ProgramBlock::silence(i));
    }
    assert_eq!(stuck.status().blocks_dropped, 5, "15 offered, bound 10");
    assert_eq!(live.status().blocks_dropped, 0);
}

#[test]
fn the_vban_state_table() {
    assert_eq!(vban_state(false, None, true, true, None), (STATE_DISABLED, None));
    assert_eq!(
        vban_state(true, Some("VBAN carries no 32000 Hz"), false, false, None),
        (STATE_WAITING, Some("VBAN carries no 32000 Hz".to_string()))
    );
    assert_eq!(vban_state(true, None, false, true, None), (STATE_OPENING, None));
    assert_eq!(
        vban_state(true, None, true, false, Some("no IPv4 address")),
        (STATE_WAITING, Some("no IPv4 address".to_string()))
    );
    assert_eq!(
        vban_state(true, None, true, false, None),
        (STATE_WAITING, Some("the address is not resolved yet".to_string()))
    );
    assert_eq!(vban_state(true, None, true, true, Some("old error")), (STATE_RUNNING, None));
}

#[test]
fn a_vban_outputs_latency_is_the_send_budget_the_delay_and_the_converter() {
    assert!((vban_latency_ms(0, 48_000) - 66.6666).abs() < 1e-3);
    assert!((vban_latency_ms(40, 48_000) - 106.6666).abs() < 1e-3);
    assert!((vban_latency_ms(0, 96_000) - (66.6666 + 16.6667)).abs() < 1e-3);
}

#[test]
fn the_status_lists_every_entry_in_list_order() {
    let out = Arc::new(VbanOut::for_destination(VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap(), 0));
    out.set_config(crate::playback::vban_out::tests::active_config(&["127.0.0.1:6980"]));
    let mut fast = entry("out-1");
    fast.rate = RateChoice::Fixed(96_000);
    let mut off = entry("out-2");
    off.enabled = false;
    let outputs = AudioOutputs::new();
    outputs.replace(vec![
        RunningOutput { entry: fast, built_rate: 96_000, sink: Some(OutputSink::Vban(out)), error: None },
        RunningOutput { entry: off, built_rate: 48_000, sink: None, error: None },
    ]);
    let st = outputs.status();
    assert_eq!(st.len(), 2);
    assert_eq!((st[0].id.as_str(), st[0].kind, st[0].rate, st[0].format), ("out-1", "vban", 96_000, "int24"));
    assert_eq!(st[0].state, STATE_OPENING, "no thread off Windows");
    assert_eq!(st[0].channels, 2);
    assert_eq!(st[0].vban.as_ref().unwrap().targets[0].target, "127.0.0.1:6980");
    assert_eq!((st[1].state, st[1].blocks_sent), (STATE_DISABLED, 0));
    assert!(st[1].vban.is_none());
}
```

and in `program_output_tests_order.rs`, next to the three VBAN-before-NDI tests:

```rust
/// #233: with two outputs, BOTH hold the boundary's block before its NDI
/// submit returns — the fan-out sits where #210's VBAN hand-off sat.
#[test]
fn every_output_has_the_block_before_the_ndi_submit() {
    let gate = Arc::new(Gate::default());
    let _opener = Opener(gate.clone());
    let (entered_tx, entered_rx) = mpsc::channel();
    let held = gate.clone();
    let backend = Arc::new(HookedNdi {
        inner: MockNdiBackend::new(),
        on_audio: Box::new(move || {
            let _ = entered_tx.send(());
            held.wait();
        }),
    });
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let (a, b) = (Arc::new(VbanOut::new()), Arc::new(VbanOut::new()));
    let outputs = Arc::new(crate::playback::audio_out::AudioOutputs::new());
    outputs.replace(vec![
        crate::playback::audio_out::tests::running_vban("out-1", a.clone()),
        crate::playback::audio_out::tests::running_vban("out-2", b.clone()),
    ]);
    let out = ProgramOutput::new(sender, 2, 2).with_outputs(outputs);
    let stamp = at(4);
    let submit = std::thread::spawn(move || {
        let mut out = out;
        let s = out.submit(ProgramJob::Standby { stamp_100ns: stamp });
        (out, s)
    });
    entered_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the pair's NDI submit started");
    assert_eq!((a.queued(), b.queued()), (1, 1), "both outputs before the NDI submit");
    gate.open();
    let (_out, s) = submit.join().expect("the submit thread");
    assert_eq!(s, stamp);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server audio_out::tests` and `cargo test -p sp-server program_output` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement** — `audio_out.rs`:

```rust
//! #233: the fan-out of the program's audio to its outputs. After the peak
//! limiter, where #210's VBAN hand-off sat (`ProgramOutput::serve`, before
//! MAX and the NDI submit), each boundary's block — ONE shared copy
//! (`ProgramBlock`) — is pushed into every running output's own bounded,
//! drop-oldest queue; every output has its own thread. A slow, blocked or
//! failed output can never delay the boundary, the NDI submit, MAX or another
//! output: a push takes the output's queue lock for µs and never waits.
//!
//! The list is swapped whole by the settings task (`audio_out_task.rs`): the
//! program thread reads one `Arc` snapshot per boundary. `status()` is
//! `GET /api/v1/program` → `outputs[]`, in list order, disabled entries too.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use sp_core::audio_outputs::{DEFAULT_NETWORK_RATE, OutputEntry, OutputType};

use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_out::{VbanOut, VbanStatus};
use crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS;
use crate::playback::vban_rate::fft_delay_frames;

pub const STATE_RUNNING: &str = "running";
pub const STATE_OPENING: &str = "opening";
pub const STATE_WAITING: &str = "waiting";
pub const STATE_DISABLED: &str = "disabled";

/// Where an output's blocks go (Lane 3 adds `Asio`).
#[derive(Clone)]
pub enum OutputSink {
    Vban(Arc<VbanOut>),
}

impl OutputSink {
    pub fn push(&self, block: ProgramBlock) {
        match self {
            Self::Vban(out) => out.push(block),
        }
    }

    pub fn stop(&self) {
        match self {
            Self::Vban(out) => out.stop(),
        }
    }
}

/// One entry of the list as it runs.
#[derive(Clone)]
pub struct RunningOutput {
    pub entry: OutputEntry,
    /// The rate it was built for (`audio_out_task::build_rate`).
    pub built_rate: u32,
    /// `None`: disabled, or it could not be built (`error`).
    pub sink: Option<OutputSink>,
    pub error: Option<String>,
}

/// `GET /api/v1/program` → `outputs[i]`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputStatus {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    pub enabled: bool,
    /// running | opening | waiting | disabled.
    pub state: &'static str,
    pub reason: Option<String>,
    pub rate: u32,
    pub format: &'static str,
    pub channels: u32,
    pub delay_ms: u32,
    pub latency_ms: f64,
    pub blocks_sent: u64,
    pub blocks_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vban: Option<VbanStatus>,
}

/// A VBAN output's state: disabled, not built (the reason), its thread not
/// running yet, waiting for an address, or running.
pub fn vban_state(
    enabled: bool,
    build_error: Option<&str>,
    thread_running: bool,
    addressed: bool,
    resolve_error: Option<&str>,
) -> (&'static str, Option<String>) {
    if !enabled {
        return (STATE_DISABLED, None);
    }
    if let Some(e) = build_error {
        return (STATE_WAITING, Some(e.to_string()));
    }
    if !thread_running {
        return (STATE_OPENING, None);
    }
    if addressed {
        return (STATE_RUNNING, None);
    }
    let reason = resolve_error.unwrap_or("the address is not resolved yet");
    (STATE_WAITING, Some(reason.to_string()))
}

/// A VBAN output's latency from the boundary, ms: the send latency, the
/// delay, the rate converter's delay.
pub fn vban_latency_ms(delay_ms: u32, rate_hz: u32) -> f64 {
    VBAN_SEND_LATENCY_100NS as f64 / 10_000.0
        + f64::from(delay_ms)
        + fft_delay_frames(rate_hz) as f64 * 1_000.0 / f64::from(rate_hz)
}

impl RunningOutput {
    pub fn status(&self) -> OutputStatus {
        let e = &self.entry;
        match e.kind {
            OutputType::Vban => {
                let st = self.sink.as_ref().map(|OutputSink::Vban(out)| (out.status(), out.is_running()));
                let addressed = st.as_ref().is_some_and(|(s, _)| s.targets.iter().any(|t| t.addr.is_some()));
                let resolve_error = st.as_ref().and_then(|(s, _)| s.targets.first()).and_then(|t| t.error.clone());
                let (state, reason) = vban_state(
                    e.enabled,
                    self.error.as_deref(),
                    st.as_ref().is_some_and(|(_, r)| *r),
                    addressed,
                    resolve_error.as_deref(),
                );
                let telemetry = st.map(|(s, _)| s);
                OutputStatus {
                    id: e.id.clone(),
                    kind: e.kind.as_str(),
                    name: e.name.clone(),
                    enabled: e.enabled,
                    state,
                    reason,
                    rate: self.built_rate,
                    format: e.vban.as_ref().map_or("int24", |v| v.format.as_str()),
                    channels: 2,
                    delay_ms: e.delay_ms,
                    latency_ms: vban_latency_ms(e.delay_ms, self.built_rate),
                    blocks_sent: telemetry.as_ref().map_or(0, |s| s.blocks_sent),
                    blocks_dropped: telemetry.as_ref().map_or(0, |s| s.blocks_dropped),
                    vban: telemetry,
                }
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The program's audio outputs (owned by `ProgramBus`).
pub struct AudioOutputs {
    list: Mutex<Arc<Vec<RunningOutput>>>,
    network_rate: AtomicU32,
    problems: Mutex<Vec<String>>,
}

impl Default for AudioOutputs {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioOutputs {
    pub fn new() -> Self {
        Self {
            list: Mutex::new(Arc::new(Vec::new())),
            network_rate: AtomicU32::new(DEFAULT_NETWORK_RATE),
            problems: Mutex::new(Vec::new()),
        }
    }

    /// Hand `block` to every running output (an `Arc` bump each; never waits).
    pub fn offer(&self, block: &ProgramBlock) {
        let list = self.running();
        for sink in list.iter().filter_map(|o| o.sink.as_ref()) {
            sink.push(block.clone());
        }
    }

    pub fn running(&self) -> Arc<Vec<RunningOutput>> {
        lock(&self.list).clone()
    }

    pub fn replace(&self, list: Vec<RunningOutput>) {
        *lock(&self.list) = Arc::new(list);
    }

    /// Stop every output's thread (process shutdown).
    pub fn stop_all(&self) {
        for sink in self.running().iter().filter_map(|o| o.sink.as_ref()) {
            sink.stop();
        }
    }

    pub fn network_rate(&self) -> u32 {
        self.network_rate.load(Ordering::Relaxed)
    }

    pub fn set_network_rate(&self, rate: u32) {
        self.network_rate.store(rate, Ordering::Relaxed);
    }

    pub fn problems(&self) -> Vec<String> {
        lock(&self.problems).clone()
    }

    pub fn set_problems(&self, problems: Vec<String>) {
        *lock(&self.problems) = problems;
    }

    pub fn status(&self) -> Vec<OutputStatus> {
        self.running().iter().map(RunningOutput::status).collect()
    }

    /// Tests: one VBAN output as the whole fan-out (#210's test seam).
    #[cfg(test)]
    pub fn single_vban(out: Arc<VbanOut>) -> Self {
        let outputs = Self::new();
        outputs.replace(vec![tests::running_vban("out-1", out)]);
        outputs
    }
}

#[cfg(test)]
#[path = "audio_out_tests.rs"]
pub(crate) mod tests;
```

(`self.sink.as_ref().map(|OutputSink::Vban(out)| …)` is an irrefutable closure pattern while `OutputSink` has one variant; Lane 3 turns it into a `match`.)

`program_output.rs`: import `crate::playback::audio_out::AudioOutputs` and `crate::playback::audio_out_block::{ProgramBlock, is_program_block}` instead of the `vban_out` items; the `vban` field becomes

```rust
    /// #210 + #233: the program's audio outputs (`audio_out.rs`): each
    /// submitted pair's limited block, copied once and shared.
    outputs: Option<Arc<AudioOutputs>>,
```

(`outputs: None` in `new`), and

```rust
    /// #233: also hand every submitted pair's audio block to the outputs.
    pub fn with_outputs(mut self, outputs: Arc<AudioOutputs>) -> Self {
        self.outputs = Some(outputs);
        self
    }

    /// Tests: one VBAN output as the whole fan-out (the #210 tests' seam).
    #[cfg(test)]
    pub fn with_vban(self, vban: Arc<crate::playback::vban_out::VbanOut>) -> Self {
        self.with_outputs(Arc::new(AudioOutputs::single_vban(vban)))
    }

    /// #210 + #233: hand one pair's audio block to every output (never
    /// blocks); returns the instant it was handed over, read off `now`.
    fn feed_outputs(&self, block: ProgramBlock, now: &impl Fn() -> i64) -> i64 {
        if let Some(outputs) = &self.outputs {
            outputs.offer(&block);
        }
        now()
    }
```

`Pair::vban_block` → `Pair::program_block` (same body, `ProgramBlock`); `serve` calls `self.feed_outputs(pair.program_block(stamp_100ns), &now)`. `start_program`: drop the `vban` local, the `run_vban_config_task` spawn and the Windows `spawn_vban_thread`; add after `self.program.set(...)`:

```rust
        // #233: the outputs task (the vban_* migration, the 5 s list re-read,
        // one thread per output).
        crate::playback::audio_out_task::start_outputs(self.pool.clone(), bus.outputs().clone(), shutdown);
```

and in the shutdown closure `bus.outputs().stop_all();` instead of `vban.stop();`. `spawn_program_thread`: `.with_outputs(bus.outputs().clone())`. Docs: the module doc's #210 paragraph says "the program's audio outputs (`audio_out.rs`, #233)" where it says "the program's VBAN output"; the const-assert stays. (`audio_out_task::start_outputs` lands in Task 1.8; the two tasks go in one push.)

`program_bus.rs` (keep the line count): `use crate::playback::vban_out::VbanOut;` → `use crate::playback::audio_out::AudioOutputs;`; the field

```rust
    /// #210 + #233: the program's audio outputs, fed by the `SP-program`
    /// sender thread and reported under `outputs` on `GET /api/v1/program`.
    outputs: Arc<AudioOutputs>,
```

`vban: Arc::new(VbanOut::new())` → `outputs: Arc::new(AudioOutputs::new())`; the accessor `/// #233: the program's audio outputs.` + `pub fn outputs(&self) -> &Arc<AudioOutputs> { &self.outputs }`. Run `wc -l` after `cargo fmt`: 971.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server audio_out::tests`, `cargo test -p sp-server program_output`, `cargo test -p sp-server program_bus` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the fan-out — one shared block for every output, no cross-output drops, both before the NDI submit` then `feat(#233): AudioOutputs fan-out after the limiter; ProgramBus owns it`.

### Task 1.8: the outputs task — keep what did not change, rebuild what did

**Files:**
- Create: `crates/sp-server/src/playback/audio_out_task.rs`, `crates/sp-server/src/playback/audio_out_task_tests.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (`pub mod audio_out_task; // #233: the outputs' settings task (keep / build / stop, DNS, migration)`)

**Interfaces:**
- Consumes: `audio_out_config::{load, OutputsSettings}`, `audio_out_migrate::{migrate_vban_settings, MigrationOutcome}`, `AudioOutputs`, `RunningOutput`, `OutputSink`, `VbanOut::for_entry`, `resolve_dest`, `needs_resolve`, `spawn_vban_thread` (Windows), `effective_rate`.
- Produces: `audio_out_task::{OUTPUTS_SETTINGS_POLL: Duration (5 s), enum Step { Keep(usize), Build }, struct Plan { steps: Vec<Step>, stop: Vec<usize> }, build_rate(&OutputEntry, network_rate) -> u32, plan(&[RunningOutput], &[OutputEntry], network_rate) -> Plan, async apply(&AudioOutputs, OutputsSettings, &mut HashMap<String, Instant>), start_outputs(SqlitePool, Arc<AudioOutputs>, &broadcast::Sender<()>), async run_outputs_task(…)}`.

- [ ] **Step 1: Write the failing tests** — `audio_out_task_tests.rs`:

```rust
//! #233: the outputs task's plan (keep an unchanged entry, build a new or
//! changed one, stop a removed or changed one; a network-rate change rebuilds
//! only the entries that follow the network) and `apply` on real outputs.

use super::*;
use crate::playback::audio_out::{OutputSink, RunningOutput};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_config::OutputsSettings;
use crate::playback::vban_out::{VbanOut, VbanTake};
use sp_core::audio_outputs::{OutputEntry, RateChoice, VbanDest, VbanSampleFormat};
use std::sync::Arc;
use std::time::Duration;

fn entry(id: &str, rate: RateChoice) -> OutputEntry {
    let mut e = OutputEntry::vban(
        id,
        id,
        VbanDest {
            host: "127.0.0.1".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    );
    e.rate = rate;
    e
}

fn ran(e: &OutputEntry, built_rate: u32) -> RunningOutput {
    RunningOutput { entry: e.clone(), built_rate, sink: None, error: None }
}

const FIXED: RateChoice = RateChoice::Fixed(48_000);
const NET: RateChoice = RateChoice::Network;

#[test]
fn an_unchanged_list_keeps_everything() {
    let (a, b) = (entry("out-1", FIXED), entry("out-2", NET));
    let p = plan(&[ran(&a, 48_000), ran(&b, 96_000)], &[a, b], 96_000);
    assert_eq!(p, Plan { steps: vec![Step::Keep(0), Step::Keep(1)], stop: vec![] });
}

#[test]
fn an_added_entry_is_built_and_the_rest_kept_in_settings_order() {
    let (a, b, c) = (entry("out-1", FIXED), entry("out-2", FIXED), entry("out-3", FIXED));
    let p = plan(&[ran(&a, 48_000), ran(&b, 48_000)], &[c, b, a], 96_000);
    assert_eq!(p.steps, vec![Step::Build, Step::Keep(1), Step::Keep(0)]);
    assert!(p.stop.is_empty());
}

#[test]
fn a_changed_entry_is_rebuilt_and_its_old_output_stopped() {
    let a = entry("out-1", FIXED);
    let mut a2 = a.clone();
    a2.delay_ms = 10;
    let p = plan(&[ran(&a, 48_000)], &[a2], 48_000);
    assert_eq!(p, Plan { steps: vec![Step::Build], stop: vec![0] });
    let mut off = a.clone();
    off.enabled = false;
    assert_eq!(plan(&[ran(&a, 48_000)], &[off], 48_000).stop, vec![0], "a toggle rebuilds");
}

#[test]
fn a_removed_entry_is_stopped() {
    let (a, b) = (entry("out-1", FIXED), entry("out-2", FIXED));
    let p = plan(&[ran(&a, 48_000), ran(&b, 48_000)], std::slice::from_ref(&b), 48_000);
    assert_eq!(p, Plan { steps: vec![Step::Keep(1)], stop: vec![0] });
}

#[test]
fn a_network_rate_change_rebuilds_only_the_entries_that_follow_it() {
    let (foh, net) = (entry("out-1", FIXED), entry("out-2", NET));
    let running = [ran(&foh, 48_000), ran(&net, 48_000)];
    let p = plan(&running, &[foh.clone(), net.clone()], 96_000);
    assert_eq!(p, Plan { steps: vec![Step::Keep(0), Step::Build], stop: vec![1] });
    assert_eq!(build_rate(&net, 96_000), 96_000);
    assert_eq!(build_rate(&foh, 96_000), 48_000);
}

fn settings(entries: Vec<OutputEntry>, network_rate: u32) -> OutputsSettings {
    OutputsSettings { entries, network_rate, problems: vec!["p".into()] }
}

fn vban(o: &RunningOutput) -> Arc<VbanOut> {
    match &o.sink {
        Some(OutputSink::Vban(out)) => out.clone(),
        None => panic!("{} has no output", o.entry.id),
    }
}

#[tokio::test]
async fn apply_keeps_an_unchanged_output_when_another_is_added() {
    let outputs = AudioOutputs::new();
    let mut resolved = std::collections::HashMap::new();
    let (a, b) = (entry("out-1", FIXED), entry("out-2", NET));
    apply(&outputs, settings(vec![a.clone(), b.clone()], 96_000), &mut resolved).await;
    let first = outputs.running();
    let sink_a = vban(&first[0]);
    assert!(sink_a.config().is_active(), "resolved at build");
    sink_a.push(ProgramBlock::silence(1));
    let c = entry("out-3", FIXED);
    apply(&outputs, settings(vec![a.clone(), b.clone(), c], 96_000), &mut resolved).await;
    let second = outputs.running();
    assert_eq!(second.len(), 3);
    assert!(Arc::ptr_eq(&sink_a, &vban(&second[0])), "out-1 kept: its thread, queue, counter");
    assert!(Arc::ptr_eq(&vban(&first[1]), &vban(&second[1])), "out-2 kept");
    assert_eq!(sink_a.queued(), 1, "its queue kept");
    assert_eq!(outputs.network_rate(), 96_000);
    assert_eq!(outputs.problems(), vec!["p".to_string()]);

    let mut a2 = a.clone();
    a2.delay_ms = 10;
    apply(&outputs, settings(vec![a2, b], 96_000), &mut resolved).await;
    let third = outputs.running();
    assert!(!Arc::ptr_eq(&sink_a, &vban(&third[0])), "a changed entry is rebuilt");
    assert_eq!(sink_a.take_timeout(Duration::ZERO), VbanTake::Block(ProgramBlock::silence(1)));
    assert_eq!(sink_a.take_timeout(Duration::ZERO), VbanTake::Stopped, "the old one stopped");
}

#[tokio::test]
async fn a_disabled_entry_runs_nothing_but_is_listed() {
    let outputs = AudioOutputs::new();
    let mut off = entry("out-1", FIXED);
    off.enabled = false;
    apply(&outputs, settings(vec![off], 48_000), &mut std::collections::HashMap::new()).await;
    let list = outputs.running();
    assert_eq!(list.len(), 1);
    assert!(list[0].sink.is_none());
    assert_eq!(outputs.status()[0].state, "disabled");
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server audio_out_task` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement** — `audio_out_task.rs`:

```rust
//! #233: the outputs' settings task. At its start it runs the one-time
//! `vban_*` migration (`audio_out_migrate.rs`); then every
//! [`OUTPUTS_SETTINGS_POLL`] it reads the list (leniently) and the network
//! rate, and applies them: an entry identical to a running one (and built
//! for the same rate) is KEPT — its thread, queue and frame counter run on —
//! a new or changed one is built (its target resolved, its thread spawned on
//! Windows), a removed or changed one is stopped (its thread drains and
//! exits). A kept VBAN output re-resolves its target every
//! `VBAN_RESOLVE_EVERY`; a failed re-resolve keeps the last good address.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::audio_outputs::{OutputEntry, OutputType, effective_rate};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::audio_out::{AudioOutputs, OutputSink, RunningOutput};
use crate::playback::audio_out_config::{OutputsSettings, load};
use crate::playback::audio_out_migrate::{MigrationOutcome, migrate_vban_settings};
use crate::playback::vban_out::{VbanConfig, VbanOut, needs_resolve, resolve_dest};

/// How often the list is re-read (#210's `VBAN_SETTINGS_POLL`).
pub const OUTPUTS_SETTINGS_POLL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Keep the running output at this index.
    Keep(usize),
    Build,
}

/// One step per wanted entry (settings order), and the running outputs to stop.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub stop: Vec<usize>,
}

/// The rate an entry's output is built for.
pub fn build_rate(entry: &OutputEntry, network_rate: u32) -> u32 {
    match entry.kind {
        OutputType::Vban => effective_rate(entry.rate, network_rate),
    }
}

/// Keep, build, stop. Ids are unique (validated), so at most one running
/// output matches a wanted entry.
pub fn plan(running: &[RunningOutput], wanted: &[OutputEntry], network_rate: u32) -> Plan {
    let mut kept = vec![false; running.len()];
    let mut steps = Vec::with_capacity(wanted.len());
    for w in wanted {
        let rate = build_rate(w, network_rate);
        match (0..running.len()).find(|&i| running[i].entry == *w && running[i].built_rate == rate) {
            Some(i) => {
                kept[i] = true;
                steps.push(Step::Keep(i));
            }
            None => steps.push(Step::Build),
        }
    }
    let stop = (0..running.len()).filter(|&i| !kept[i]).collect();
    Plan { steps, stop }
}

/// Apply one read of the settings (see the module doc).
pub async fn apply(
    outputs: &AudioOutputs,
    settings: OutputsSettings,
    resolved: &mut HashMap<String, Instant>,
) {
    let running = outputs.running();
    let plan = plan(&running, &settings.entries, settings.network_rate);
    for &i in &plan.stop {
        resolved.remove(&running[i].entry.id);
    }
    let mut next = Vec::with_capacity(settings.entries.len());
    for (entry, step) in settings.entries.iter().zip(&plan.steps) {
        let output = match *step {
            Step::Keep(i) => {
                let kept = running[i].clone();
                refresh_vban(&kept, resolved).await;
                kept
            }
            Step::Build => build(entry, settings.network_rate, resolved).await,
        };
        next.push(output);
    }
    outputs.replace(next);
    for &i in &plan.stop {
        if let Some(sink) = &running[i].sink {
            sink.stop();
        }
        log_stopped(&running[i].entry);
    }
    outputs.set_network_rate(settings.network_rate);
    outputs.set_problems(settings.problems);
}

async fn build(
    entry: &OutputEntry,
    network_rate: u32,
    resolved: &mut HashMap<String, Instant>,
) -> RunningOutput {
    let built_rate = build_rate(entry, network_rate);
    let mut output = RunningOutput { entry: entry.clone(), built_rate, sink: None, error: None };
    if !entry.enabled {
        return output;
    }
    match entry.kind {
        OutputType::Vban => match (VbanOut::for_entry(entry, network_rate), entry.vban.clone()) {
            (Ok(out), Some(dest)) => {
                let out = Arc::new(out);
                out.set_config(resolve_dest(dest, true, Vec::new()).await);
                resolved.insert(entry.id.clone(), Instant::now());
                warn_unresolved(&entry.id, &out.config());
                start_vban_thread(&out, &entry.id);
                log_started(entry, built_rate);
                output.sink = Some(OutputSink::Vban(out));
            }
            (Err(e), _) => output.error = Some(e),
            (Ok(_), None) => output.error = Some("not a VBAN entry".into()),
        },
    }
    output
}

/// A kept VBAN output's DNS, on #210's cadence.
async fn refresh_vban(output: &RunningOutput, resolved: &mut HashMap<String, Instant>) {
    let (Some(OutputSink::Vban(out)), Some(dest)) = (&output.sink, output.entry.vban.clone()) else {
        return;
    };
    let since = resolved.get(&output.entry.id).map(Instant::elapsed);
    if !needs_resolve(false, true, since) {
        return;
    }
    let cfg = resolve_dest(dest, true, out.config().targets.clone()).await;
    warn_unresolved(&output.entry.id, &cfg);
    out.set_config(cfg);
    resolved.insert(output.entry.id.clone(), Instant::now());
}

/// Windows: the output's paced thread (#210's, MMCSS "Pro Audio").
#[cfg_attr(test, mutants::skip)] // OS thread spawn; Windows-only, like #210's
fn start_vban_thread(out: &Arc<VbanOut>, id: &str) {
    #[cfg(windows)]
    crate::playback::vban_out::spawn_vban_thread(out.clone(), id.to_string());
    #[cfg(not(windows))]
    let _ = (out, id);
}

#[cfg_attr(test, mutants::skip)] // logging only
fn warn_unresolved(id: &str, cfg: &VbanConfig) {
    for t in cfg.targets.iter().filter(|t| t.error.is_some()) {
        warn!(id, spec = %t.spec, error = t.error.as_deref().unwrap_or_default(), kept = ?t.addr,
            "audio output: resolving a VBAN target failed");
    }
}

#[cfg_attr(test, mutants::skip)] // logging only
fn log_started(entry: &OutputEntry, rate: u32) {
    info!(id = %entry.id, name = %entry.name, kind = entry.kind.as_str(), rate, delay_ms = entry.delay_ms,
        "audio output: started");
}

#[cfg_attr(test, mutants::skip)] // logging only
fn log_stopped(entry: &OutputEntry) {
    info!(id = %entry.id, name = %entry.name, "audio output: stopped");
}

#[cfg_attr(test, mutants::skip)] // logging only; the decision is migrate_vban_settings' (tested)
fn log_migration(outcome: &MigrationOutcome) {
    match outcome {
        MigrationOutcome::Nothing => {}
        MigrationOutcome::Migrated(m) => {
            let ids: Vec<&str> = m.entries.iter().map(|e| e.id.as_str()).collect();
            info!(?ids, "audio outputs: #210's VBAN settings became entries (48 kHz INT24, unchanged)");
            for s in &m.skipped {
                warn!(target = %s, "audio outputs: a #210 VBAN target was not migrated");
            }
        }
        MigrationOutcome::DroppedStale { keys } => {
            warn!(keys, "audio outputs: stale vban_* keys next to the list were deleted");
        }
    }
}

/// Spawn [`run_outputs_task`].
#[cfg_attr(test, mutants::skip)] // task spawn
pub fn start_outputs(pool: SqlitePool, outputs: Arc<AudioOutputs>, shutdown: &broadcast::Sender<()>) {
    tokio::spawn(run_outputs_task(pool, outputs, shutdown.subscribe()));
}

/// The task: migrate once, then apply the settings every 5 s until shutdown.
#[cfg_attr(test, mutants::skip)] // a timer loop around apply (tested)
pub async fn run_outputs_task(
    pool: SqlitePool,
    outputs: Arc<AudioOutputs>,
    mut shutdown: broadcast::Receiver<()>,
) {
    match migrate_vban_settings(&pool).await {
        Ok(outcome) => log_migration(&outcome),
        Err(e) => warn!(%e, "audio outputs: the vban_* migration failed — no VBAN output until it runs"),
    }
    let mut resolved = HashMap::new();
    let mut reported: Vec<String> = Vec::new();
    loop {
        match load(&pool).await {
            Ok(settings) => {
                for p in settings.problems.iter().filter(|p| !reported.contains(p)) {
                    warn!(problem = %p, "audio outputs: a stored entry is skipped");
                }
                reported = settings.problems.clone();
                apply(&outputs, settings, &mut resolved).await;
            }
            Err(e) => warn!(%e, "audio outputs: reading the settings failed"),
        }
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(OUTPUTS_SETTINGS_POLL) => {}
        }
    }
    outputs.stop_all();
    info!("audio outputs: settings task stopped");
}

#[cfg(test)]
#[path = "audio_out_task_tests.rs"]
mod tests;
```

Mutation notes: `plan`'s `==` / `&&` are killed by the changed-entry and network-rate tests; `kept[i] = true` (a plain assignment, no mutant) is pinned by the stop lists; `!kept[i]` by `a_removed_entry_is_stopped`; `if !entry.enabled` by `a_disabled_entry_runs_nothing_but_is_listed`.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server audio_out_task` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the outputs task — keep unchanged outputs, rebuild changed ones, the network rate` then `feat(#233): audio_out_task — migration at start, the 5 s list re-read, per-output DNS`.

### Task 1.9: `GET /api/v1/program` → `outputs[]`

**Files:**
- Create: `crates/sp-server/src/api/program_tests_outputs.rs`
- Modify: `crates/sp-server/src/api/program.rs`, `crates/sp-server/src/api/program_tests.rs` (delete three VBAN tests)

**Interfaces:**
- Consumes: `ProgramBus::outputs()`, `AudioOutputs::{status, network_rate, problems}`, `OutputStatus`, Task 1.8's `apply` (test).
- Produces: `ProgramResponse { …, outputs: Vec<OutputStatus>, audio_network_rate: u32, outputs_problems: Vec<String>, … }` (no `vban`).

- [ ] **Step 1: Write the failing tests** — `program_tests_outputs.rs` (hook `#[cfg(test)] #[path = "program_tests_outputs.rs"] mod tests_outputs;` in `program.rs`):

```rust
//! #233: `GET /api/v1/program` lists every audio output (`outputs[]`) with its
//! state, rate, format, latency and telemetry; the network rate; the stored
//! entries this version could not run. The top-level `vban` block is gone.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::audio_outputs::{OutputEntry, RateChoice, VbanDest, VbanSampleFormat};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};
use crate::playback::audio_out::{OutputSink, RunningOutput};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_out::VbanOut;
use crate::playback::vban_out::tests::active_config;
use crate::playback::vban_packet::VbanFormat;

async fn get_program(state: &crate::AppState) -> serde_json::Value {
    let req = Request::builder().uri("/api/v1/program").body(Body::empty()).unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn foh(id: &str) -> OutputEntry {
    OutputEntry::vban(
        id,
        "FOH",
        VbanDest {
            host: "127.0.0.1".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    )
}

#[tokio::test]
async fn get_program_lists_every_output_with_its_telemetry() {
    let state = test_state().await;
    let out = Arc::new(VbanOut::for_destination(
        VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap(),
        400_000,
    ));
    out.set_config(active_config(&["127.0.0.1:6980"]));
    for i in 0..=out.bound() as i64 {
        out.push(ProgramBlock::silence(i)); // one over the bound
    }
    let mut fast = foh("out-1");
    fast.rate = RateChoice::Fixed(96_000);
    fast.delay_ms = 40;
    let mut off = foh("out-2");
    off.enabled = false;
    let outputs = state.program_bus.outputs();
    outputs.replace(vec![
        RunningOutput { entry: fast, built_rate: 96_000, sink: Some(OutputSink::Vban(out)), error: None },
        RunningOutput { entry: off, built_rate: 48_000, sink: None, error: None },
    ]);
    outputs.set_network_rate(96_000);

    let json = get_program(&state).await;
    assert!(json.get("vban").is_none(), "the #210 block moved under outputs[]");
    assert_eq!(json["audio_network_rate"], 96_000);
    assert_eq!(json["outputs_problems"], serde_json::json!([]));
    let o = &json["outputs"][0];
    assert_eq!(o["id"], "out-1");
    assert_eq!(o["type"], "vban");
    assert_eq!(o["name"], "FOH");
    assert_eq!(o["enabled"], true);
    assert_eq!(o["state"], "opening");
    assert_eq!(o["reason"], serde_json::Value::Null);
    assert_eq!(o["rate"], 96_000);
    assert_eq!(o["format"], "int24");
    assert_eq!(o["channels"], 2);
    assert_eq!(o["delay_ms"], 40);
    let latency = o["latency_ms"].as_f64().unwrap();
    assert!((latency - (66.6666 + 40.0 + 16.6667)).abs() < 1e-3, "{latency}");
    assert_eq!(o["blocks_dropped"], 1);
    assert_eq!(o["blocks_sent"], 0);
    assert_eq!(o["vban"]["stream_name"], "sp-program");
    assert_eq!(o["vban"]["targets"][0]["target"], "127.0.0.1:6980");
    for key in ["packets_sent", "send_errors", "late_sends", "late_max_us", "frame_counter", "slew_owed_us"] {
        assert!(o["vban"].get(key).is_some(), "vban.{key}");
    }
    let off = &json["outputs"][1];
    assert_eq!((off["id"].as_str(), off["state"].as_str()), (Some("out-2"), Some("disabled")));
    assert!(off.get("vban").is_none());
}

#[tokio::test]
async fn get_program_names_a_stored_entry_it_could_not_read() {
    let state = test_state().await;
    let raw = format!(
        "[{},{}]",
        serde_json::to_string(&foh("out-1")).unwrap(),
        r#"{"id":"out-2","name":"DVS","type":"asio","asio":{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}"#
    );
    crate::db::models::set_setting(&state.pool, "audio_outputs", &raw).await.unwrap();
    let settings = crate::playback::audio_out_config::load(&state.pool).await.unwrap();
    crate::playback::audio_out_task::apply(state.program_bus.outputs(), settings, &mut HashMap::new()).await;
    let json = get_program(&state).await;
    assert_eq!(json["outputs"].as_array().unwrap().len(), 1, "the VBAN entry runs");
    assert_eq!(json["outputs"][0]["id"], "out-1");
    assert_eq!(json["outputs_problems"][0], "entry 2 (id out-2): type must be vban");
}
```

Also move `get_program_reports_the_vban_threads_late_packets` from `program_tests.rs` here, reading `json["outputs"][0]["vban"]["late_max_us"]` / `["late_events"]` instead of `json["vban"][…]`, with the `VbanOut` placed as `running_vban("out-1", out)` (`audio_out::tests::running_vban`) via `state.program_bus.outputs().replace(…)`; its assertions are unchanged. In `program_tests.rs` DELETE `get_program_reports_the_vban_block` (replaced above), `the_vban_settings_save_through_the_settings_api_and_load_back` (the keys are gone; Task 1.2's router tests cover the saves) and the moved test.

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server api::program` — Expected now: FAIL (`outputs` missing; `vban` present).

- [ ] **Step 3: Implement** in `api/program.rs`: drop `use crate::playback::vban_out::VbanStatus;`, add `use crate::playback::audio_out::OutputStatus;`; the module doc's "`vban`, the #210 VBAN audio output's telemetry" becomes "`outputs` (#233): every audio output of the list in list order (`playback::audio_out::OutputStatus`: id, type, name, enabled, state + reason, rate, format, channels, delay, latency, blocks sent / dropped, and a VBAN entry's #210 telemetry under `vban`), `audio_network_rate` and `outputs_problems` (stored entries this version could not run)". In `ProgramResponse` replace `pub vban: VbanStatus,` with

```rust
    /// #233: every audio output, in list order.
    pub outputs: Vec<OutputStatus>,
    /// #233: the network sample rate an output at "network" runs at.
    pub audio_network_rate: u32,
    /// #233: stored entries this version could not run (named, skipped).
    pub outputs_problems: Vec<String>,
```

and in `ProgramResponse::new` replace `vban: bus.vban().status(),` with

```rust
            outputs: bus.outputs().status(),
            audio_network_rate: bus.outputs().network_rate(),
            outputs_problems: bus.outputs().problems(),
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server api::program` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): GET /api/v1/program lists the outputs; a stored entry it cannot read is named` then `feat(#233): outputs[], audio_network_rate and outputs_problems on GET /api/v1/program (the vban block moved)`.

### Task 1.10: Nastavenia "Zvukové výstupy" (VBAN entries, network rate, live state)

**Files:**
- Create: `sp-ui/src/components/audio_outputs.rs`, `e2e/settings-audio-outputs.spec.ts`
- Modify: `sp-ui/src/components/mod.rs` (`pub mod audio_outputs;`), `sp-ui/src/pages/settings.rs` (render it after the form), `sp-ui/src/components/settings_form.rs` (remove the VBAN fieldset; MERGE into `store.settings` on save), `crates/sp-core/src/config.rs` (delete `SETTING_VBAN_ENABLED` / `_STREAM_NAME` / `_TARGETS` and their test asserts; keep `DEFAULT_VBAN_STREAM_NAME`), `e2e/mock-api.mjs`
- Delete: `e2e/settings-vban.spec.ts`

**Interfaces:**
- Consumes: `sp_core::audio_outputs::{OutputEntry, RateChoice, VbanSampleFormat, SUPPORTED_RATES, new_vban, validate_list, ListError::sk}`, `sp_core::config::{SETTING_AUDIO_OUTPUTS, SETTING_AUDIO_NETWORK_RATE, audio_network_rate}`, `crate::api::patch_json_empty`, `crate::store::{DashboardStore, poll_into}`; the server's `GET /api/v1/program` → `outputs[]`.
- Produces: the testids `settings-audio-outputs` (fieldset), `settings-audio-network-rate` (select), `audio-outputs-add-vban`, `audio-outputs-save`, `audio-outputs-message`, `audio-outputs-load-error`, per row `audio-output-row` (`data-id` = the entry id), `audio-output-name`, `audio-output-enabled`, `audio-output-rate` (`network` or a rate), `audio-output-delay`, `audio-output-vban-host`, `audio-output-vban-port`, `audio-output-vban-stream`, `audio-output-vban-format`, `audio-output-state`, `audio-output-remove`; `audio_outputs::{OutputLive, ProgramOutputs, state_sk, stored_list, rate_value, rate_choice}` (Lane 3 adds the ASIO fields in the same row).

- [ ] **Step 1: Write the failing Playwright spec** — `e2e/settings-audio-outputs.spec.ts` (and `git rm e2e/settings-vban.spec.ts`):

```ts
import { test, expect, type Page } from "@playwright/test";

// #233: Nastavenia "Zvukové výstupy" — the program's audio outputs as ONE
// list (`audio_outputs`) and the network rate. A real user adds a VBAN
// output, fills it, saves; the save is ONE PATCH carrying exactly the two
// keys; the values survive a reload and the output shows its live state from
// GET /api/v1/program. A bad entry is refused in Slovak before anything is
// sent. Saving the OTHER settings keeps the outputs list (Review Focus 1).
// Zero console errors is each test's last assertion.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

function realConsoleErrors(): string[] {
  return consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
}

// Wait until the LOADED settings are in the page (`sp-ui-frontend.md`): the
// fixture's Gemini model differs from the form's default.
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-audio-outputs"]')).toBeVisible({ timeout: 10000 });
  await expect(page.locator('[data-testid="settings-gemini-model"]')).toHaveValue("gemini-2.5-flash", {
    timeout: 10000,
  });
}

function settingsPatches(page: Page): Record<string, unknown>[] {
  const bodies: Record<string, unknown>[] = [];
  page.on("request", (req) => {
    if (req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url())) {
      bodies.push(req.postDataJSON());
    }
  });
  return bodies;
}

const TWO = JSON.stringify([
  { id: "out-1", name: "FOH", type: "vban", enabled: true, rate: 48000, delay_ms: 0,
    vban: { host: "fohabl.lan", port: 6980, stream_name: "sp-program", format: "int24" } },
  { id: "out-2", name: "lv1", type: "vban", enabled: true, rate: "network", delay_ms: 20,
    vban: { host: "lv1.lan", port: 6980, stream_name: "sp-program", format: "int16" } },
]);

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/settings-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/settings-reset");
});

test("an empty list: add, fill and save one VBAN output; it survives a reload with its live state (#233)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await expect(page.locator('[data-testid="audio-output-row"]')).toHaveCount(0);
  await expect(page.locator('[data-testid="settings-audio-network-rate"]')).toHaveValue("48000");

  await page.locator('[data-testid="audio-outputs-add-vban"]').click();
  const row = page.locator('[data-testid="audio-output-row"]');
  await expect(row).toHaveCount(1);
  await expect(row).toHaveAttribute("data-id", "out-1");
  await expect(row.locator('[data-testid="audio-output-name"]')).toHaveValue("VBAN 1");
  await row.locator('[data-testid="audio-output-vban-host"]').fill("dev1.lan");
  await row.locator('[data-testid="audio-output-rate"]').selectOption("96000");
  await page.locator('[data-testid="settings-audio-network-rate"]').selectOption("96000");
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Uložené");

  expect(patches).toHaveLength(1);
  expect(Object.keys(patches[0]).sort()).toEqual(["audio_network_rate", "audio_outputs"]);
  expect(JSON.parse(patches[0]["audio_outputs"] as string)).toEqual([
    { id: "out-1", name: "VBAN 1", type: "vban", enabled: true, rate: 96000, delay_ms: 0,
      vban: { host: "dev1.lan", port: 6980, stream_name: "sp-program", format: "int24" } },
  ]);
  expect(patches[0]["audio_network_rate"]).toBe("96000");

  // Backend effect: the program lists it.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.outputs.map((o: { id: string; rate: number }) => [o.id, o.rate])).toEqual([["out-1", 96000]]);

  await page.reload();
  await openSettings(page);
  await expect(page.locator('[data-testid="audio-output-vban-host"]')).toHaveValue("dev1.lan");
  await expect(page.locator('[data-testid="settings-audio-network-rate"]')).toHaveValue("96000");
  await expect(page.locator('[data-testid="audio-output-state"]')).toContainText("beží", { timeout: 10000 });
  expect(realConsoleErrors()).toEqual([]);
});

test("a bad entry is refused in Slovak before anything is sent (#233)", async ({ page }) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await page.locator('[data-testid="audio-outputs-add-vban"]').click();
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText(
    "Výstup 1 (out-1): cieľ je prázdne",
  );
  expect(patches).toHaveLength(0);
  expect(realConsoleErrors()).toEqual([]);
});

test("an output is removed and another switched off; the save carries exactly the rest (#233)", async ({
  page,
  request,
}) => {
  await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  const patches = settingsPatches(page);
  await openSettings(page);
  const rows = page.locator('[data-testid="audio-output-row"]');
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(1).locator('[data-testid="audio-output-rate"]')).toHaveValue("network");
  await expect(rows.nth(1).locator('[data-testid="audio-output-delay"]')).toHaveValue("20");
  await expect(rows.nth(1).locator('[data-testid="audio-output-vban-format"]')).toHaveValue("int16");
  await rows.nth(0).locator('[data-testid="audio-output-remove"]').click();
  await expect(rows).toHaveCount(1);
  await rows.nth(0).locator('[data-testid="audio-output-enabled"]').uncheck();
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Uložené");
  const saved = JSON.parse(patches[0]["audio_outputs"] as string);
  expect(saved.map((e: { id: string; enabled: boolean }) => [e.id, e.enabled])).toEqual([["out-2", false]]);
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.outputs[0].state).toBe("disabled");
  await expect(page.locator('[data-testid="audio-output-state"]')).toContainText("vypnutý", { timeout: 10000 });
  expect(realConsoleErrors()).toEqual([]);
});

test("saving the other settings keeps the outputs list (#233)", async ({ page, request }) => {
  await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  const patches = settingsPatches(page);
  await openSettings(page);
  await expect(page.locator('[data-testid="audio-output-row"]')).toHaveCount(2);
  await page.locator('[data-testid="settings-gemini-model"]').fill("gemini-x");
  await page.getByRole("button", { name: "Uložiť nastavenia" }).click();
  await expect.poll(() => patches.length).toBe(1);
  expect(patches[0]).not.toHaveProperty("audio_outputs");
  await expect(page.locator('[data-testid="audio-output-row"]')).toHaveCount(2);
  const stored = await (await request.get("/api/v1/settings")).json();
  expect(JSON.parse(stored.audio_outputs)).toHaveLength(2);
  expect(realConsoleErrors()).toEqual([]);
});
```

- [ ] **Step 2: Run the mock suite against the CURRENT dist** (Tier-0, `sp-ui-frontend.md` "Running the mock E2E locally"): download the last green dev CI `dist` artifact, run the mock + `npx playwright test --project=chromium settings-audio-outputs.spec.ts` — Expected: FAIL (no `settings-audio-outputs` fieldset). CI's Frontend E2E job is the real run.

- [ ] **Step 3: Implement.** `sp-ui/src/components/audio_outputs.rs`:

```rust
//! #233: Nastavenia "Zvukové výstupy" — the program's audio outputs (ONE
//! list, `audio_outputs`) and the network sample rate. It edits
//! `sp_core::audio_outputs` entries, refuses a bad list with the SERVER's
//! own rules (the Slovak text of the first problem) before sending, saves
//! ONLY `audio_outputs` + `audio_network_rate` (its own PATCH — never the
//! other settings, which the form above saves), merges what it saved into
//! `store.settings`, and shows each output's live state (`GET
//! /api/v1/program` → `outputs[]`, every 2 s). Rows are keyed by id and every
//! cell reads the list by id: a refresh never leaves a stale row, typing never
//! drops focus (`sp-ui-frontend.md`).

use std::collections::HashMap;

use leptos::prelude::*;
use serde::Deserialize;
use sp_core::audio_outputs::{
    OutputEntry, RateChoice, SUPPORTED_RATES, VbanSampleFormat, new_vban, validate_list,
};
use sp_core::config::{SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, audio_network_rate};

use crate::api;
use crate::store::DashboardStore;

/// One output's live state (the fields this section shows).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct OutputLive {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub latency_ms: f64,
}

/// `GET /api/v1/program`, the outputs only.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ProgramOutputs {
    #[serde(default)]
    pub outputs: Vec<OutputLive>,
}

/// The Slovak label of an output's state.
pub fn state_sk(state: &str) -> &'static str {
    match state {
        "running" => "beží",
        "opening" => "otvára sa",
        "waiting" => "čaká",
        "disabled" => "vypnutý",
        _ => "neznámy stav",
    }
}

/// The stored list the section starts from; `Err` when it cannot be read
/// (the section then refuses to save over it).
pub fn stored_list(settings: &HashMap<String, String>) -> Result<Vec<OutputEntry>, String> {
    match settings.get(SETTING_AUDIO_OUTPUTS).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(Vec::new()),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|_| "Uložený zoznam výstupov sa nedá načítať — neukladajte ho".to_string()),
    }
}

/// A rate as the select's value.
pub fn rate_value(rate: RateChoice) -> String {
    match rate {
        RateChoice::Network => "network".to_string(),
        RateChoice::Fixed(hz) => hz.to_string(),
    }
}

/// The select's value as a rate.
pub fn rate_choice(value: &str) -> RateChoice {
    value.parse().map(RateChoice::Fixed).unwrap_or(RateChoice::Network)
}

fn live_text(live: &ProgramOutputs, id: &str) -> String {
    match live.outputs.iter().find(|o| o.id == id) {
        None => "neuložený".to_string(),
        Some(o) if o.state == "running" => format!("{} · {:.0} ms", state_sk(&o.state), o.latency_ms),
        Some(o) => state_sk(&o.state).to_string(),
    }
}

fn live_reason(live: &ProgramOutputs, id: &str) -> String {
    live.outputs
        .iter()
        .find(|o| o.id == id)
        .and_then(|o| o.reason.clone())
        .unwrap_or_default()
}

/// Read one field of the entry `id` (reactive).
fn read<T: Default>(list: RwSignal<Vec<OutputEntry>>, id: &str, f: impl Fn(&OutputEntry) -> T) -> T {
    list.with(|l| l.iter().find(|e| e.id == id).map(&f).unwrap_or_default())
}

/// Change the entry `id` in place.
fn edit(list: RwSignal<Vec<OutputEntry>>, id: &str, f: impl FnOnce(&mut OutputEntry)) {
    list.update(|l| {
        if let Some(e) = l.iter_mut().find(|e| e.id == id) {
            f(e);
        }
    });
}

#[component]
pub fn AudioOutputs() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let entries = RwSignal::new(Vec::<OutputEntry>::new());
    let network_rate = RwSignal::new(audio_network_rate(None).to_string());
    let load_error = RwSignal::new(None::<String>);
    let message = RwSignal::new(String::new());
    let live = RwSignal::new(ProgramOutputs::default());

    let _sync = Effect::new(move |_| {
        let settings = store.settings.get();
        match stored_list(&settings) {
            Ok(list) => {
                entries.set(list);
                load_error.set(None);
            }
            Err(e) => {
                entries.set(Vec::new());
                load_error.set(Some(e));
            }
        }
        let rate = settings.get(SETTING_AUDIO_NETWORK_RATE).map(String::as_str);
        network_rate.set(audio_network_rate(rate).to_string());
    });

    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));
    let _poll = Effect::new(move |_| {
        crate::store::poll_into("/api/v1/program", 2_000, cancelled, live);
    });

    let add_vban = move |_| {
        entries.update(|l| {
            let fresh = new_vban(l);
            l.push(fresh);
        });
    };

    let on_save = move |_| {
        let list = entries.get();
        if let Err(e) = validate_list(&list) {
            message.set(e.sk());
            return;
        }
        let Ok(text) = serde_json::to_string(&list) else {
            return;
        };
        let mut body = HashMap::new();
        body.insert(SETTING_AUDIO_OUTPUTS.to_string(), text);
        body.insert(SETTING_AUDIO_NETWORK_RATE.to_string(), network_rate.get());
        leptos::task::spawn_local(async move {
            message.set("Ukladám…".into());
            match api::patch_json_empty("/api/v1/settings", &body).await {
                Ok(()) => {
                    message.set("Uložené".into());
                    store.settings.update(move |s| s.extend(body));
                }
                Err(_) => message.set("Chyba pri ukladaní".into()),
            }
        });
    };

    view! {
        <fieldset data-testid="settings-audio-outputs">
            <legend>"Zvukové výstupy"</legend>
            <label>
                "Vzorkovacia frekvencia siete"
                <select
                    data-testid="settings-audio-network-rate"
                    prop:value=move || network_rate.get()
                    on:change=move |ev| network_rate.set(event_target_value(&ev))
                >
                    {SUPPORTED_RATES
                        .iter()
                        .map(|r| view! { <option value=r.to_string()>{format!("{r} Hz")}</option> })
                        .collect_view()}
                </select>
            </label>
            {move || {
                load_error
                    .get()
                    .map(|e| view! { <p class="audio-outputs-error" data-testid="audio-outputs-load-error">{e}</p> })
            }}
            <For
                each=move || entries.with(|l| l.iter().map(|e| e.id.clone()).collect::<Vec<_>>())
                key=|id| id.clone()
                children=move |id| view! { <OutputRow id=id entries=entries live=live /> }
            />
            <div class="form-actions">
                <button type="button" data-testid="audio-outputs-add-vban" on:click=add_vban>
                    "Pridať výstup VBAN"
                </button>
                <button
                    type="button"
                    data-testid="audio-outputs-save"
                    disabled=move || load_error.get().is_some()
                    on:click=on_save
                >
                    "Uložiť výstupy"
                </button>
                <span class="save-status" data-testid="audio-outputs-message">{move || message.get()}</span>
            </div>
        </fieldset>
    }
}

#[component]
fn OutputRow(
    id: String,
    entries: RwSignal<Vec<OutputEntry>>,
    live: RwSignal<ProgramOutputs>,
) -> impl IntoView {
    let row_id = id.clone();
    let id = StoredValue::new(id);
    let vban = move |f: fn(&sp_core::audio_outputs::VbanDest) -> String| {
        read(entries, &id.get_value(), move |e| e.vban.as_ref().map(f).unwrap_or_default())
    };
    view! {
        <div class="audio-output-row" data-testid="audio-output-row" data-id=row_id>
            <label>
                "Názov"
                <input
                    type="text"
                    data-testid="audio-output-name"
                    prop:value=move || read(entries, &id.get_value(), |e| e.name.clone())
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        edit(entries, &id.get_value(), |e| e.name = v);
                    }
                />
            </label>
            <label>
                <input
                    type="checkbox"
                    data-testid="audio-output-enabled"
                    prop:checked=move || read(entries, &id.get_value(), |e| e.enabled)
                    on:change=move |ev| {
                        let v = event_target_checked(&ev);
                        edit(entries, &id.get_value(), |e| e.enabled = v);
                    }
                />
                "Zapnutý"
            </label>
            <label>
                "Frekvencia"
                <select
                    data-testid="audio-output-rate"
                    prop:value=move || rate_value(read(entries, &id.get_value(), |e| e.rate))
                    on:change=move |ev| {
                        let v = rate_choice(&event_target_value(&ev));
                        edit(entries, &id.get_value(), |e| e.rate = v);
                    }
                >
                    <option value="network">"podľa siete"</option>
                    {SUPPORTED_RATES
                        .iter()
                        .map(|r| view! { <option value=r.to_string()>{format!("{r} Hz")}</option> })
                        .collect_view()}
                </select>
            </label>
            <label>
                "Oneskorenie (ms)"
                <input
                    type="number"
                    min="0"
                    max="2000"
                    data-testid="audio-output-delay"
                    prop:value=move || read(entries, &id.get_value(), |e| e.delay_ms.to_string())
                    on:input=move |ev| {
                        let v = event_target_value(&ev).trim().parse().unwrap_or(0);
                        edit(entries, &id.get_value(), |e| e.delay_ms = v);
                    }
                />
            </label>
            <label>
                "Cieľ (host)"
                <input
                    type="text"
                    data-testid="audio-output-vban-host"
                    placeholder="dev1.lan"
                    prop:value=move || vban(|v| v.host.clone())
                    on:input=move |ev| {
                        let v = event_target_value(&ev).trim().to_string();
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.host = v;
                            }
                        });
                    }
                />
            </label>
            <label>
                "Port"
                <input
                    type="number"
                    min="1"
                    max="65535"
                    data-testid="audio-output-vban-port"
                    prop:value=move || vban(|v| v.port.to_string())
                    on:input=move |ev| {
                        let v = event_target_value(&ev).trim().parse().unwrap_or(0);
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.port = v;
                            }
                        });
                    }
                />
            </label>
            <label>
                "Názov streamu"
                <input
                    type="text"
                    maxlength="16"
                    data-testid="audio-output-vban-stream"
                    prop:value=move || vban(|v| v.stream_name.clone())
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.stream_name = v;
                            }
                        });
                    }
                />
            </label>
            <label>
                "Formát"
                <select
                    data-testid="audio-output-vban-format"
                    prop:value=move || vban(|v| v.format.as_str().to_string())
                    on:change=move |ev| {
                        let v = VbanSampleFormat::parse(&event_target_value(&ev)).unwrap_or_default();
                        edit(entries, &id.get_value(), |e| {
                            if let Some(d) = e.vban.as_mut() {
                                d.format = v;
                            }
                        });
                    }
                >
                    <option value="int16">"16 bitov"</option>
                    <option value="int24">"24 bitov"</option>
                    <option value="float32">"32 bitov (float)"</option>
                </select>
            </label>
            <span
                class="audio-output-state"
                data-testid="audio-output-state"
                title=move || live_reason(&live.get(), &id.get_value())
            >
                {move || live_text(&live.get(), &id.get_value())}
            </span>
            <button
                type="button"
                data-testid="audio-output-remove"
                on:click=move |_| entries.update(|l| l.retain(|e| e.id != id.get_value()))
            >
                "Odobrať"
            </button>
        </div>
    }
}
```

`sp-ui/src/pages/settings.rs`: `use crate::components::{audio_outputs, resolume_hosts, settings_form};` and after `<settings_form::SettingsForm />`: `<hr /><audio_outputs::AudioOutputs />` (outside the form: its buttons are `type="button"`, never a submit).

`sp-ui/src/components/settings_form.rs`: delete the three `vban_*` signals, their two sync lines, their three `settings.insert` lines and the `settings-vban` fieldset; the module doc says the VBAN output moved to "Zvukové výstupy" (`audio_outputs.rs`, #233); and in the save task replace `store.settings.set(settings);` with

```rust
                    // #233: MERGE what this form saved — the outputs section
                    // reads `audio_outputs` from the same map; replacing it
                    // would blank the outputs list (Review Focus 1).
                    store.settings.update(move |s| s.extend(settings));
```

(`settings` is moved into the async block already.)

`crates/sp-core/src/config.rs`: delete `SETTING_VBAN_ENABLED`, `SETTING_VBAN_STREAM_NAME`, `SETTING_VBAN_TARGETS` (+ docs); the test `vban_setting_keys_and_default_stream_name` keeps only `assert_eq!(DEFAULT_VBAN_STREAM_NAME, "sp-program");` and is renamed `the_default_vban_stream_name`.

`e2e/mock-api.mjs`:
- the fixture comment: `// #233: audio_outputs / audio_network_rate are absent (an empty list, 48 kHz).` (replacing the #210 vban_* comment);
- in the settings PATCH handler, BEFORE writing anything (the server refuses the whole PATCH):

```js
// #233: as the server — a bad output list or network rate refuses the whole
// PATCH with 400 and the server's text (the cases the dashboard can send).
const MOCK_RATES = [44100, 48000, 88200, 96000, 192000];
function outputsRefusal(body) {
  if (body.audio_network_rate !== undefined && !MOCK_RATES.includes(Number(String(body.audio_network_rate).trim()))) {
    return `audio_network_rate must be one of ${MOCK_RATES.join(", ")}`;
  }
  if (body.audio_outputs === undefined || String(body.audio_outputs).trim() === "") return null;
  let list;
  try {
    list = JSON.parse(body.audio_outputs);
  } catch {
    return "audio_outputs is not a JSON list (line 1, column 1)";
  }
  if (!Array.isArray(list)) return "audio_outputs is not a JSON list (line 1, column 1)";
  const seen = new Set();
  for (const [i, e] of list.entries()) {
    const at = `entry ${i + 1} (id ${e.id})`;
    if (seen.has(e.id)) return `${at}: id is used by an earlier entry`;
    seen.add(e.id);
    if (e.type === "vban") {
      if (!e.vban || !e.vban.host) return `${at}: vban.host is empty`;
      if (!(e.vban.port >= 1 && e.vban.port <= 65535)) return `${at}: vban.port must be 1-65535`;
    }
  }
  return null;
}
```

  and at the top of `app.patch("/api/v1/settings", …)`: `const refusal = outputsRefusal(req.body); if (refusal) { res.status(400).send(refusal); return; }`;
- in `GET /api/v1/program`: delete the `vban` block and add

```js
    // #233: every audio output (mirrors `audio_out::OutputStatus`) from the
    // stored list, like the server's outputs task; the mock "runs" every
    // enabled entry.
    audio_network_rate: Number(settings.audio_network_rate || 48000),
    outputs_problems: [],
    outputs: mockOutputs(),
```

  with

```js
function mockOutputs() {
  let list = [];
  try {
    list = JSON.parse(settings.audio_outputs || "[]");
  } catch {
    list = [];
  }
  const network = Number(settings.audio_network_rate || 48000);
  return list.map((e) => {
    const rate = e.rate === "network" || e.rate === undefined ? network : Number(e.rate);
    const enabled = e.enabled !== false;
    return {
      id: e.id,
      type: e.type,
      name: e.name,
      enabled,
      state: enabled ? "running" : "disabled",
      reason: null,
      rate,
      format: (e.vban && e.vban.format) || "int24",
      channels: 2,
      delay_ms: e.delay_ms || 0,
      latency_ms: 66.6666 + (e.delay_ms || 0) + (rate === 48000 ? 0 : 1000 / 60),
      blocks_sent: 0,
      blocks_dropped: 0,
    };
  });
}
```

- [ ] **Step 4: Run** — locally: `rustfmt --edition 2024 --check sp-ui/src/components/audio_outputs.rs sp-ui/src/components/settings_form.rs sp-ui/src/pages/settings.rs`; `node --check e2e/mock-api.mjs`. CI: `Build WASM (trunk)` + `Frontend E2E Tests` (the new spec, `slovak-only.spec.ts`, every settings spec) — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): Zvukové výstupy — add/save/reload, Slovak refusal, remove/toggle, the other settings keep the list` (the spec + `git rm settings-vban.spec.ts`) then `feat(#233): Nastavenia "Zvukové výstupy" — the VBAN list, the network rate, live state; the form merges its save` (sp-ui, sp-core config, mock).

### Task 1.11: the FOH and 96 kHz live gates, docs, push

**Files:**
- Create: `e2e/audio-outputs-gate.ts`, `e2e/audio-outputs-gate.spec.ts` (mock suite: pure functions), `e2e/post-deploy-audio-outputs.spec.ts`, `.claude/rules/audio-outputs.md`
- Modify: `.claude/rules/vban-out.md`, `CLAUDE.md` (router line)

**Interfaces:**
- Consumes: `GET /api/v1/program` → `outputs[]`; `GET`/`PATCH /api/v1/settings`.
- Produces: `audio-outputs-gate.ts::{OutputStatus, FOH_TARGET = "fohabl.lan:6980", MIN_BLOCKS = 25, fohPathFailures(first, second): string[], VbanHeader, parseVbanHeader(Uint8Array): VbanHeader | null, ReceiverWant, receiverFailures(packets, want): string[]}` (Lane 3 adds `asioGateFailures`).

- [ ] **Step 1: Write the failing unit spec** — `e2e/audio-outputs-gate.spec.ts`:

```ts
import { test, expect } from "@playwright/test";
import { fohPathFailures, parseVbanHeader, receiverFailures, type OutputStatus } from "./audio-outputs-gate";

const foh = (blocks: number, over: Partial<OutputStatus> = {}): OutputStatus => ({
  id: "out-1", type: "vban", name: "fohabl.lan:6980", enabled: true, state: "running", reason: null,
  rate: 48000, format: "int24", channels: 2, delay_ms: 0, latency_ms: 66.67, blocks_sent: blocks,
  blocks_dropped: 0,
  vban: { stream_name: "sp-program", packets_sent: blocks * 8, send_errors: 0,
    targets: [{ target: "fohabl.lan:6980", addr: "10.77.7.30:6980", error: null }] },
  ...over,
});

function packet(sr: number, frames: number, bit: number, name: string, counter: number): Uint8Array {
  const b = new Uint8Array(28 + frames * 6);
  b.set([0x56, 0x42, 0x41, 0x4e, sr, frames - 1, 1, bit]);
  b.set(Array.from(name, (c) => c.charCodeAt(0)), 8);
  new DataView(b.buffer).setUint32(24, counter, true);
  return b;
}

test.describe("audio outputs gate (#233)", () => {
  test("FOH sending 48 kHz INT24 sp-program passes", () => {
    expect(fohPathFailures([foh(100)], [foh(130)])).toEqual([]);
  });
  test("a missing FOH output fails", () => {
    expect(fohPathFailures([], [])).toEqual(["no VBAN output to fohabl.lan:6980"]);
  });
  test("a FOH output moved off 48 kHz INT24 fails with each reason", () => {
    const moved = foh(130, { rate: 96000, format: "int16", state: "waiting", reason: "no IPv4 address" });
    expect(fohPathFailures([foh(100)], [moved])).toEqual([
      "the FOH output is waiting (no IPv4 address)",
      "the FOH output runs at 96000 Hz, not 48000",
      "the FOH output sends int16, not int24",
    ]);
  });
  test("a FOH output that stalled or errors fails", () => {
    const stalled = foh(110);
    stalled.vban!.send_errors = 3;
    expect(fohPathFailures([foh(100)], [stalled])).toEqual([
      "only 10 FOH blocks went out between the reads",
      "3 FOH send errors between the reads",
    ]);
  });
  test("a VBAN header parses", () => {
    expect(parseVbanHeader(packet(4, 200, 2, "sp-e2e-96k", 7))).toEqual({
      srIndex: 4, subProtocol: 0, frames: 200, channels: 2, formatBit: 2, stream: "sp-e2e-96k", counter: 7,
    });
    expect(parseVbanHeader(new Uint8Array(10))).toBeNull();
  });
  test("a contiguous 96 kHz stream passes; a wrong index or a gap fails", () => {
    const want = { srIndex: 4, frames: 200, formatBit: 2, stream: "sp-e2e-96k", minPackets: 3 };
    const good = [0, 1, 2].map((k) => packet(4, 200, 2, "sp-e2e-96k", 0xffffffff + k));
    expect(receiverFailures(good, want)).toEqual([]);
    expect(receiverFailures([packet(3, 200, 2, "sp-e2e-96k", 0), ...good.slice(1)], want)[0]).toContain("a packet carries");
    const gap = [packet(4, 200, 2, "sp-e2e-96k", 1), packet(4, 200, 2, "sp-e2e-96k", 3), packet(4, 200, 2, "sp-e2e-96k", 4)];
    expect(receiverFailures(gap, want)).toEqual(["the frame counter jumps 1 -> 3"]);
    expect(receiverFailures(good.slice(0, 2), want)).toEqual(["2 packets, want at least 3"]);
  });
});
```

(`0xffffffff + k`: the counter wraps — `setUint32` stores `(0xffffffff + k) >>> 0`; the gate compares with `>>> 0`.)

- [ ] **Step 2: Run** (mock suite, locally or CI): `npx playwright test --project=chromium audio-outputs-gate.spec.ts` — Expected: FAIL (module missing).

- [ ] **Step 3: Implement** — `e2e/audio-outputs-gate.ts`:

```ts
/**
 * #233 post-deploy gates (pure; unit-tested in the mock suite by
 * audio-outputs-gate.spec.ts): FOH still gets SongPlayer's 48 kHz INT24
 * `sp-program` through the output list (the migration kept it byte-identical),
 * and a VBAN destination at another rate carries that rate's index with a
 * contiguous frame counter.
 */

export interface VbanTelemetry {
  stream_name: string;
  packets_sent: number;
  send_errors: number;
  targets: { target: string; addr: string | null; error: string | null }[];
}

export interface OutputStatus {
  id: string;
  type: string;
  name: string;
  enabled: boolean;
  state: string;
  reason: string | null;
  rate: number;
  format: string;
  channels: number;
  delay_ms: number;
  latency_ms: number;
  blocks_sent: number;
  blocks_dropped: number;
  vban?: VbanTelemetry;
}

/** SNV's FOH destination (#210's first target, migrated as out-1). */
export const FOH_TARGET = "fohabl.lan:6980";
/** Blocks that must go out between the two reads (30 per second). */
export const MIN_BLOCKS = 25;

function fohOf(list: OutputStatus[]): OutputStatus | undefined {
  return list.find((o) => o.type === "vban" && o.vban?.targets[0]?.target === FOH_TARGET);
}

export function fohPathFailures(first: OutputStatus[], second: OutputStatus[]): string[] {
  const a = fohOf(first);
  const b = fohOf(second);
  if (!a || !b) return [`no VBAN output to ${FOH_TARGET}`];
  const f: string[] = [];
  if (b.state !== "running") f.push(`the FOH output is ${b.state}${b.reason ? ` (${b.reason})` : ""}`);
  if (b.rate !== 48000) f.push(`the FOH output runs at ${b.rate} Hz, not 48000`);
  if (b.format !== "int24") f.push(`the FOH output sends ${b.format}, not int24`);
  if (b.vban?.stream_name !== "sp-program") f.push(`the FOH stream is ${b.vban?.stream_name}, not sp-program`);
  if (b.delay_ms !== 0) f.push(`the FOH output is delayed ${b.delay_ms} ms`);
  const sent = b.blocks_sent - a.blocks_sent;
  if (sent < MIN_BLOCKS) f.push(`only ${sent} FOH blocks went out between the reads`);
  const errors = (b.vban?.send_errors ?? 0) - (a.vban?.send_errors ?? 0);
  if (errors > 0) f.push(`${errors} FOH send errors between the reads`);
  return f;
}

export interface VbanHeader {
  srIndex: number;
  subProtocol: number;
  frames: number;
  channels: number;
  formatBit: number;
  stream: string;
  counter: number;
}

export function parseVbanHeader(b: Uint8Array): VbanHeader | null {
  if (b.length < 28 || b[0] !== 0x56 || b[1] !== 0x42 || b[2] !== 0x41 || b[3] !== 0x4e) return null;
  const name = b.subarray(8, 24);
  const end = name.indexOf(0);
  return {
    srIndex: b[4] & 0x1f,
    subProtocol: b[4] >> 5,
    frames: b[5] + 1,
    channels: b[6] + 1,
    formatBit: b[7],
    stream: String.fromCharCode(...(end < 0 ? name : name.subarray(0, end))),
    counter: new DataView(b.buffer, b.byteOffset, b.length).getUint32(24, true),
  };
}

export interface ReceiverWant {
  srIndex: number;
  frames: number;
  formatBit: number;
  stream: string;
  minPackets: number;
}

export function receiverFailures(packets: Uint8Array[], want: ReceiverWant): string[] {
  const f: string[] = [];
  const headers = packets.map(parseVbanHeader);
  if (headers.some((h) => h === null)) f.push("a datagram is not VBAN");
  const ok = headers.filter((h): h is VbanHeader => h !== null);
  if (ok.length < want.minPackets) f.push(`${ok.length} packets, want at least ${want.minPackets}`);
  const bad = ok.find(
    (h) =>
      h.srIndex !== want.srIndex ||
      h.subProtocol !== 0 ||
      h.frames !== want.frames ||
      h.channels !== 2 ||
      h.formatBit !== want.formatBit ||
      h.stream !== want.stream,
  );
  if (bad) f.push(`a packet carries ${JSON.stringify(bad)}`);
  for (let i = 1; i < ok.length; i++) {
    if (ok[i].counter !== ((ok[i - 1].counter + 1) >>> 0)) {
      f.push(`the frame counter jumps ${ok[i - 1].counter} -> ${ok[i].counter}`);
      break;
    }
  }
  return f;
}
```

`e2e/post-deploy-audio-outputs.spec.ts`:

```ts
/**
 * #233 post-deploy gates on SNV:
 * 1. FOH's path is unchanged by the output list: the migrated VBAN output to
 *    fohabl.lan:6980 runs at 48 kHz INT24, stream `sp-program`, no delay,
 *    blocks going out, no send errors (two reads, `fohPathFailures`).
 * 2. A VBAN destination at 96 kHz: a receiver ON THE BOX (a UDP socket on
 *    127.0.0.1) reads rate index 4, 200-frame INT24 packets, a contiguous
 *    counter, 480 a second. The test adds a temporary entry `e2e-96k` and
 *    restores the stored list in `finally`; the FOH entries are unchanged by
 *    both PATCHes, so the outputs task keeps them running (Task 1.8).
 * Read-only otherwise. No sleeps: every wait is an expect.poll.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import dgram from "node:dgram";
import { MIN_BLOCKS, fohPathFailures, receiverFailures, type OutputStatus } from "./audio-outputs-gate";

async function outputs(request: APIRequestContext): Promise<OutputStatus[]> {
  const resp = await request.get("/api/v1/program");
  expect(resp.status(), "GET /api/v1/program").toBe(200);
  return (await resp.json()).outputs as OutputStatus[];
}

function fohBlocks(list: OutputStatus[]): number {
  return list.find((o) => o.vban?.targets[0]?.target === "fohabl.lan:6980")?.blocks_sent ?? -1;
}

test.describe("audio outputs (#233)", () => {
  test("FOH still gets 48 kHz INT24 sp-program", async ({ request }) => {
    test.setTimeout(30_000);
    const first = await outputs(request);
    console.log(`[#233 outputs] first: ${JSON.stringify(first)}`);
    const start = fohBlocks(first);
    await expect
      .poll(async () => fohBlocks(await outputs(request)) - start, {
        message: `${MIN_BLOCKS} FOH blocks go out`,
        timeout: 10_000,
      })
      .toBeGreaterThanOrEqual(MIN_BLOCKS);
    const second = await outputs(request);
    console.log(`[#233 outputs] second: ${JSON.stringify(second)}`);
    expect(fohPathFailures(first, second), "the FOH path").toEqual([]);
  });

  test("a 96 kHz VBAN destination reads index 4 and a contiguous counter", async ({ request }) => {
    test.setTimeout(60_000);
    const socket = dgram.createSocket("udp4");
    const packets: Uint8Array[] = [];
    socket.on("message", (m) => packets.push(new Uint8Array(m)));
    await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", () => resolve()));
    const port = socket.address().port;
    const stored = (await (await request.get("/api/v1/settings")).json()).audio_outputs as string | undefined;
    const kept = (JSON.parse(stored && stored.trim() ? stored : "[]") as { id: string }[]).filter(
      (e) => !e.id.startsWith("e2e-"),
    );
    const restore = JSON.stringify(kept);
    const withProbe = [
      ...kept,
      { id: "e2e-96k", name: "E2E 96 kHz", type: "vban", enabled: true, rate: 96000, delay_ms: 0,
        vban: { host: "127.0.0.1", port, stream_name: "sp-e2e-96k", format: "int24" } },
    ];
    try {
      const add = await request.patch("/api/v1/settings", { data: { audio_outputs: JSON.stringify(withProbe) } });
      expect(add.status(), "the probe entry is accepted").toBe(204);
      await expect
        .poll(() => packets.length, { message: "the 96 kHz output starts (the list is re-read every 5 s)", timeout: 20_000 })
        .toBeGreaterThan(0);
      packets.length = 0;
      await expect
        .poll(() => packets.length, { message: "one second of 96 kHz packets", timeout: 10_000 })
        .toBeGreaterThanOrEqual(480);
      const taken = packets.slice(0, 480);
      expect(
        receiverFailures(taken, { srIndex: 4, frames: 200, formatBit: 0x02, stream: "sp-e2e-96k", minPackets: 480 }),
      ).toEqual([]);
    } finally {
      const back = await request.patch("/api/v1/settings", { data: { audio_outputs: restore } });
      socket.close();
      expect(back.status(), "the stored list is restored").toBe(204);
    }
  });
});
```

Docs. `.claude/rules/audio-outputs.md` (new; Lanes 2–3 extend it):

```markdown
---
paths:
  - "crates/sp-core/src/audio_outputs*.rs"
  - "crates/sp-server/src/playback/audio_out*.rs"
  - "crates/sp-server/src/playback/vban_rate*.rs"
  - "crates/sp-server/src/playback/asrc*.rs"
  - "crates/sp-server/src/playback/asio_*.rs"
  - "crates/sp-server/src/api/audio*.rs"
  - "crates/sp-server/src/api/program_tests_outputs.rs"
  - "sp-ui/src/components/audio_outputs.rs"
  - "e2e/settings-audio-outputs.spec.ts"
  - "e2e/audio-outputs-gate*.ts"
  - "e2e/post-deploy-audio-*.spec.ts"
---

# Program audio outputs (#233): one list, VBAN per destination, ASIO with a drift servo

Spec: `docs/superpowers/specs/2026-10-07-audio-outputs-asio-design.md`.
Plan: `docs/superpowers/plans/2026-10-07-audio-outputs-asio.md`.

## The list (`audio_outputs`) and the network rate (`audio_network_rate`)

- ONE setting, a JSON list of `sp_core::audio_outputs::OutputEntry`:
  `{id, name, type, enabled, rate ("network" | Hz), delay_ms (0..=2000),
  vban: {host, port, stream_name, format: int16|int24|float32}}`. Limits: 16
  outputs, 8 VBAN; ids `[a-z0-9-]{1,32}`, unique; `out-N` from the dashboard.
- `audio_network_rate`: 44100 / 48000 / 88200 / 96000 / 192000, default 48000;
  SNV = 96000. An entry at `"network"` runs at it.
- A PATCH is parsed strictly (`audio_out_config::parse_list`): every error
  names the entry, the sanitized id and the field, never the value (serde's
  text can quote input); 400 refuses the whole PATCH. Stored normalized.
- The outputs task (`audio_out_task.rs`) re-reads the list every 5 s,
  leniently: an entry this version cannot read is skipped and named in
  `outputs_problems` (a WARN once); the rest run.
- An entry identical to a running one (and built for the same rate) is KEPT:
  thread, queue, frame counter. Only a new or changed entry is rebuilt — so a
  dashboard save, or the post-deploy probe entry, never disturbs FOH.

## Migration (first start, `audio_out_migrate.rs`)

`vban_enabled` / `vban_stream_name` / `vban_targets` → one entry per target
(first 8), `rate: 48000` (fixed, NEVER "network"), `int24`, the stream name as
#210 put it on the wire; then the keys are deleted, in one transaction. A list
already stored wins (stale keys are only deleted). A rollback to ≤ 0.72 finds
no `vban_*` keys: VBAN is off there until they are PATCHed back.

## The fan-out (`audio_out.rs`)

`ProgramOutput::serve` → `split` → `limit` → `feed_outputs` (ONE `Arc<[f32]>`
copy of the limited block, `ProgramBlock`) → MAX → video side + NDI submit.
Every running output has its own drop-oldest queue and thread; a push never
waits. Pinned: `program_output_tests_order.rs::every_output_has_the_block_before_the_ndi_submit`.

## VBAN per destination (`vban_out.rs`, `vban_packet.rs`, `vban_rate.rs`)

- `VbanFormat`: SR index (48k=3, 96k=4, 192k=5, 44.1k=16, 88.2k=17), sample
  type (INT16 0x01, INT24 0x02, FLOAT32 0x04), packets = the largest divisor
  of `rate/30` within 256 frames and 1436 payload bytes (96k INT24: 16 × 200,
  one every 1/480 s). Packet k at `due + L + delay + k·slot/packets`.
- 48 kHz: no converter, #210's bytes — pinned against a verbatim copy of the
  0.72.0 encoder (`vban_packet_tests_legacy.rs`,
  `vban_out_tests_dest.rs::a_migrated_foh_entry_sends_the_0_72_datagrams`).
- Other rates: rubato `Fft`, both sides fixed (1600 in, `rate/30` out),
  delay `rate/60` frames.
- The queue bound grows with the delay (`queue_bound`); `VbanStallLog`'s
  buckets count packets, so at 96 kHz `late_max_us` covers 30–60 s.

## Telemetry

`GET /api/v1/program` → `outputs[]` `{id, type, name, enabled, state
(running|opening|waiting|disabled), reason, rate, format, channels, delay_ms,
latency_ms, blocks_sent, blocks_dropped, vban: {#210's VbanStatus}}`,
`audio_network_rate`, `outputs_problems`. The top-level `vban` is gone.

## Dashboard (`sp-ui` `audio_outputs.rs`)

Its own section and PATCH (only the two keys); validates with
`sp_core::audio_outputs::validate_list` and shows `ListError::sk()`; the main
form MERGES its save into `store.settings` (replacing it blanked the list).

## Live gates (`e2e/post-deploy-audio-outputs.spec.ts`)

FOH (`fohabl.lan:6980`) still 48 kHz INT24 `sp-program`; a temporary
`e2e-96k` entry to a UDP receiver on 127.0.0.1 reads index 4, 200-frame INT24
packets, a contiguous counter (480/s); `finally` restores the list.
```

`.claude/rules/vban-out.md`: in "Settings + telemetry" replace the three keys' bullets with "Since #233 there are no `vban_*` keys: each destination is an entry of `audio_outputs` (`audio-outputs.md`) with its own format, delay and ONE target; the outputs task resolves and re-resolves it (60 s), keeping the last good address"; the telemetry bullet becomes "`GET /api/v1/program` → `outputs[i].vban` (the same `VbanStatus` fields + `blocks_sent`)"; the UI bullet points at "Zvukové výstupy" (`audio-outputs.md`) and drops `settings-vban*`; the "Wire format" section adds "#233: other rates / formats per destination — `VbanFormat` (`audio-outputs.md`); the bytes above are the 48 kHz INT24 (`PROGRAM`) format, pinned by `vban_packet_tests_legacy.rs`". Add `crates/sp-server/src/playback/audio_out*.rs` to its `paths:`.

`CLAUDE.md` "Playbook router", after the VBAN line:

```markdown
- program audio outputs (#233: ONE list `audio_outputs` + `audio_network_rate` (SNV 96000); a PATCH is parsed strictly, errors name entry + field; the task keeps unchanged entries running and rebuilds changed ones; `vban_*` migrated once to 48 kHz INT24 entries (FOH byte-identical, pinned against the 0.72.0 encoder); one fan-out after the limiter, one queue + thread per output; VBAN per destination: rate index, INT16/24/FLOAT32, packet geometry, delay, rubato `Fft` for ≠ 48 kHz; `outputs[]` on `GET /api/v1/program`; Nastavenia "Zvukové výstupy"; lanes 2–3: the drift servo and ASIO) → `.claude/rules/audio-outputs.md` (auto-loads on `audio_out*`, `vban_rate*`, `asrc*`, `asio_*`, sp-core `audio_outputs*`, the UI section, the specs)
```

- [ ] **Step 4: Run** — mock suite: `audio-outputs-gate.spec.ts` PASS (locally with node + Playwright, or CI's Frontend E2E). The post-deploy spec runs only on the box (CI's E2E job).

- [ ] **Step 5: Commit** — `test(#233): the outputs gate functions (FOH path, VBAN receiver)` then `test(#233): SNV post-deploy — FOH still 48 kHz INT24 sp-program; a 96 kHz destination on the box` then `docs(#233): audio-outputs rules, vban-out update, CLAUDE.md router line`.

- [ ] **Step 6: Lane checks, then push.** `cargo fmt --all --check` (revert `db/models.rs` if touched); `wc -l` of every touched `.rs` ≤ 1000 (expect `program_bus.rs` 971, `vban_out.rs` < 900); `cargo mutants --in-diff <(git diff origin/dev...HEAD) --list` (listing does not compile; Tier-0-allowed) and map every listed mutant to its killing test; `actionlint` not needed (no workflow change). `git push origin dev`; monitor CI to terminal state (Lint, Test, Windows build+tests, WASM, Frontend E2E, Mutation, Build Tauri, Deploy, E2E win-resolume incl. both new post-deploy tests).

- [ ] **Step 7: MAIN SESSION OPS — after the deploy is green:**
  1. Read SNV: `curl -s http://10.77.9.201:8920/api/v1/program | jq '.outputs, .audio_network_rate, .outputs_problems'` → `out-1` (fohabl.lan:6980) and `out-2` (lv1.lan:6980) `running`, 48000, `int24`, `sp-program`; `curl -s http://10.77.9.201:8920/api/v1/settings | jq 'with_entries(select(.key|startswith("vban_")))'` → `{}`; SongPlayer's log has `audio outputs: #210's VBAN settings became entries`.
  2. Set the network rate (spec open point): `curl -s -X PATCH -H 'content-type: application/json' -d '{"audio_network_rate":"96000"}' http://10.77.9.201:8920/api/v1/settings` → 204; read `outputs` twice 5 s apart: the two FOH entries keep 48000 and their `vban.frame_counter` keeps growing without a reset (not rebuilt).
  3. Tell the camera-box session: done, FOH VBAN unchanged.
  4. Post the evidence on #233 (`gh issue comment 233 --body-file …`).

---

## Lane 2 — The drift servo and the ASRC, pure, with the closed-loop simulation

Design comment for #233 (lane start): *Approach:* the ASIO output's resampler is ONE rubato `Async` sinc stage (256 taps, `FixedAsync::Input` of one 1600-frame program block, ~2.7 ms at 96 kHz) whose relative ratio `1 + ppm·1e-6` a servo sets once per block. The servo ports camera-box's `asrc-compensator` (`libobs/media-io/asrc-compensator.{h,c}`; the MIT Rust model `camera-box/src/asrc_bench.rs` `RealtimeAsrcCompensator`) turned around for an OUTPUT: per block it sees, on the program wall, when the block was handled, its boundary, the frames buffered for the card and the frames the card consumed. A least-squares slope of (consumed/rate − handled) over up to 600 s is the card's ppm (applied after 60 s, ≥ 30 one-second points); a PI on the 1 s-window mean of the boundary-to-play latency (P 2 ppm/ms on a 10 s EMA, ±50 ppm; I 0.0002 ppm/(ms·s), ±3 ppm) holds the latency at two grid slots + the entry's delay; the sum is clamped to ±300 ppm and slewed ≤ 5 ppm/s. A block more than one slot off target, or a window mean more than 10 ms off, re-centres at once (a 5 ms fade out, the inserted silence or the skipped frames, a 5 ms fade in: `Splice`); a rate point more than 10 ms off the fit re-bases the regression instead of entering it, so a step never disturbs the rate estimate. Everything is pure and Linux-tested, including a closed-loop simulation (card −50 / 0 / +50 ppm, clock steps ±1 / ±44 ms, a dropped buffer). *Rejected:* libswresample's `swr_set_compensation` as in camera-box (a C dependency; rubato's `Async` adjusts the ratio itself, allocation-free); a fill-only PI without the rate estimate (a ±50 ppm card would sit at the P clamp's edge, research §1); camera-box's restore burst and 1000 ppm step pay-back (an output re-centres under a fade instead: its buffer is ours, not OBS's mixer); a latency target of "2–3 driver buffers" from the spec (the program hands one 33 ms block per boundary, 10–33 ms late in normal operation — that target underruns on every block; two slots is VBAN's proven budget). *Architektúra:* `playback::asrc_servo` (pure, f64 + integer 100 ns where a boundary is pinned) + `playback::asrc` (`Asrc` over rubato 5.0.1 `Async`, `Splice`); no runtime wiring in this lane (Lane 3's ASIO worker calls them).

A scratch Python model of exactly this servo (dev1 scratchpad of the planning session, `model/servo_sim.py`; the lane worker writes its own, `rust-workspace.md`) gives, over 900 s at 96 kHz / 128-frame buffers with 0–15 ms hand-off jitter: 0 underruns in every case; final correction −50.5 / 0.75 / 52.0 ppm for cards at −50 / 0 / +50; ≤ 5 ppm per second of wall; latency within ±15 ms of target after 70 s; 1 re-centre (the start) for ±1 ms steps, 2 for ±44 ms steps; minimum ring fill 22 ms after a −44 ms step (16.9 ms with 30 ms jitter).

### Task 2.1: `asrc_servo` — the constants, the rate regression, the level loop, the slew

**Files:**
- Create: `crates/sp-server/src/playback/asrc_servo.rs`, `crates/sp-server/src/playback/asrc_servo_tests.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (`pub mod asrc_servo; // #233: the ASIO output's drift servo (camera-box's asrc-compensator, for an output), pure`)

**Interfaces:**
- Consumes: nothing (pure).
- Produces: `asrc_servo::{MAX_PPM, MAX_SLEW_PPM_PER_S, REGRESSION_SPAN_S, REGRESSION_MIN_POINTS, REGRESSION_LOCK_SPAN_S, REGRESSION_CAP, WINDOW_100NS, STEP_RESIDUAL_S, LEVEL_KP_PPM_PER_MS, LEVEL_KP_MAX_PPM, LEVEL_EMA_TAU_S, LEVEL_KI_PPM_PER_MS_S, LEVEL_INTEGRAL_MAX_PPM, MAX_SANE_WINDOW_PPM, GROSS_STEP_100NS, RECENTRE_100NS, BASE_LATENCY_100NS, struct RateRegression (offer(x_s, y_s) -> Offered, rate_ppm, locked, len, flush), enum Offered { Inserted, Rebased, Restarted }, struct LevelLoop (update(err_ms, dt_s, rate_ppm) -> f64, reset_error, ema_ms, integral_ppm), slew(applied, target, dt_s) -> f64}`.

- [ ] **Step 1: Write the failing tests** — `asrc_servo_tests.rs` (part 1; Task 2.2 appends):

```rust
//! #233: the drift servo — its constants are camera-box's, the regression's
//! lock / eviction / re-base / restart edges, the level loop's clamps and
//! anti-windup, the slew. Pins derived with a scratch model.

use super::*;

#[test]
fn the_constants_are_camera_boxs() {
    // camera-box src/asrc_bench.rs (MIT) and vendor/obs-studio/libobs/media-io/asrc-compensator.h
    assert_eq!(MAX_PPM, 300.0); // :204 / .h:42
    assert_eq!(MAX_SLEW_PPM_PER_S, 5.0); // :209 / .h:48
    assert_eq!(REGRESSION_SPAN_S, 600.0); // :223 / .h:62
    assert_eq!(REGRESSION_MIN_POINTS, 30); // :228 / .h:67
    assert_eq!(REGRESSION_LOCK_SPAN_S, 60.0); // :235 / .h:76
    assert_eq!(REGRESSION_CAP, 640); // :243 / .h:82
    assert_eq!(WINDOW_100NS, 10_000_000); // WINDOW_S 1.0, :470 / .h:120
    assert_eq!(STEP_RESIDUAL_S, 0.010); // STEP_RESIDUAL_MS 10, :284 / .h:149
    assert_eq!(LEVEL_KP_PPM_PER_MS, 2.0); // :373 / .h:232
    assert_eq!(LEVEL_KP_MAX_PPM, 50.0); // :380 / .h:239
    assert_eq!(LEVEL_EMA_TAU_S, 10.0); // :392 / .h:251
    assert_eq!(LEVEL_KI_PPM_PER_MS_S, 0.0002); // :264 / .h:130
    assert_eq!(LEVEL_INTEGRAL_MAX_PPM, 3.0); // :271 / .h:137
    assert_eq!(MAX_SANE_WINDOW_PPM, 100_000.0); // :447 / .h:109
    // SongPlayer's own: one grid slot, VBAN's send budget, the window step.
    assert_eq!(GROSS_STEP_100NS, sp_core::genlock::UNITS_PER_SECOND / sp_core::genlock::GENLOCK_GRID_FPS);
    assert_eq!(BASE_LATENCY_100NS, crate::playback::vban_packet::VBAN_SEND_LATENCY_100NS);
    assert_eq!(RECENTRE_100NS, 100_000, "10 ms, camera-box's step residual");
}

/// `n` points `step_s` apart on y = ppm·1e-6·x + 0.5.
fn line(r: &mut RateRegression, n: usize, step_s: f64, ppm: f64) {
    for i in 0..n {
        let x = i as f64 * step_s;
        assert_eq!(r.offer(x, ppm * 1e-6 * x + 0.5), Offered::Inserted, "point {i}");
    }
}

#[test]
fn the_rate_locks_at_30_points_spanning_60_s() {
    let mut r = RateRegression::default();
    line(&mut r, 30, 2.0, 20.0); // 30 points over 58 s
    assert!(!r.locked());
    assert_eq!(r.rate_ppm(), 0.0, "no rate before the lock");
    let mut r = RateRegression::default();
    line(&mut r, 29, 3.0, 20.0); // 29 points over 84 s
    assert!(!r.locked());
    let mut r = RateRegression::default();
    line(&mut r, 31, 2.0, 20.0); // 31 points over exactly 60 s
    assert!(r.locked());
    assert!((r.rate_ppm() - 20.0).abs() < 1e-6, "{}", r.rate_ppm());
}

#[test]
fn old_points_leave_by_span_and_by_cap() {
    let mut r = RateRegression::default();
    line(&mut r, 700, 1.0, -30.0);
    assert_eq!(r.len(), 601, "600 s of 1 s points");
    let mut r = RateRegression::default();
    line(&mut r, 700, 0.5, -30.0);
    assert_eq!(r.len(), REGRESSION_CAP);
    assert!((r.rate_ppm() + 30.0).abs() < 1e-6);
}

#[test]
fn a_step_after_the_lock_rebases_and_keeps_the_rate() {
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    let rate = r.rate_ppm();
    assert_eq!(r.offer(100.0, 20e-6 * 100.0 + 0.5 + 0.044), Offered::Rebased);
    assert_eq!(r.offer(101.0, 20e-6 * 101.0 + 0.5 + 0.044), Offered::Inserted, "the new line is the old one");
    assert!((r.rate_ppm() - rate).abs() < 1e-6, "the rate is not disturbed");
}

/// `n` points one second apart on y = 0 (exact in f64, so a residual of
/// exactly `STEP_RESIDUAL_S` can be pinned; `0.5 + 0.010 − 0.5` is not 0.010).
fn flat(r: &mut RateRegression, n: usize) {
    for i in 0..n {
        assert_eq!(r.offer(i as f64, 0.0), Offered::Inserted);
    }
}

#[test]
fn a_residual_of_exactly_10_ms_is_a_point_and_more_is_a_step() {
    let mut r = RateRegression::default();
    flat(&mut r, 40);
    assert_eq!(r.offer(40.0, 0.010), Offered::Inserted);
    let mut r = RateRegression::default();
    flat(&mut r, 40);
    assert_eq!(r.offer(40.0, 0.0101), Offered::Rebased);
}

#[test]
fn a_jump_before_the_lock_restarts_the_regression() {
    let mut r = RateRegression::default();
    flat(&mut r, 5);
    assert_eq!(r.offer(5.0, 0.020), Offered::Restarted);
    assert_eq!(r.len(), 1);
    let mut r = RateRegression::default();
    flat(&mut r, 5);
    assert_eq!(r.offer(5.0, 0.010), Offered::Inserted, "10 ms is still a point");
    r.flush();
    assert_eq!(r.len(), 0);
}

#[test]
fn points_at_one_instant_give_no_slope() {
    let mut r = RateRegression::default();
    for _ in 0..40 {
        r.offer(5.0, 0.5);
    }
    assert_eq!(r.rate_ppm(), 0.0);
}

#[test]
fn the_level_loop_p_is_a_smoothed_clamped_2_ppm_per_ms() {
    let mut l = LevelLoop::default();
    let out = l.update(10.0, 1.0, 0.0);
    let alpha = 1.0 / 11.0;
    assert!((l.ema_ms() - 10.0 * alpha).abs() < 1e-12);
    assert!((out - (2.0 * 10.0 * alpha + 0.0002 * 10.0)).abs() < 1e-12, "P + I");
    let mut l = LevelLoop::default();
    for _ in 0..200 {
        l.update(40.0, 1.0, 0.0);
    }
    assert!((l.update(40.0, 1.0, 0.0) - (50.0 + 1.6)).abs() < 0.1, "P at its 50 ppm clamp, I growing");
}

#[test]
fn the_integral_is_clamped_and_frozen_while_the_sum_saturates() {
    let mut l = LevelLoop::default();
    for _ in 0..10_000 {
        l.update(100.0, 1.0, 0.0);
    }
    assert_eq!(l.integral_ppm(), LEVEL_INTEGRAL_MAX_PPM);
    let mut l = LevelLoop::default();
    for _ in 0..10_000 {
        l.update(-100.0, 1.0, 0.0);
    }
    assert_eq!(l.integral_ppm(), -LEVEL_INTEGRAL_MAX_PPM);
    let mut frozen = LevelLoop::default();
    frozen.update(10.0, 1.0, 290.0); // 290 + P 1.8 + 0 < 300: I moves
    let moved = frozen.integral_ppm();
    assert!(moved > 0.0);
    frozen.update(10.0, 1.0, 299.0); // 299 + P + I ≥ 300: I frozen
    assert_eq!(frozen.integral_ppm(), moved);
    l.reset_error();
    assert_eq!(l.ema_ms(), 0.0);
}

#[test]
fn the_slew_is_5_ppm_per_second_and_never_backwards_in_time() {
    assert_eq!(slew(0.0, 100.0, 1.0), 5.0);
    assert_eq!(slew(0.0, 100.0, 0.5), 2.5);
    assert_eq!(slew(10.0, 7.0, 1.0), 7.0);
    assert_eq!(slew(10.0, -100.0, 2.0), 0.0);
    assert_eq!(slew(10.0, 100.0, -1.0), 10.0, "a backward wall moves nothing");
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server asrc_servo` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement** — `asrc_servo.rs` (part 1; Task 2.2 adds `Servo`):

```rust
//! #233: the ASIO output's drift servo — camera-box's `asrc-compensator`
//! (libobs `media-io/asrc-compensator.{h,c}`; its MIT Rust model
//! `camera-box/src/asrc_bench.rs` `RealtimeAsrcCompensator`; line numbers
//! below are camera-box at `73db0b232`) turned around for an OUTPUT. The producer is SongPlayer's program (the genlock wall,
//! dantesync); the consumer is the card's clock (Dante PTP, SoundGrid). Pure:
//! the ASIO worker (`asio_out.rs`) feeds it one observation per program block
//! and gives its answer to the resampler (`asrc.rs`).
//!
//! Per block, on the program wall: when the block was handled, its boundary,
//! the frames buffered for the card (the ring + the splice's hold) and the
//! frames the card consumed so far.
//! - latency = buffered / rate + (handled − boundary): a boundary's time to
//!   its sound leaving SongPlayer. Target: two grid slots (VBAN's send budget)
//!   + the entry's delay.
//! - rate point = (handled, consumed / rate − handled) per 1 s window (the
//!   window's means): the least-squares slope over up to 600 s is the card's
//!   ppm against the wall, used once 30 points span 60 s (camera-box #1084).
//! - level loop: P 2 ppm/ms on a 10 s EMA of the window-mean latency error,
//!   ±50 ppm; I 0.0002 ppm/(ms·s), ±3 ppm, frozen while rate + P + I
//!   saturates (camera-box #1335 follow-up 5).
//! - output: clamp(rate + P + I, ±300 ppm), moved at most 5 ppm per second of
//!   the wall (camera-box #803: inaudible).
//! - steps: a block more than one slot off target, or a window mean more than
//!   10 ms off, re-centres at once (the worker inserts or skips under fades,
//!   `asrc::Splice`); a rate point more than 10 ms off the fit re-bases the
//!   regression (#1335 follow-up 2): a step never disturbs the rate estimate.
//!
//! Sign: a POSITIVE correction makes MORE output per input — the card runs
//! fast, or the buffer is low. rubato's relative ratio is `1 + ppm·1e-6`.

use std::collections::VecDeque;

/// Bound on the applied correction, ppm (camera-box `asrc_bench.rs:204`, `asrc-compensator.h:42`).
pub const MAX_PPM: f64 = 300.0;
/// The applied correction moves at most this per second of wall (`:209`, `.h:48`).
pub const MAX_SLEW_PPM_PER_S: f64 = 5.0;
/// The rate regression's span, s (`:223`, `.h:62`).
pub const REGRESSION_SPAN_S: f64 = 600.0;
/// Points before a slope is used (`:228`, `.h:67`).
pub const REGRESSION_MIN_POINTS: usize = 30;
/// Span before a slope is used, s (`:235`, `.h:76`).
pub const REGRESSION_LOCK_SPAN_S: f64 = 60.0;
/// Points kept at most (`:243`, `.h:82`).
pub const REGRESSION_CAP: usize = 640;
/// The measurement window, 100 ns (`WINDOW_S` 1.0, `:470`, `.h:120`).
pub const WINDOW_100NS: i64 = 10_000_000;
/// A rate point this far off the fit is a step, s (`STEP_RESIDUAL_MS` 10, `:284`, `.h:149`).
pub const STEP_RESIDUAL_S: f64 = 0.010;
/// The level loop's P gain, ppm per ms (`:373`, `.h:232`).
pub const LEVEL_KP_PPM_PER_MS: f64 = 2.0;
/// The P term's clamp, ppm (`:380`, `.h:239`).
pub const LEVEL_KP_MAX_PPM: f64 = 50.0;
/// The EMA that smooths the level error before P, s (`:392`, `.h:251`).
pub const LEVEL_EMA_TAU_S: f64 = 10.0;
/// The level loop's I gain, ppm per (ms · s) (`:264`, `.h:130`).
pub const LEVEL_KI_PPM_PER_MS_S: f64 = 0.0002;
/// The I term's clamp, ppm (`:271`, `.h:137`).
pub const LEVEL_INTEGRAL_MAX_PPM: f64 = 3.0;
/// A window measuring more than this is starved, not a clock (`:447`, `.h:109`).
pub const MAX_SANE_WINDOW_PPM: f64 = 100_000.0;
/// SongPlayer's: one block this far off target re-centres at once — one grid
/// slot (a literal, pinned against the genlock grid).
pub const GROSS_STEP_100NS: i64 = 333_333;
/// SongPlayer's: a window mean this far off target re-centres (10 ms,
/// camera-box's step residual).
pub const RECENTRE_100NS: i64 = 100_000;
/// SongPlayer's: the latency target before the entry's delay — two grid
/// slots, VBAN's send budget (`VBAN_SEND_LATENCY_100NS`, pinned by a test).
pub const BASE_LATENCY_100NS: i64 = 666_666;

/// What [`RateRegression::offer`] did with a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offered {
    Inserted,
    /// A step after the lock: the line moved, the slope kept.
    Rebased,
    /// A step before the lock: the points start over.
    Restarted,
}

/// The card's rate: ordinary least squares over up to [`REGRESSION_SPAN_S`].
#[derive(Debug, Default)]
pub struct RateRegression {
    points: VecDeque<(f64, f64)>,
    offset_s: f64,
}

impl RateRegression {
    pub fn len(&self) -> usize {
        self.points.len()
    }

    fn span_s(&self) -> f64 {
        match (self.points.front(), self.points.back()) {
            (Some(a), Some(b)) => b.0 - a.0,
            _ => 0.0,
        }
    }

    /// (slope, intercept, x origin), the slope per second of x.
    fn fit(&self) -> Option<(f64, f64, f64)> {
        let x0 = self.points.front()?.0;
        let n = self.points.len() as f64;
        let mx = self.points.iter().map(|p| p.0 - x0).sum::<f64>() / n;
        let my = self.points.iter().map(|p| p.1).sum::<f64>() / n;
        let sxx: f64 = self.points.iter().map(|p| (p.0 - x0 - mx).powi(2)).sum();
        if sxx == 0.0 {
            return None;
        }
        let sxy: f64 = self.points.iter().map(|p| (p.0 - x0 - mx) * (p.1 - my)).sum();
        let slope = sxy / sxx;
        Some((slope, my - slope * mx, x0))
    }

    pub fn locked(&self) -> bool {
        self.points.len() >= REGRESSION_MIN_POINTS && self.span_s() >= REGRESSION_LOCK_SPAN_S
    }

    /// The card's ppm against the wall once locked, else 0.
    pub fn rate_ppm(&self) -> f64 {
        if !self.locked() {
            return 0.0;
        }
        self.fit().map_or(0.0, |(slope, _, _)| slope * 1e6)
    }

    pub fn offer(&mut self, x_s: f64, y_raw_s: f64) -> Offered {
        let y = y_raw_s - self.offset_s;
        if self.points.len() >= REGRESSION_MIN_POINTS
            && let Some((slope, intercept, x0)) = self.fit()
        {
            let residual = y - (intercept + slope * (x_s - x0));
            if residual.abs() > STEP_RESIDUAL_S {
                self.offset_s += residual;
                return Offered::Rebased;
            }
        } else if let Some(&(_, last)) = self.points.back()
            && (y - last).abs() > STEP_RESIDUAL_S
        {
            self.flush();
            self.points.push_back((x_s, y_raw_s));
            return Offered::Restarted;
        }
        self.points.push_back((x_s, y));
        // Shrinks the deque, so it always ends (a mutant can only empty it).
        while self.span_s() > REGRESSION_SPAN_S || self.points.len() > REGRESSION_CAP {
            self.points.pop_front();
        }
        Offered::Inserted
    }

    pub fn flush(&mut self) {
        self.points.clear();
        self.offset_s = 0.0;
    }
}

/// The level loop's P + I (camera-box #1335 follow-up 5).
#[derive(Debug, Default)]
pub struct LevelLoop {
    ema_ms: f64,
    integral_ppm: f64,
}

impl LevelLoop {
    /// P + I after one window of `dt_s` whose latency error is `err_ms`
    /// (target − latency: positive = too little buffered = more output).
    pub fn update(&mut self, err_ms: f64, dt_s: f64, rate_ppm: f64) -> f64 {
        let alpha = dt_s / (LEVEL_EMA_TAU_S + dt_s);
        self.ema_ms += alpha * (err_ms - self.ema_ms);
        let p = (LEVEL_KP_PPM_PER_MS * self.ema_ms).clamp(-LEVEL_KP_MAX_PPM, LEVEL_KP_MAX_PPM);
        if (rate_ppm + p + self.integral_ppm).abs() < MAX_PPM {
            self.integral_ppm = (self.integral_ppm + LEVEL_KI_PPM_PER_MS_S * err_ms * dt_s)
                .clamp(-LEVEL_INTEGRAL_MAX_PPM, LEVEL_INTEGRAL_MAX_PPM);
        }
        p + self.integral_ppm
    }

    /// After a re-centre: the error the EMA held is gone.
    pub fn reset_error(&mut self) {
        self.ema_ms = 0.0;
    }

    pub fn ema_ms(&self) -> f64 {
        self.ema_ms
    }

    pub fn integral_ppm(&self) -> f64 {
        self.integral_ppm
    }
}

/// `applied` moved toward `target` by at most [`MAX_SLEW_PPM_PER_S`] × `dt_s`
/// (a backward wall moves nothing).
pub fn slew(applied: f64, target: f64, dt_s: f64) -> f64 {
    let step = MAX_SLEW_PPM_PER_S * dt_s.max(0.0);
    applied + (target - applied).clamp(-step, step)
}

#[cfg(test)]
#[path = "asrc_servo_tests.rs"]
mod tests;
```

Note: `sxx == 0.0` instead of `<= 0.0`: a sum of squares is never negative, so `<=` would leave an equivalent `<` mutant; `==` → `!=` is killed by `points_at_one_instant_give_no_slope`.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server asrc_servo` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the servo's parts — camera-box constants, regression edges, level loop, slew` then `feat(#233): asrc_servo — rate regression, level loop, slew (camera-box asrc-compensator, for an output)`.

### Task 2.2: `Servo::observe` — one observation per program block

**Files:**
- Modify: `crates/sp-server/src/playback/asrc_servo.rs` (append), `crates/sp-server/src/playback/asrc_servo_tests.rs` (append)

**Interfaces:**
- Consumes: Task 2.1.
- Produces: `asrc_servo::{struct Observation { handled_100ns: i64, stamp_100ns: i64, buffered_frames: u64, consumed_frames: u64 }, struct ServoAction { correction_ppm: f64, recentre_100ns: i64 }, struct ServoStatus { correction_ppm, rate_ppm, locked: bool, latency_ms, recentres: u64, rebases: u64 }, struct Servo (new(device_rate_hz: f64, target_latency_100ns: i64), observe(Observation) -> ServoAction, status() -> ServoStatus, target_100ns()), frames_to_100ns(frames: u64, rate_hz: f64) -> i64}`.

- [ ] **Step 1: Write the failing tests** (append to `asrc_servo_tests.rs`):

```rust
const RATE: f64 = 96_000.0;
const T0: i64 = 17_900_000_000_000_000;

/// A block handled `late_100ns` after its boundary `k`, with `buffered`
/// frames waiting and the card at `consumed`.
fn obs(k: i64, late_100ns: i64, buffered: u64, consumed: u64) -> Observation {
    Observation {
        handled_100ns: T0 + k * GROSS_STEP_100NS + late_100ns,
        stamp_100ns: T0 + k * GROSS_STEP_100NS,
        buffered_frames: buffered,
        consumed_frames: consumed,
    }
}

#[test]
fn frames_convert_to_100ns() {
    assert_eq!(frames_to_100ns(96_000, RATE), 10_000_000);
    assert_eq!(frames_to_100ns(128, RATE), 13_333);
    assert_eq!(frames_to_100ns(0, RATE), 0);
}

#[test]
fn the_first_block_re_centres_from_empty_to_the_target() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    let a = s.observe(obs(0, 50_000, 0, 0));
    assert_eq!(a, ServoAction { correction_ppm: 0.0, recentre_100ns: BASE_LATENCY_100NS - 50_000 });
    assert_eq!(s.status().recentres, 1);
}

#[test]
fn one_block_exactly_a_slot_off_is_not_gross_and_one_more_is() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 0, 0)); // the start re-centre
    // buffered 0, handled − stamp = 333_333: latency 333_333, error exactly one slot
    let a = s.observe(obs(1, 333_333, 0, 3_200));
    assert_eq!(a.recentre_100ns, 0);
    // handled − stamp = 333_332: error 333_334
    let b = s.observe(obs(2, 333_332, 0, 6_400));
    assert_eq!(b.recentre_100ns, 333_334);
    assert_eq!(s.status().recentres, 2);
}

/// Blocks `from..from + n` handled `late_100ns` after their boundary with
/// `buffered` frames waiting, the card at `ppm`; the last action.
fn steady(s: &mut Servo, from: i64, n: i64, late_100ns: i64, buffered: u64, ppm: f64) -> ServoAction {
    let mut last = ServoAction { correction_ppm: 0.0, recentre_100ns: 0 };
    for k in from..from + n {
        let elapsed_s = (k * GROSS_STEP_100NS) as f64 / 1e7;
        let consumed = (elapsed_s * RATE * (1.0 + ppm * 1e-6)) as u64;
        last = s.observe(obs(k, late_100ns, buffered, consumed));
    }
    last
}

#[test]
fn a_window_mean_exactly_10_ms_off_is_held_and_more_re_centres() {
    // 5_440 frames = 566_667 (100 ns) at 96 kHz; handled 1 before the
    // boundary: latency 566_666, exactly RECENTRE_100NS under the target.
    // 32 blocks: the window opened at block 1 closes at block 32 (span ≥ 1 s).
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    assert_eq!(steady(&mut s, 1, 32, -1, 5_440, 0.0).recentre_100ns, 0);
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    assert_eq!(steady(&mut s, 1, 32, -2, 5_440, 0.0).recentre_100ns, 100_001);
    assert_eq!(s.status().recentres, 2);
}

#[test]
fn a_fast_card_gets_a_positive_correction_once_locked() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    steady(&mut s, 1, 30 * 90, 0, 6_400, 40.0); // 90 s
    let st = s.status();
    assert!(st.locked);
    assert!((st.rate_ppm - 40.0).abs() < 0.5, "{st:?}");
    assert!(st.correction_ppm > 0.0, "more output for a fast card");
    assert!(st.correction_ppm <= 5.0 * 92.0, "slewed");
}

#[test]
fn a_starved_window_flushes_the_rate_and_holds_the_correction() {
    let mut s = Servo::new(RATE, BASE_LATENCY_100NS);
    s.observe(obs(0, 0, 6_400, 0));
    steady(&mut s, 1, 30 * 70, 0, 6_400, 20.0);
    let before = s.status();
    assert!(before.locked);
    // The card stops consuming for two windows (a vanished driver): the
    // second window measures −1e6 ppm.
    let frozen = (70.0 * RATE * (1.0 + 20e-6)) as u64;
    let mut held = ServoAction { correction_ppm: 0.0, recentre_100ns: 0 };
    for k in 30 * 70 + 1..30 * 70 + 1 + 62 {
        held = s.observe(obs(k, 0, 6_400, frozen));
    }
    assert!(!s.status().locked, "the regression starts over");
    assert!((held.correction_ppm - before.correction_ppm).abs() < 0.5, "held, not chased: {held:?} {before:?}");
}

(The worker never feeds a vanished driver for long — Lane 3's stall watch closes it after 2 s — but the servo must not rail if it is fed one.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server asrc_servo` — Expected now: FAIL to compile (`Servo` missing).

- [ ] **Step 3: Implement** (append to `asrc_servo.rs`, before the test hook):

```rust
/// One observation, taken by the worker as it handles a program block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    /// When the block was handled, on the program wall (100 ns).
    pub handled_100ns: i64,
    /// The block's boundary (its stamp).
    pub stamp_100ns: i64,
    /// Frames buffered for the card: the ring + the splice's hold.
    pub buffered_frames: u64,
    /// Frames the card consumed since the output opened.
    pub consumed_frames: u64,
}

/// What the worker does with the block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ServoAction {
    /// The resampler's correction, ppm (relative ratio `1 + ppm·1e-6`).
    pub correction_ppm: f64,
    /// Insert (> 0) or skip (< 0) this much before the block, 100 ns.
    pub recentre_100ns: i64,
}

/// The servo's state for the status (`outputs[i].asio`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ServoStatus {
    pub correction_ppm: f64,
    pub rate_ppm: f64,
    pub locked: bool,
    /// The last window's mean latency (boundary → leaving SongPlayer), ms.
    pub latency_ms: f64,
    pub recentres: u64,
    pub rebases: u64,
}

/// `frames` at `rate_hz` in 100 ns, rounded.
pub fn frames_to_100ns(frames: u64, rate_hz: f64) -> i64 {
    (frames as f64 * 1e7 / rate_hz).round() as i64
}

#[derive(Debug, Default)]
struct Window {
    n: i64,
    first: Option<(i64, f64)>,
    sum_x_s: f64,
    sum_y_s: f64,
    sum_latency_100ns: i64,
}

struct Closed {
    x_mean_s: f64,
    y_mean_s: f64,
    x_end_100ns: i64,
    span_100ns: i64,
    latency_mean_100ns: i64,
    ppm: f64,
}

impl Window {
    /// Add one observation (`x` relative to the servo's origin); a window
    /// spanning [`WINDOW_100NS`] closes and starts over.
    fn add(&mut self, x_100ns: i64, y_s: f64, latency_100ns: i64) -> Option<Closed> {
        let (x0, y0) = *self.first.get_or_insert((x_100ns, y_s));
        self.n += 1;
        self.sum_x_s += x_100ns as f64 / 1e7;
        self.sum_y_s += y_s;
        self.sum_latency_100ns += latency_100ns;
        let span_100ns = x_100ns - x0;
        if span_100ns < WINDOW_100NS {
            return None;
        }
        let n = self.n as f64;
        let closed = Closed {
            x_mean_s: self.sum_x_s / n,
            y_mean_s: self.sum_y_s / n,
            x_end_100ns: x_100ns,
            span_100ns,
            latency_mean_100ns: self.sum_latency_100ns / self.n,
            ppm: (y_s - y0) / (span_100ns as f64 / 1e7) * 1e6,
        };
        *self = Self::default();
        Some(closed)
    }
}

/// The servo of one ASIO output (see the module doc).
pub struct Servo {
    rate_hz: f64,
    target_100ns: i64,
    origin_100ns: Option<i64>,
    window: Window,
    regression: RateRegression,
    level: LevelLoop,
    applied_ppm: f64,
    last_apply_100ns: Option<i64>,
    status: ServoStatus,
}

impl Servo {
    /// For a card at `device_rate_hz`, holding `target_latency_100ns`
    /// ([`BASE_LATENCY_100NS`] + the entry's delay).
    pub fn new(device_rate_hz: f64, target_latency_100ns: i64) -> Self {
        Self {
            rate_hz: device_rate_hz,
            target_100ns: target_latency_100ns,
            origin_100ns: None,
            window: Window::default(),
            regression: RateRegression::default(),
            level: LevelLoop::default(),
            applied_ppm: 0.0,
            last_apply_100ns: None,
            status: ServoStatus::default(),
        }
    }

    pub fn target_100ns(&self) -> i64 {
        self.target_100ns
    }

    pub fn status(&self) -> ServoStatus {
        ServoStatus { correction_ppm: self.applied_ppm, ..self.status }
    }

    pub fn observe(&mut self, o: Observation) -> ServoAction {
        let latency_100ns =
            frames_to_100ns(o.buffered_frames, self.rate_hz) + (o.handled_100ns - o.stamp_100ns);
        let err_100ns = self.target_100ns - latency_100ns;
        let Some(origin) = self.origin_100ns else {
            self.origin_100ns = Some(o.handled_100ns);
            return self.recentre(err_100ns);
        };
        if err_100ns.abs() > GROSS_STEP_100NS {
            return self.recentre(err_100ns);
        }
        let x_100ns = o.handled_100ns - origin;
        let y_s = o.consumed_frames as f64 / self.rate_hz - x_100ns as f64 / 1e7;
        let Some(w) = self.window.add(x_100ns, y_s, latency_100ns) else {
            return self.hold();
        };
        if w.ppm.abs() > MAX_SANE_WINDOW_PPM {
            self.regression.flush();
            return self.hold();
        }
        if self.regression.offer(w.x_mean_s, w.y_mean_s) == Offered::Rebased {
            self.status.rebases += 1;
        }
        self.status.latency_ms = w.latency_mean_100ns as f64 / 10_000.0;
        let mean_err_100ns = self.target_100ns - w.latency_mean_100ns;
        if mean_err_100ns.abs() > RECENTRE_100NS {
            return self.recentre(mean_err_100ns);
        }
        let dt_100ns = self.last_apply_100ns.map_or(w.span_100ns, |prev| w.x_end_100ns - prev);
        self.last_apply_100ns = Some(w.x_end_100ns);
        let dt_s = dt_100ns as f64 / 1e7;
        let rate = self.regression.rate_ppm();
        let pi = self.level.update(mean_err_100ns as f64 / 10_000.0, dt_s, rate);
        let target = (rate + pi).clamp(-MAX_PPM, MAX_PPM);
        self.applied_ppm = slew(self.applied_ppm, target, dt_s);
        self.status.rate_ppm = rate;
        self.status.locked = self.regression.locked();
        self.hold()
    }

    fn hold(&self) -> ServoAction {
        ServoAction { correction_ppm: self.applied_ppm, recentre_100ns: 0 }
    }

    fn recentre(&mut self, err_100ns: i64) -> ServoAction {
        self.window = Window::default();
        self.level.reset_error();
        self.status.recentres += 1;
        ServoAction { correction_ppm: self.applied_ppm, recentre_100ns: err_100ns }
    }
}
```

The `the_first_block_re_centres…` pin: `T0 + 0 + 50_000` handled, stamp `T0`, buffered 0 → latency 50_000 → `recentre_100ns = 666_666 − 50_000` ✓. 
- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server asrc_servo` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): Servo::observe — start re-centre, slot and 10 ms edges, a fast card, a starved window` then `feat(#233): Servo — one observation per block: latency target, rate lock, PI, clamp, slew, re-centre`.

### Task 2.3: `Asrc` (rubato `Async` sinc) and `Splice` (the re-centre under fades)

**Files:**
- Create: `crates/sp-server/src/playback/asrc.rs`, `crates/sp-server/src/playback/asrc_tests.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (`pub mod asrc; // #233: the ASIO output's resampler (rubato Async sinc) + the re-centre splice`)

**Interfaces:**
- Consumes: rubato `=5.0.1` (Lane 1), `vban_packet::{VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS}`, `sp_core::audio_outputs::PROGRAM_RATE`.
- Produces: `asrc::{ASRC_SINC_LEN = 256, ASRC_MAX_RELATIVE = 1.001, SPLICE_FADE_S = 0.005, struct Asrc (new(device_rate_hz: f64) -> Result<Self, String>, set_correction_ppm(f64) -> Result<(), String>, process(&mut self, block: &[f32]) -> Result<&[f32], String>, delay_frames(), max_out_frames()), struct Splice (new(device_rate_hz, max_insert_frames, max_block_frames), insert(frames), skip(frames), process(&mut self, input: &[f32]) -> &[f32], held_frames())}`.

- [ ] **Step 1: Write the failing tests** — `asrc_tests.rs`:

```rust
//! #233: the ASIO resampler — 48 kHz → the card's rate with the servo's
//! correction (the frame counts prove the ratio), its bounds, its delay, the
//! tone kept; the splice — a pass-through delayed by its fade, an insert and a
//! skip with no click.

use super::*;

fn tone(blocks: usize, hz: f32) -> Vec<Vec<f32>> {
    (0..blocks)
        .map(|b| {
            (0..1600)
                .flat_map(|i| {
                    let x = (((b * 1600 + i) as f32) * 2.0 * std::f32::consts::PI * hz / 48_000.0).sin() * 0.5;
                    [x, x]
                })
                .collect()
        })
        .collect()
}

fn total_frames(rate: f64, ppm: f64, blocks: usize) -> usize {
    let mut a = Asrc::new(rate).unwrap();
    a.set_correction_ppm(ppm).unwrap();
    let block = vec![0.0f32; 3200];
    (0..blocks).map(|_| a.process(&block).unwrap().len() / 2).sum()
}

#[test]
fn the_ratio_is_the_rate_times_the_correction() {
    // rubato's Async counts its output exactly over time; ±4 frames over 300
    // blocks covers the first block's ratio ramp.
    assert!(total_frames(96_000.0, 0.0, 300).abs_diff(960_000) <= 4);
    assert!(total_frames(96_000.0, 100.0, 300).abs_diff(960_096) <= 4);
    assert!(total_frames(96_000.0, -250.0, 300).abs_diff(959_760) <= 4);
    assert!(total_frames(44_100.0, 0.0, 300).abs_diff(441_000) <= 4);
    assert!(total_frames(48_000.0, 50.0, 300).abs_diff(480_024) <= 4);
}

#[test]
fn a_correction_past_1000_ppm_is_refused() {
    let mut a = Asrc::new(96_000.0).unwrap();
    assert!(a.set_correction_ppm(999.0).is_ok());
    assert!(a.set_correction_ppm(1_001.0).is_err());
    assert!(a.set_correction_ppm(-1_001.0).is_err());
}

#[test]
fn the_delay_is_half_the_sinc_at_the_card_rate() {
    assert_eq!(Asrc::new(96_000.0).unwrap().delay_frames(), 256);
    assert_eq!(Asrc::new(48_000.0).unwrap().delay_frames(), 128);
}

#[test]
fn a_block_that_is_not_one_program_block_is_refused() {
    let mut a = Asrc::new(96_000.0).unwrap();
    assert!(a.process(&[0.0; 3198]).is_err());
}

#[test]
fn a_1khz_tone_stays_1khz_at_96k_with_a_correction() {
    let mut a = Asrc::new(96_000.0).unwrap();
    a.set_correction_ppm(300.0).unwrap();
    let mut left = Vec::new();
    for (b, block) in tone(30, 1000.0).iter().enumerate() {
        let out = a.process(block).unwrap();
        if b >= 3 {
            left.extend(out.iter().step_by(2).copied());
        }
    }
    let rising = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    assert!((898..=902).contains(&rising), "{rising}");
}

fn max_step(samples: &[f32]) -> f32 {
    samples.chunks_exact(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0.0, f32::max)
}

#[test]
fn the_splice_passes_audio_delayed_by_its_fade() {
    let mut s = Splice::new(96_000.0, 0, 3_200);
    assert_eq!(s.held_frames(), 480, "5 ms at 96 kHz");
    let input: Vec<f32> = (0..9_600).map(|i| i as f32).collect();
    let mut out = Vec::new();
    for chunk in input.chunks(3_200) {
        out.extend_from_slice(s.process(chunk));
    }
    let mut want = vec![0.0f32; 960];
    want.extend_from_slice(&input[..input.len() - 960]);
    assert_eq!(out, want, "bit for bit, 480 frames late");
}

#[test]
fn an_insert_is_a_fade_a_gap_and_a_fade_never_a_click() {
    let mut s = Splice::new(96_000.0, 4_800, 3_200);
    let dc = vec![0.5f32; 6_400];
    let mut out = Vec::new();
    out.extend_from_slice(s.process(&dc));
    s.insert(4_224); // 44 ms
    out.extend_from_slice(s.process(&dc));
    out.extend_from_slice(s.process(&dc));
    assert_eq!(out.len(), 3 * 6_400 + 2 * 4_224);
    // From frame 481 on: the first 480 frames are the hold's start-up silence.
    assert!(max_step(&out[962..]) <= 0.5 / 480.0 + 1e-6, "{}", max_step(&out[962..]));
    let zeros = out.chunks_exact(2).filter(|f| f[0] == 0.0).count();
    assert!(zeros >= 4_224);
}

#[test]
fn a_skip_is_a_fade_and_a_fade_and_spans_blocks() {
    let mut s = Splice::new(96_000.0, 0, 3_200);
    let dc = vec![0.5f32; 6_400];
    let mut out = Vec::new();
    out.extend_from_slice(s.process(&dc));
    s.skip(4_224);
    out.extend_from_slice(s.process(&dc));
    out.extend_from_slice(s.process(&dc));
    out.extend_from_slice(s.process(&dc));
    assert_eq!(out.len(), 4 * 6_400 - 2 * 4_224);
    assert!(max_step(&out[962..]) <= 0.5 / 480.0 + 1e-6);
    let mut big = Splice::new(96_000.0, 0, 3_200);
    big.process(&dc);
    big.skip(5_000); // more than one block (3_200 frames)
    assert_eq!(big.process(&dc).len(), 0, "the whole block is skipped (the hold stays held)");
    assert_eq!(big.process(&dc).len(), 6_400 - 2 * (5_000 - 3_200), "the rest of the skip");
}
```

(`Splice` sample counts are interleaved stereo: 6_400 samples = 3_200 frames. The pins follow `process` step by step: a skip of 5_000 frames eats the whole 3_200-frame block and emits nothing, then the remaining 1_800 frames of the next block.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server asrc::tests` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement** — `asrc.rs`:

```rust
//! #233: the ASIO output's resampler and its re-centre.
//!
//! `Asrc`: ONE rubato `Async` sinc stage (256 taps, BlackmanHarris², fixed
//! input of one 1600-frame program block) converts the 48 kHz program to the
//! card's rate; the servo (`asrc_servo.rs`) sets its relative ratio
//! `1 + ppm·1e-6` once per block, ramped across the block. Allocation-free
//! after `new` (`process_into_buffer`, rubato's `log` feature off). Its delay
//! is `sinc_len · ratio / 2` frames (256 at 96 kHz, 2.7 ms).
//!
//! `Splice`: the servo's re-centre on the resampler's output (the card's
//! rate): a 5 ms fade out, the inserted silence or the skipped frames, a 5 ms
//! fade in — never a click. It holds back its last 5 ms so a fade-out can
//! still reach audio not yet in the ring (a constant 5 ms of latency, counted
//! in the servo's `buffered_frames`).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction,
};
use sp_core::audio_outputs::PROGRAM_RATE;

use crate::playback::vban_packet::{VBAN_BLOCK_FRAMES, VBAN_BLOCK_SAMPLES, VBAN_CHANNELS};

/// The sinc length (rubato's default; ~2.7 ms at 96 kHz).
pub const ASRC_SINC_LEN: usize = 256;
/// The ratio's room around nominal: ±1000 ppm, well past the servo's ±300.
pub const ASRC_MAX_RELATIVE: f64 = 1.001;
/// A re-centre's fade, s.
pub const SPLICE_FADE_S: f64 = 0.005;

pub struct Asrc {
    inner: Async<f32>,
    out: Vec<f32>,
}

impl Asrc {
    pub fn new(device_rate_hz: f64) -> Result<Self, String> {
        let ratio = device_rate_hz / f64::from(PROGRAM_RATE);
        let params = SincInterpolationParameters::new(ASRC_SINC_LEN, WindowFunction::BlackmanHarris2);
        let inner = Async::<f32>::new_sinc(
            ratio,
            ASRC_MAX_RELATIVE,
            &params,
            VBAN_BLOCK_FRAMES,
            VBAN_CHANNELS,
            FixedAsync::Input,
        )
        .map_err(|e| e.to_string())?;
        let out = vec![0.0; inner.output_frames_max() * VBAN_CHANNELS];
        Ok(Self { inner, out })
    }

    /// The servo's correction (ramped across the next block).
    pub fn set_correction_ppm(&mut self, ppm: f64) -> Result<(), String> {
        self.inner
            .set_resample_ratio_relative(1.0 + ppm * 1e-6, true)
            .map_err(|e| e.to_string())
    }

    /// One program block (3200 interleaved samples) at the card's rate.
    pub fn process(&mut self, block: &[f32]) -> Result<&[f32], String> {
        if block.len() != VBAN_BLOCK_SAMPLES {
            return Err(format!("a block of {} samples, not {VBAN_BLOCK_SAMPLES}", block.len()));
        }
        let frames_out = self.inner.output_frames_max();
        let produced = {
            let input = InterleavedSlice::new(block, VBAN_CHANNELS, VBAN_BLOCK_FRAMES)
                .map_err(|e| e.to_string())?;
            let mut output = InterleavedSlice::new_mut(&mut self.out[..], VBAN_CHANNELS, frames_out)
                .map_err(|e| e.to_string())?;
            self.inner
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| e.to_string())?
                .1
        };
        Ok(&self.out[..produced * VBAN_CHANNELS])
    }

    pub fn delay_frames(&self) -> usize {
        self.inner.output_delay()
    }

    pub fn max_out_frames(&self) -> usize {
        self.inner.output_frames_max()
    }
}

/// The re-centre (module doc). Samples are interleaved stereo.
pub struct Splice {
    fade: usize,
    hold: Vec<f32>,
    out: Vec<f32>,
    insert: usize,
    skip: usize,
    fade_in_left: usize,
    muted: bool,
}

impl Splice {
    /// For a card at `device_rate_hz`; `max_insert_frames` and
    /// `max_block_frames` size the buffer once (no allocation per block).
    pub fn new(device_rate_hz: f64, max_insert_frames: usize, max_block_frames: usize) -> Self {
        let fade = ((device_rate_hz * SPLICE_FADE_S).round() as usize).max(1);
        Self {
            fade,
            hold: vec![0.0; fade * VBAN_CHANNELS],
            out: Vec::with_capacity((fade + max_insert_frames + max_block_frames) * VBAN_CHANNELS),
            insert: 0,
            skip: 0,
            fade_in_left: 0,
            muted: false,
        }
    }

    /// The frames held back (counted as buffered).
    pub fn held_frames(&self) -> usize {
        self.fade
    }

    pub fn insert(&mut self, frames: usize) {
        self.insert += frames;
    }

    pub fn skip(&mut self, frames: usize) {
        self.skip += frames;
    }

    pub fn process(&mut self, input: &[f32]) -> &[f32] {
        let fade = self.fade;
        self.out.clear();
        if (self.insert > 0 || self.skip > 0) && !self.muted {
            for (i, frame) in self.hold.chunks_exact_mut(VBAN_CHANNELS).enumerate() {
                let g = (fade - i - 1) as f32 / fade as f32;
                frame.iter_mut().for_each(|x| *x *= g);
            }
            self.muted = true;
        }
        self.out.extend_from_slice(&self.hold);
        if self.insert > 0 {
            self.out.resize(self.out.len() + self.insert * VBAN_CHANNELS, 0.0);
            self.insert = 0;
        }
        let skipped = (self.skip * VBAN_CHANNELS).min(input.len());
        self.skip -= skipped / VBAN_CHANNELS;
        let rest = &input[skipped..];
        let start = self.out.len();
        self.out.extend_from_slice(rest);
        if self.muted && self.skip == 0 && !rest.is_empty() {
            self.muted = false;
            self.fade_in_left = fade;
        }
        for frame in self.out[start..].chunks_exact_mut(VBAN_CHANNELS) {
            if self.fade_in_left == 0 {
                break;
            }
            let g = (fade - self.fade_in_left + 1) as f32 / fade as f32;
            frame.iter_mut().for_each(|x| *x *= g);
            self.fade_in_left -= 1;
        }
        let keep = fade * VBAN_CHANNELS;
        let n = self.out.len();
        self.hold.copy_from_slice(&self.out[n - keep..]);
        self.out.truncate(n - keep);
        &self.out
    }
}

#[cfg(test)]
#[path = "asrc_tests.rs"]
mod tests;
```

(`Adjustable` must be in scope for `set_resample_ratio_relative`; `Resampler` for `process_into_buffer` / `output_frames_max` / `output_delay`.)

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server asrc::tests` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the ASIO resampler's ratio, bounds, delay, pitch; the splice's pass-through, insert, skip` then `feat(#233): asrc — rubato Async sinc with the servo's ratio, the fade/gap/skip splice`.

### Task 2.4: the closed-loop simulation

**Files:**
- Create: `crates/sp-server/src/playback/asrc_servo_sim_tests.rs` (hook in `asrc_servo.rs`: `#[cfg(test)] #[path = "asrc_servo_sim_tests.rs"] mod sim_tests;`)

**Interfaces:**
- Consumes: `Servo`, `Observation`, `ServoAction`, `GROSS_STEP_100NS`, `BASE_LATENCY_100NS`, `MAX_PPM`, `MAX_SLEW_PPM_PER_S`.
- Produces: tests only.

- [ ] **Step 1: Write the simulation** — the Rust twin of the scratch model (an event loop over card callbacks and block hand-offs; the resampler as an exact frame count with a fractional carry):

```rust
//! #233: the servo in a closed loop. A card consumes `buffer` frames per
//! callback at `rate·(1 + card_ppm)`; the program hands one block per grid
//! slot, `h ∈ [0, jitter]` late (a seeded LCG); the servo's correction makes
//! `1600·rate/48000·(1 + ppm)` frames per block (fractional carry); a
//! re-centre inserts or drops at once. A clock step shifts the program
//! timeline against the card at 400 s (forward = the program catches up,
//! backward = it pauses); a dropped buffer is one callback the host missed.
//! 900 s per case.

use super::*;

struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Copy)]
struct Case {
    rate: f64,
    buffer: u64,
    card_ppm: f64,
    jitter_s: f64,
    step_s: f64,
    drop_at_s: Option<f64>,
}

const SNV: Case = Case { rate: 96_000.0, buffer: 128, card_ppm: 0.0, jitter_s: 0.015, step_s: 0.0, drop_at_s: None };
const STEP_AT_S: f64 = 400.0;
const RUN_S: f64 = 900.0;
const SLOT_S: f64 = GROSS_STEP_100NS as f64 / 1e7;

#[derive(Debug)]
struct Outcome {
    underruns: u64,
    max_abs_ppm: f64,
    final_ppm: f64,
    worst_slew_excess: f64,
    recentres: u64,
    worst_latency_err_ms: f64,
}

fn run(c: Case) -> Outcome {
    let mut servo = Servo::new(c.rate, BASE_LATENCY_100NS);
    let mut rng = Lcg(7);
    let (mut fill, mut consumed, mut carry) = (0.0f64, 0u64, 0.0f64);
    let (mut next_cb, mut last_handled, mut shift) = (0.0f64, 0.0f64, 0.0f64);
    let (mut stepped, mut dropped) = (false, false);
    let mut out = Outcome { underruns: 0, max_abs_ppm: 0.0, final_ppm: 0.0, worst_slew_excess: 0.0, recentres: 0, worst_latency_err_ms: 0.0 };
    let mut last_change: Option<(f64, f64)> = None;
    let per_block = 1600.0 * c.rate / 48_000.0;
    let mut k: i64 = 0;
    loop {
        let stamp_s = k as f64 * SLOT_S;
        if !stepped && stamp_s >= STEP_AT_S {
            shift -= c.step_s;
            stepped = true;
        }
        let handled = (stamp_s + 0.5 + shift + rng.unit() * c.jitter_s).max(last_handled);
        if handled > RUN_S {
            break;
        }
        if next_cb <= handled {
            if c.drop_at_s.is_some_and(|t| !dropped && next_cb >= t) {
                dropped = true; // the host missed this callback: nothing taken, nothing counted
            } else if fill >= c.buffer as f64 {
                fill -= c.buffer as f64;
                consumed += c.buffer;
            } else {
                if next_cb > 3.0 {
                    out.underruns += 1;
                }
                fill = 0.0;
                consumed += c.buffer;
            }
            next_cb += c.buffer as f64 / (c.rate * (1.0 + c.card_ppm * 1e-6));
            continue;
        }
        last_handled = handled;
        let wall_s = handled - 0.5 - shift;
        let a = servo.observe(Observation {
            handled_100ns: (wall_s * 1e7).round() as i64,
            stamp_100ns: (stamp_s * 1e7).round() as i64,
            buffered_frames: fill.floor() as u64,
            consumed_frames: consumed,
        });
        let latency_s = fill / c.rate + (wall_s - stamp_s);
        let quiet = (handled - (STEP_AT_S + 0.5)).abs() > 3.0 && c.drop_at_s.is_none_or(|t| (handled - t).abs() > 3.0);
        if handled > 70.0 && quiet {
            let err_ms = (latency_s + a.recentre_100ns as f64 / 1e7 - BASE_LATENCY_100NS as f64 / 1e7).abs() * 1e3;
            out.worst_latency_err_ms = out.worst_latency_err_ms.max(err_ms);
        }
        out.max_abs_ppm = out.max_abs_ppm.max(a.correction_ppm.abs());
        if last_change.is_none_or(|(_, p)| p != a.correction_ppm) {
            if let Some((t, p)) = last_change {
                let allowed = MAX_SLEW_PPM_PER_S * (wall_s - t);
                out.worst_slew_excess = out.worst_slew_excess.max((a.correction_ppm - p).abs() - allowed);
            }
            last_change = Some((wall_s, a.correction_ppm));
        }
        out.final_ppm = a.correction_ppm;
        fill = (fill + a.recentre_100ns as f64 / 1e7 * c.rate).max(0.0);
        let produced = per_block * (1.0 + a.correction_ppm * 1e-6) + carry;
        carry = produced - produced.floor();
        fill += produced.floor();
        k += 1;
    }
    out.recentres = servo.status().recentres;
    out
}

fn assert_held(o: &Outcome, card_ppm: f64, recentres: u64) {
    assert_eq!(o.underruns, 0, "{o:?}");
    assert!(o.max_abs_ppm <= MAX_PPM, "{o:?}");
    assert!(o.worst_slew_excess <= 1e-9, "{o:?}");
    assert!(o.worst_latency_err_ms <= 20.0, "{o:?}");
    assert!((o.final_ppm - card_ppm).abs() <= 5.0, "{o:?}");
    assert_eq!(o.recentres, recentres, "{o:?}");
}

#[test]
fn a_card_at_minus_50_0_and_plus_50_ppm_is_followed() {
    for ppm in [-50.0, 0.0, 50.0] {
        assert_held(&run(Case { card_ppm: ppm, ..SNV }), ppm, 1);
    }
}

#[test]
fn a_1_ms_clock_step_either_way_is_absorbed_without_a_re_centre() {
    for step in [0.001, -0.001] {
        assert_held(&run(Case { card_ppm: 20.0, step_s: step, ..SNV }), 20.0, 1);
    }
}

#[test]
fn a_44_ms_clock_step_either_way_re_centres_once() {
    for step in [0.044, -0.044] {
        assert_held(&run(Case { card_ppm: 20.0, step_s: step, ..SNV }), 20.0, 2);
    }
}

#[test]
fn a_dropped_buffer_is_absorbed() {
    assert_held(&run(Case { card_ppm: -20.0, drop_at_s: Some(500.0), ..SNV }), -20.0, 1);
}

#[test]
fn a_48k_card_with_256_frame_buffers_is_followed() {
    assert_held(&run(Case { rate: 48_000.0, buffer: 256, card_ppm: 50.0, ..SNV }), 50.0, 1);
}
```

- [ ] **Step 2: Run the scratch model's twin first.** Port this file 1:1 into your scratch Python model, run every case, and confirm every assertion holds with margin there (the planning session's model gave: underruns 0, |final − card| ≤ 2.8 ppm, latency error ≤ 14.4 ms, slew within bound, re-centres 1 / 1 / 2 / 1 / 1). If a pin fails in the model, fix the MODEL's port first, never loosen an assertion.

- [ ] **Step 3: Run (CI only):** `cargo test -p sp-server asrc_servo::sim_tests` — Expected: PASS (each case ≈ 0.7 M events; well under a second in the test profile).

- [ ] **Step 4: Commit** — `test(#233): the servo closed-loop — ±50 ppm cards, ±1 / ±44 ms steps, a dropped buffer, 48 kHz cards`.

### Task 2.5: docs, push

- [ ] **Step 1:** Append to `.claude/rules/audio-outputs.md` a section "## The drift servo and the ASRC (`asrc_servo.rs`, `asrc.rs`)": the observation, the latency target (two slots + delay — and WHY not driver buffers), the camera-box constants table with their `asrc_bench.rs` / `asrc-compensator.h` lines, the sign rule, re-centre thresholds (one slot per block, 10 ms per window), the splice (5 ms fades, 5 ms hold counted as buffered), the simulation's cases and what it asserts, and "integers where a boundary is pinned (latency, window span), f64 for the regression".
- [ ] **Step 2:** Lane checks (`cargo fmt --all --check`, `wc -l`, `cargo mutants --in-diff … --list` mapped to killers), `git push origin dev`, CI to terminal state. Nothing changes on the box (no runtime wiring).

---

## Lane 3 — The ASIO output (`azo`), the driver list, the ASIO UI, telemetry, the live gates at SNV and PP

Design comment for #233 (lane start): *Approach:* an ASIO entry (`type: "asio"`, `asio: {driver, channels: [left, right]}`, at most 4, one per driver) runs on its own worker thread that owns the driver: it loads it with `azo` 0.2.1 (pure-Rust COM host, MIT, no Steinberg SDK — iemmixer's choice), reads its CURRENT rate, preferred buffer, output channels and sample type and never sets any of them (Dante Controller / SoundGrid own them; a CI scan forbids the setters), creates buffers on every output channel, and starts it. The driver's buffer-switch callback (four static callback slots, as ASIO callbacks carry no user pointer) pops the frames it needs from an `rtrb` ring, writes L/R into the two configured channels in the driver's sample type (Int16/24/32LSB, Float32LSB, Int32LSB16–24), zeroes every other channel, and only counts — no allocation, lock or log. The worker takes each program block from its queue, asks the Lane 2 servo, resamples with `Asrc`, splices, and fills the ring. A reset request, a rate change or 2 s without a callback (a vanished driver) closes the output; it reopens after 2 / 10 / 30 / 60 s (a run of 60 s resets the backoff), showing the reason; DVS's single client shows as a busy driver. All of it is decided in Linux-tested modules (`asio_format`, `asio_state`, `asio_out` with a fake driver); only `asio_win.rs` calls COM. `GET /api/v1/audio/asio-drivers` lists the registered drivers (registry read only). *Rejected:* cpal / asio-sys (compiles the Steinberg SDK into the MIT exe — GPLv3 or a signed agreement; takes locks in the callback; can set the rate; research §A, §B); one driver thread per process (a slot per output keeps several ASIO devices independent); MMCSS for the worker (iemmixer: never pre-empt the driver's own callback thread; the worker has a 66.7 ms cushion). *Architektúra:* `playback::{asio_format, asio_state, audio_out_queue, asio_out}` (pure / fake-tested) + `playback::asio_win` (`#[cfg(windows)]`, azo `=0.2.1`, windows-sys `Win32_UI_WindowsAndMessaging` for the message pump) + `rtrb` `=0.4.0` + `api::audio`; framework reference read: `iemmixer/crates/iem-audio-io/src/{asio.rs,format.rs,telemetry.rs,reset.rs}` and `azo-0.2.1/src/lib.rs`.

**MAIN SESSION OPS — before this lane's push:**
1. On SNV (MCP `win-resolume`, read-only): `tasklist /m dvs_asio_x64.dll` — which process holds DVS's ASIO driver now (DVS takes ONE ASIO client). If another app holds it, ask the owner (❓, Slovak) before the SNV entry is added after the deploy; never kill it.
2. Tell the camera-box session: "SongPlayer will open Dante Virtual Soundcard's ASIO driver at SNV (the owner's go, #233 6034604457): its program goes out on DVS channels 1/2, which Ableton FOH has as cg."
3. If `.github/workflows/deploy-pp.yml` and `e2e/post-deploy-pp.config.ts` exist on dev at lane start (#229 lane 6), Task 3.8 adds the ASIO gate to PP's subset; otherwise the PP gate is the manual read in Step 8 of Task 3.8.

### Task 3.1: ASIO entries in the list (`sp_core` + the server parse)

**Files:**
- Modify: `crates/sp-core/src/audio_outputs.rs`, `crates/sp-core/src/audio_outputs_tests.rs`, `crates/sp-server/src/playback/audio_out_config.rs`, `crates/sp-server/src/playback/audio_out_config_tests.rs`, `crates/sp-server/src/api/program_tests_outputs.rs`

**Interfaces:**
- Consumes: Lane 1's model.
- Produces: `sp_core::audio_outputs::{OutputType::Asio, OutputType::NAMES = "vban or asio", MAX_ASIO_OUTPUTS = 4, MAX_DRIVER_NAME_LEN = 128, MAX_ASIO_CHANNEL: u32 = 511, struct AsioDest { driver: String, channels: [u32; 2] }, OutputEntry.asio: Option<AsioDest>, OutputEntry::asio(id, name, dest), new_asio(entries, driver: &str) -> OutputEntry, Problem::{SameChannel, DriverTaken, OutOfRange}}`; the server's strict and lenient parse read `asio`.

- [ ] **Step 1: Write the failing tests.** Append to `audio_outputs_tests.rs`:

```rust
fn dvs(id: &str) -> OutputEntry {
    OutputEntry::asio(id, "DVS", AsioDest { driver: "Dante Virtual Soundcard (x64)".into(), channels: [0, 1] })
}

#[test]
fn an_asio_entry_serializes_in_the_spec_layout() {
    assert_eq!(
        serde_json::to_string(&dvs("out-3")).unwrap(),
        r#"{"id":"out-3","name":"DVS","type":"asio","enabled":true,"rate":"network","delay_ms":0,"asio":{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}"#
    );
    assert!(validate_entry(0, &dvs("out-3")).is_ok());
}

#[test]
fn asio_limits_at_their_edges() {
    let check = |f: &dyn Fn(&mut OutputEntry)| {
        let mut e = dvs("out-3");
        f(&mut e);
        validate_entry(0, &e)
    };
    assert!(check(&|e| e.asio.as_mut().unwrap().driver = "d".repeat(128)).is_ok());
    assert_eq!(check(&|e| e.asio.as_mut().unwrap().driver = "d".repeat(129)).unwrap_err().problem, Problem::TooLong);
    assert_eq!(check(&|e| e.asio.as_mut().unwrap().driver = String::new()).unwrap_err().problem, Problem::Empty);
    assert!(check(&|e| e.asio.as_mut().unwrap().channels = [511, 0]).is_ok());
    let far = check(&|e| e.asio.as_mut().unwrap().channels = [0, 512]).unwrap_err();
    assert_eq!((far.field, far.problem), ("asio.channels", Problem::OutOfRange));
    assert_eq!(check(&|e| e.asio.as_mut().unwrap().channels = [3, 3]).unwrap_err().problem, Problem::SameChannel);
    assert_eq!(check(&|e| e.asio = None).unwrap_err().field, "asio");
}

#[test]
fn two_asio_entries_on_one_driver_are_refused() {
    let err = validate_list(&[dvs("out-1"), dvs("out-2")]).unwrap_err();
    assert_eq!(err.to_string(), "entry 2 (id out-2): asio.driver is already used by an earlier ASIO entry (a driver takes one client)");
    assert_eq!(err.sk(), "Výstup 2 (out-2): ovládač už má iný výstup ASIO (ovládač berie jedného klienta)");
    let five: Vec<OutputEntry> = (1..=5)
        .map(|n| OutputEntry::asio(&format!("out-{n}"), "a", AsioDest { driver: format!("d{n}"), channels: [0, 1] }))
        .collect();
    assert_eq!(
        validate_list(&five).unwrap_err(),
        ListError::TooManyOfType { kind: OutputType::Asio, count: 5, max: MAX_ASIO_OUTPUTS }
    );
}

#[test]
fn a_new_asio_entry_takes_the_first_free_id_and_channels_1_2() {
    let e = new_asio(&[dvs("out-4")], "Blackmagic ASIO");
    assert_eq!((e.id.as_str(), e.name.as_str(), e.kind), ("out-5", "ASIO 5", OutputType::Asio));
    assert_eq!(e.asio.unwrap(), AsioDest { driver: "Blackmagic ASIO".into(), channels: [0, 1] });
}
```

Append to `audio_out_config_tests.rs`:

```rust
#[test]
fn an_asio_entry_parses_and_a_bad_one_names_its_field() {
    let ok = r#"[{"id":"out-3","name":"DVS","type":"asio","asio":{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}]"#;
    assert_eq!(parse_list(ok).unwrap()[0].asio.as_ref().unwrap().channels, [0, 1]);
    let three = r#"[{"id":"out-3","name":"DVS","type":"asio","asio":{"driver":"d","channels":[0,1,2]}}]"#;
    assert_eq!(parse_list(three).unwrap_err(), "entry 1 (id out-3): asio.channels has the wrong type");
    let none = r#"[{"id":"out-3","name":"DVS","type":"asio"}]"#;
    assert_eq!(parse_list(none).unwrap_err(), "entry 1 (id out-3): asio is missing");
}
```

Lane 1 tests that change their expectation in this lane (an `asio` entry is readable now): in `audio_out_config_tests.rs`, `each_type_error_names_the_entry_and_the_field_never_the_value` expects `"entry 1 (id out-1): type must be vban or asio"` for `"midi"`; in `a_stored_entry_this_version_cannot_read_is_skipped_and_the_rest_run` and in `program_tests_outputs.rs::get_program_names_a_stored_entry_it_could_not_read`, the second entry becomes `{"id":"out-2","name":"AES67","type":"aes67"}` (a later transport this version cannot read) and the expected problem `"entry 2 (id out-2): type must be vban or asio"`. Commit these edits in the `test(#233)` commit with the reason in its message.

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-core audio_outputs` and `cargo test -p sp-server audio_out_config` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement.** In `sp-core/src/audio_outputs.rs`:
  - `OutputType` gains `Asio` (`as_str` "asio", `parse("asio")`), `NAMES = "vban or asio"`.
  - consts `MAX_ASIO_OUTPUTS: usize = 4`, `MAX_DRIVER_NAME_LEN: usize = 128`, `MAX_ASIO_CHANNEL: u32 = 511`.
  - 

```rust
/// Where an ASIO output plays: a registered driver and its two output
/// channels (0-based; the dashboard shows them 1-based).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsioDest {
    pub driver: String,
    pub channels: [u32; 2],
}
```

  - `OutputEntry` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub asio: Option<AsioDest>,` (after `vban`); `OutputEntry::vban` sets `asio: None`; new

```rust
    /// An enabled ASIO entry (its rate is the driver's; `rate` is kept as stored).
    pub fn asio(id: &str, name: &str, dest: AsioDest) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            kind: OutputType::Asio,
            enabled: true,
            rate: RateChoice::Network,
            delay_ms: 0,
            vban: None,
            asio: Some(dest),
        }
    }
```

  - `Problem` gains `OutOfRange` ("must be 0-511" / "musí byť 1 až 512" — the dashboard shows channels 1-based), `SameChannel` ("must name two different channels" / "musia byť dva rôzne kanály"), `DriverTaken` ("is already used by an earlier ASIO entry (a driver takes one client)" / "už má iný výstup ASIO (ovládač berie jedného klienta)"); `field_sk`: `"asio"` → "nastavenie ASIO", `"asio.driver"` → "ovládač", `"asio.channels"` → "kanály".
  - `validate_entry`'s match gains

```rust
        OutputType::Asio => {
            let Some(a) = &e.asio else {
                return Err(err("asio", Problem::Missing));
            };
            if a.driver.is_empty() {
                return Err(err("asio.driver", Problem::Empty));
            }
            if a.driver.chars().count() > MAX_DRIVER_NAME_LEN {
                return Err(err("asio.driver", Problem::TooLong));
            }
            if a.driver.chars().any(char::is_control) {
                return Err(err("asio.driver", Problem::BadCharacters));
            }
            if a.channels.iter().any(|&c| c > MAX_ASIO_CHANNEL) {
                return Err(err("asio.channels", Problem::OutOfRange));
            }
            if a.channels[0] == a.channels[1] {
                return Err(err("asio.channels", Problem::SameChannel));
            }
        }
```

  - `validate_list`: after the VBAN count, the same for `OutputType::Asio` against `MAX_ASIO_OUTPUTS`; inside the loop, after the duplicate-id check:

```rust
        if let Some(a) = &e.asio
            && e.kind == OutputType::Asio
            && entries[..i]
                .iter()
                .any(|p| p.kind == OutputType::Asio && p.asio.as_ref().is_some_and(|q| q.driver == a.driver))
        {
            return Err(ListError::Entry(EntryError {
                index: i,
                id: e.id.clone(),
                field: "asio.driver",
                problem: Problem::DriverTaken,
            }));
        }
```

  - `new_asio(entries, driver)` like `new_vban` (name `ASIO N`, channels `[0, 1]`).

In `audio_out_config.rs`: `entry()` reads `asio` for `OutputType::Asio`:

```rust
fn asio_dest(raw: &RawValue, at: &str) -> Result<AsioDest, String> {
    let r = Reader::of(raw, at, "asio.", &format!("{at}: asio"))?;
    Ok(AsioDest { driver: r.req("driver")?, channels: r.req("channels")? })
}
```

(`[u32; 2]` refuses three numbers: "asio.channels has the wrong type".) The `vban` / `asio` pair in `entry()` becomes a `match kind { Vban => (Some(vban_dest(..)?), None), Asio => (None, Some(asio_dest(..)?)) }`. `Stored::keep` mirrors the two new list rules (ASIO count, driver taken) with the same texts (`EntryError` / `ListError` displays).

- [ ] **Step 4: Run (CI only):** the two filters above + `cargo test -p sp-server api::program` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): ASIO entries — layout, limits, one entry per driver; the later-transport fixtures` then `feat(#233): ASIO entries in the output list (one per driver, at most 4)`.

### Task 3.2: `asio_format` — the driver's sample types and the channel fill

**Files:**
- Create: `crates/sp-server/src/playback/asio_format.rs`, `crates/sp-server/src/playback/asio_format_tests.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (`pub mod asio_format; // #233: an ASIO driver's sample types + the L/R channel fill (pure)`)

**Interfaces:**
- Consumes: `vban_packet::clean_f32`.
- Produces: `asio_format::{enum AsioSample { Int16, Int24, Int32, Float32, Int32In16, Int32In18, Int32In20, Int32In24 }, AsioSample::{from_code(i32) -> Result<Self, i32>, name() -> &'static str, bytes() -> usize, encode(stereo: &[f32], ch: usize, dst: &mut [u8])}, unsupported_sample_text(code: i32) -> String, fill_channel(sample, stereo: &[f32], source: Option<usize>, dst: &mut [u8]), source_of(channel: usize, left: usize, right: usize) -> Option<usize>}`.

- [ ] **Step 1: Write the failing tests** — `asio_format_tests.rs`:

```rust
//! #233: the driver's sample types (ASIOSampleType codes, little-endian only)
//! and how the program's L/R are written into them: symmetric full scale,
//! clamped, NaN silent; every other channel zeroed; an underrun's missing
//! frames zeroed.

use super::*;

#[test]
fn the_supported_codes_and_the_refused_ones() {
    let table = [
        (16, AsioSample::Int16, "Int16LSB", 2),
        (17, AsioSample::Int24, "Int24LSB", 3),
        (18, AsioSample::Int32, "Int32LSB", 4),
        (19, AsioSample::Float32, "Float32LSB", 4),
        (24, AsioSample::Int32In16, "Int32LSB16", 4),
        (25, AsioSample::Int32In18, "Int32LSB18", 4),
        (26, AsioSample::Int32In20, "Int32LSB20", 4),
        (27, AsioSample::Int32In24, "Int32LSB24", 4),
    ];
    for (code, sample, name, bytes) in table {
        assert_eq!(AsioSample::from_code(code), Ok(sample));
        assert_eq!((sample.name(), sample.bytes()), (name, bytes));
    }
    for code in [0, 2, 15, 20, 21, 28, 32, 40] {
        assert_eq!(AsioSample::from_code(code), Err(code), "Float64, MSB, DSD and unknown types refuse");
    }
    assert_eq!(
        unsupported_sample_text(20),
        "the driver's sample type 20 is not supported (Int16/24/32LSB, Float32LSB, Int32LSB16-24)"
    );
}

fn one(sample: AsioSample, x: f32) -> Vec<u8> {
    let mut dst = vec![0xAA; sample.bytes()];
    sample.encode(&[x, -x], 0, &mut dst);
    dst
}

#[test]
fn each_type_encodes_exactly() {
    assert_eq!(one(AsioSample::Int16, 1.0), 32_767i16.to_le_bytes());
    assert_eq!(one(AsioSample::Int16, -1.0), (-32_767i16).to_le_bytes());
    assert_eq!(one(AsioSample::Int16, 0.5), 16_384i16.to_le_bytes());
    assert_eq!(one(AsioSample::Int24, 0.5), [0x00, 0x00, 0x40], "4_194_304");
    assert_eq!(one(AsioSample::Int24, -1.0), [0x01, 0x00, 0x80], "−8_388_607");
    assert_eq!(one(AsioSample::Int32, 1.0), 2_147_483_647i32.to_le_bytes());
    assert_eq!(one(AsioSample::Int32In24, 0.5), 4_194_304i32.to_le_bytes());
    assert_eq!(one(AsioSample::Int32In20, 1.0), 524_287i32.to_le_bytes());
    assert_eq!(one(AsioSample::Int32In18, 1.0), 131_071i32.to_le_bytes());
    assert_eq!(one(AsioSample::Int32In16, -1.0), (-32_767i32).to_le_bytes());
    assert_eq!(one(AsioSample::Float32, 0.25), 0.25f32.to_le_bytes());
    assert_eq!(one(AsioSample::Float32, 1.5), 1.0f32.to_le_bytes(), "clamped");
    assert_eq!(one(AsioSample::Int24, f32::NAN), [0, 0, 0], "NaN is silence");
}

#[test]
fn the_right_channel_is_read_from_the_second_sample() {
    let mut dst = vec![0u8; 4];
    AsioSample::Int16.encode(&[0.5, -0.5, 0.25, -0.25], 1, &mut dst);
    assert_eq!(&dst[..2], &(-16_384i16).to_le_bytes());
    assert_eq!(&dst[2..], &(-8_192i16).to_le_bytes());
}

#[test]
fn fill_channel_writes_the_source_zeroes_the_rest_and_the_missing_frames() {
    let stereo = [0.5f32, -0.5, 0.5, -0.5, 0.5, -0.5]; // 3 frames of 4
    let mut left = vec![0xAAu8; 4 * 3];
    fill_channel(AsioSample::Int24, &stereo, Some(0), &mut left);
    assert_eq!(&left[..9], &[0x00, 0x00, 0x40].repeat(3)[..]);
    assert_eq!(&left[9..], &[0, 0, 0], "the underrun's frame is silence");
    let mut other = vec![0xAAu8; 12];
    fill_channel(AsioSample::Int24, &stereo, None, &mut other);
    assert!(other.iter().all(|&b| b == 0));
    assert_eq!(source_of(0, 0, 1), Some(0));
    assert_eq!(source_of(1, 0, 1), Some(1));
    assert_eq!(source_of(5, 2, 5), Some(1));
    assert_eq!(source_of(3, 2, 5), None);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server asio_format` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement** — `asio_format.rs`:

```rust
//! #233: an ASIO driver's sample types (ASIOSampleType, little-endian only:
//! the big-endian ones exist on old Macs, Float64 and DSD on no driver we
//! use) and the program's L/R written into them. Integers use a SYMMETRIC
//! full scale (±(2^(bits−1) − 1)), like VBAN's INT24; a value is clamped to
//! ±1.0 and a non-finite one is silence. Every channel but the two
//! configured ones is zeroed; frames an underrun did not deliver are zeroed.
//! Pure: the driver callback (`asio_win.rs`) calls `fill_channel` per
//! channel; the encode is the iemmixer model (`iem-audio-io/src/format.rs`).

use crate::playback::vban_packet::clean_f32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsioSample {
    Int16,
    Int24,
    Int32,
    Float32,
    /// A 32-bit container holding a sign-extended 16/18/20/24-bit value.
    Int32In16,
    Int32In18,
    Int32In20,
    Int32In24,
}

impl AsioSample {
    pub fn from_code(code: i32) -> Result<Self, i32> {
        Ok(match code {
            16 => Self::Int16,
            17 => Self::Int24,
            18 => Self::Int32,
            19 => Self::Float32,
            24 => Self::Int32In16,
            25 => Self::Int32In18,
            26 => Self::Int32In20,
            27 => Self::Int32In24,
            other => return Err(other),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Int16 => "Int16LSB",
            Self::Int24 => "Int24LSB",
            Self::Int32 => "Int32LSB",
            Self::Float32 => "Float32LSB",
            Self::Int32In16 => "Int32LSB16",
            Self::Int32In18 => "Int32LSB18",
            Self::Int32In20 => "Int32LSB20",
            Self::Int32In24 => "Int32LSB24",
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Self::Int16 => 2,
            Self::Int24 => 3,
            _ => 4,
        }
    }

    /// The symmetric integer full scale; `None` for Float32.
    fn full_scale(self) -> Option<f64> {
        match self {
            Self::Int16 | Self::Int32In16 => Some(32_767.0),
            Self::Int32In18 => Some(131_071.0),
            Self::Int32In20 => Some(524_287.0),
            Self::Int24 | Self::Int32In24 => Some(8_388_607.0),
            Self::Int32 => Some(2_147_483_647.0),
            Self::Float32 => None,
        }
    }

    /// Channel `ch` (0 = L, 1 = R) of interleaved stereo `stereo` into `dst`,
    /// whole samples only.
    pub fn encode(self, stereo: &[f32], ch: usize, dst: &mut [u8]) {
        for (out, frame) in dst.chunks_exact_mut(self.bytes()).zip(stereo.chunks_exact(2)) {
            let x = clean_f32(frame[ch]);
            match (self, self.full_scale()) {
                (_, None) => out.copy_from_slice(&x.to_le_bytes()),
                (Self::Int16, Some(full)) => {
                    out.copy_from_slice(&((f64::from(x) * full).round() as i16).to_le_bytes())
                }
                (Self::Int24, Some(full)) => {
                    let b = ((f64::from(x) * full).round() as i32).to_le_bytes();
                    out.copy_from_slice(&b[..3]);
                }
                (_, Some(full)) => {
                    out.copy_from_slice(&((f64::from(x) * full).round() as i32).to_le_bytes())
                }
            }
        }
    }
}

pub fn unsupported_sample_text(code: i32) -> String {
    format!("the driver's sample type {code} is not supported (Int16/24/32LSB, Float32LSB, Int32LSB16-24)")
}

/// Which program channel output channel `channel` plays: L, R or none.
pub fn source_of(channel: usize, left: usize, right: usize) -> Option<usize> {
    if channel == left {
        Some(0)
    } else if channel == right {
        Some(1)
    } else {
        None
    }
}

/// One output channel's half-buffer: `source` of `stereo` (the frames the
/// ring delivered) encoded, everything after them — or all of it for an
/// unused channel — zeroed.
pub fn fill_channel(sample: AsioSample, stereo: &[f32], source: Option<usize>, dst: &mut [u8]) {
    let written = match source {
        Some(ch) => {
            sample.encode(stereo, ch, dst);
            (stereo.len() / 2 * sample.bytes()).min(dst.len())
        }
        None => 0,
    };
    dst[written..].fill(0);
}

#[cfg(test)]
#[path = "asio_format_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server asio_format` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): ASIO sample types — codes, exact encodes, the channel fill` then `feat(#233): asio_format — the driver's sample types and the L/R fill`.

### Task 3.3: `asio_state` — backoff, close reasons, message replies, stall, notes

**Files:**
- Create: `crates/sp-server/src/playback/asio_state.rs`, `crates/sp-server/src/playback/asio_state_tests.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (`pub mod asio_state; // #233: the ASIO output's decisions (backoff, reasons, replies, stall), pure`)

**Interfaces:**
- Consumes: `asrc_servo::{BASE_LATENCY_100NS, GROSS_STEP_100NS}`.
- Produces: `asio_state::{BACKOFF_S: [i64; 4] = [2, 10, 30, 60], STABLE_RUN_100NS = 600_000_000, STALL_100NS = 20_000_000, POLL_100NS = 100_000, MIN_RATE = 8_000.0, MAX_RATE = 384_000.0, backoff_100ns(failures: u32) -> i64, failures_after_close(failures: u32, ran_100ns: i64) -> u32, enum Reason { NotFound { present: Vec<String> }, Busy(String), Refused(String), Failed(String), Reset, RateChanged(u32), Stalled, WindowsOnly } (code(), text()), struct DeviceEvents { reset, resync, buffer_size_change, latencies_changed: bool, rate_changed: Option<f64>, overloads: u64, callbacks: u64 }, close_reason(&DeviceEvents, opened_rate: f64) -> Option<Reason>, struct StallWatch (stalled(callbacks, now_100ns) -> bool), mod selector, reply(sel: i32, value: i32) -> i32, admit_rate(f64) -> Result<u32, Reason>, rate_note(driver_rate: u32, network: u32) -> Option<String>, ring_capacity_frames(rate: f64, target_100ns: i64, max_block_frames: usize) -> usize, recentre_frames(recentre_100ns: i64, rate: f64) -> i64, asio_latency_ms(servo_latency_ms: f64, asrc_delay_frames: usize, driver_latency_frames: u32, rate: f64) -> f64}`.

- [ ] **Step 1: Write the failing tests** — `asio_state_tests.rs`:

```rust
//! #233: the ASIO output's decisions — the reopen backoff (2 / 10 / 30 / 60 s,
//! reset by a 60 s run), what closes an output, the asioMessage replies
//! (iemmixer telemetry.rs), the 2 s stall, the rate admission and note, the
//! ring size, the latency.

use super::*;

#[test]
fn the_backoff_is_2_10_30_then_every_60_s() {
    let s: Vec<i64> = (1..=6).map(|f| backoff_100ns(f) / 10_000_000).collect();
    assert_eq!(s, vec![2, 10, 30, 60, 60, 60]);
    assert_eq!(backoff_100ns(0), 20_000_000, "never less than the start backoff");
}

#[test]
fn a_60_s_run_resets_the_backoff() {
    assert_eq!(failures_after_close(3, STABLE_RUN_100NS), 1);
    assert_eq!(failures_after_close(3, STABLE_RUN_100NS - 1), 4);
    assert_eq!(failures_after_close(0, 0), 1);
}

#[test]
fn a_reset_a_size_change_or_a_new_rate_closes_the_output() {
    let base = DeviceEvents::default();
    assert_eq!(close_reason(&base, 96_000.0), None);
    assert_eq!(close_reason(&DeviceEvents { reset: true, ..base }, 96_000.0), Some(Reason::Reset));
    assert_eq!(close_reason(&DeviceEvents { buffer_size_change: true, ..base }, 96_000.0), Some(Reason::Reset));
    assert_eq!(
        close_reason(&DeviceEvents { rate_changed: Some(48_000.0), ..base }, 96_000.0),
        Some(Reason::RateChanged(48_000))
    );
    assert_eq!(close_reason(&DeviceEvents { rate_changed: Some(96_000.5), ..base }, 96_000.0), None, "under 1 Hz");
    assert_eq!(
        close_reason(&DeviceEvents { rate_changed: Some(96_001.0), ..base }, 96_000.0),
        Some(Reason::RateChanged(96_001))
    );
    assert_eq!(close_reason(&DeviceEvents { resync: true, latencies_changed: true, overloads: 3, ..base }, 96_000.0), None);
}

#[test]
fn no_callback_for_2_s_is_a_stall() {
    let mut w = StallWatch::default();
    assert!(!w.stalled(10, 0));
    assert!(!w.stalled(10, STALL_100NS - 1));
    assert!(w.stalled(10, STALL_100NS));
    assert!(!w.stalled(11, STALL_100NS + 5), "a callback resets it");
    assert!(!w.stalled(11, 2 * STALL_100NS + 4));
    assert!(w.stalled(11, 2 * STALL_100NS + 5));
}

#[test]
fn the_message_replies_follow_iemmixer() {
    use selector::*;
    assert_eq!(reply(SELECTOR_SUPPORTED, RESET_REQUEST), 1);
    assert_eq!(reply(SELECTOR_SUPPORTED, BUFFER_SIZE_CHANGE), 1);
    assert_eq!(reply(SELECTOR_SUPPORTED, SUPPORTS_TIME_CODE), 0);
    assert_eq!(reply(ENGINE_VERSION, 0), 2);
    assert_eq!(reply(RESET_REQUEST, 0), 1);
    assert_eq!(reply(RESYNC_REQUEST, 0), 1);
    assert_eq!(reply(LATENCIES_CHANGED, 0), 1);
    assert_eq!(reply(SUPPORTS_TIME_INFO, 0), 1);
    assert_eq!(reply(BUFFER_SIZE_CHANGE, 256), 0, "never resized live: the driver then asks a reset");
    assert_eq!(reply(OVERLOAD, 0), 0);
    assert_eq!(reply(99, 0), 0);
}

#[test]
fn the_rate_is_admitted_and_noted() {
    assert_eq!(admit_rate(96_000.0), Ok(96_000));
    assert_eq!(admit_rate(8_000.0), Ok(8_000));
    assert_eq!(admit_rate(384_000.0), Ok(384_000));
    assert!(admit_rate(7_999.0).is_err());
    assert!(admit_rate(384_001.0).is_err());
    assert!(admit_rate(f64::NAN).is_err());
    assert!(admit_rate(0.0).is_err());
    assert_eq!(rate_note(96_000, 96_000), None);
    assert_eq!(rate_note(48_000, 96_000).as_deref(), Some("the driver runs at 48000 Hz, the network at 96000 Hz"));
}

#[test]
fn the_ring_holds_the_target_four_slots_and_a_block() {
    // 66.67 ms + 4 × 33.33 ms = 200 ms at 96 kHz = 19_200 frames, + one block out (3_210)
    assert_eq!(ring_capacity_frames(96_000.0, 666_666, 3_210), 19_200 + 3_210);
    assert_eq!(recentre_frames(440_000, 96_000.0), 4_224);
    assert_eq!(recentre_frames(-440_000, 96_000.0), -4_224);
}

#[test]
fn the_latency_adds_the_resampler_and_the_driver() {
    let ms = asio_latency_ms(66.7, 256, 128, 96_000.0);
    assert!((ms - (66.7 + 2.6667 + 1.3333)).abs() < 1e-3, "{ms}");
}

#[test]
fn every_reason_has_a_code_and_a_text() {
    let reasons = [
        (Reason::NotFound { present: vec!["Blackmagic ASIO".into()] }, "not_found", "the driver is not registered (present: Blackmagic ASIO)"),
        (Reason::Busy("init failed".into()), "busy", "the driver refused to start (in use by another program?): init failed"),
        (Reason::Refused("x".into()), "refused", "x"),
        (Reason::Failed("y".into()), "failed", "y"),
        (Reason::Reset, "reset", "the driver asked for a reset"),
        (Reason::RateChanged(48_000), "rate_changed", "the driver's rate changed to 48000 Hz"),
        (Reason::Stalled, "stalled", "no callback from the driver for 2 s"),
        (Reason::WindowsOnly, "windows_only", "ASIO runs on Windows only"),
    ];
    for (r, code, text) in reasons {
        assert_eq!((r.code(), r.text().as_str()), (code, text));
    }
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server asio_state` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement** — `asio_state.rs`:

```rust
//! #233: the ASIO output's decisions, pure (the worker `asio_out.rs` and the
//! Windows glue `asio_win.rs` only act on them).
//! - Reopen backoff 2 / 10 / 30 / 60 / 60 … s after a failed open or a close;
//!   a run of 60 s resets it, so a reset after a long run waits 2 s (some
//!   virtual drivers stay busy for seconds after a release).
//! - A close: a reset request (or a buffer-size change, which is answered 0 so
//!   the driver then asks a reset — iemmixer never resizes live), a rate change
//!   ≥ 1 Hz (Dante Controller re-clocked the card), or no callback for 2 s (a
//!   vanished driver: a DVS crash or reinstall; iemmixer `reset.rs` STALL).
//! - asioMessage replies: iemmixer `telemetry.rs:79-99`.

use crate::playback::asrc_servo::GROSS_STEP_100NS;

pub const BACKOFF_S: [i64; 4] = [2, 10, 30, 60];
/// A run this long resets the backoff (60 s).
pub const STABLE_RUN_100NS: i64 = 600_000_000;
/// No callback for this long while running: the driver is gone (2 s).
pub const STALL_100NS: i64 = 20_000_000;
/// The worker steps at least this often (driver messages, 10 ms).
pub const POLL_100NS: i64 = 100_000;
pub const MIN_RATE: f64 = 8_000.0;
pub const MAX_RATE: f64 = 384_000.0;

pub fn backoff_100ns(failures: u32) -> i64 {
    let i = (failures.max(1) as usize - 1).min(BACKOFF_S.len() - 1);
    BACKOFF_S[i] * 10_000_000
}

pub fn failures_after_close(failures: u32, ran_100ns: i64) -> u32 {
    if ran_100ns >= STABLE_RUN_100NS { 1 } else { failures + 1 }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    NotFound { present: Vec<String> },
    Busy(String),
    Refused(String),
    Failed(String),
    Reset,
    RateChanged(u32),
    Stalled,
    WindowsOnly,
}

impl Reason {
    /// A stable code (the dashboard maps it to Slovak).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::Busy(_) => "busy",
            Self::Refused(_) => "refused",
            Self::Failed(_) => "failed",
            Self::Reset => "reset",
            Self::RateChanged(_) => "rate_changed",
            Self::Stalled => "stalled",
            Self::WindowsOnly => "windows_only",
        }
    }

    pub fn text(&self) -> String {
        match self {
            Self::NotFound { present } => {
                format!("the driver is not registered (present: {})", present.join(", "))
            }
            Self::Busy(e) => format!("the driver refused to start (in use by another program?): {e}"),
            Self::Refused(e) | Self::Failed(e) => e.clone(),
            Self::Reset => "the driver asked for a reset".into(),
            Self::RateChanged(r) => format!("the driver's rate changed to {r} Hz"),
            Self::Stalled => "no callback from the driver for 2 s".into(),
            Self::WindowsOnly => "ASIO runs on Windows only".into(),
        }
    }
}

/// What the driver said since the last poll (counted by its callbacks).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DeviceEvents {
    pub reset: bool,
    pub resync: bool,
    pub buffer_size_change: bool,
    pub latencies_changed: bool,
    pub rate_changed: Option<f64>,
    pub overloads: u64,
    /// Buffer switches since the output started (monotonic).
    pub callbacks: u64,
}

pub fn close_reason(ev: &DeviceEvents, opened_rate: f64) -> Option<Reason> {
    if ev.reset || ev.buffer_size_change {
        return Some(Reason::Reset);
    }
    match ev.rate_changed {
        Some(r) if (r - opened_rate).abs() >= 1.0 => Some(Reason::RateChanged(r.round() as u32)),
        _ => None,
    }
}

/// No callback for [`STALL_100NS`].
#[derive(Debug, Default)]
pub struct StallWatch {
    last: Option<(u64, i64)>,
}

impl StallWatch {
    pub fn stalled(&mut self, callbacks: u64, now_100ns: i64) -> bool {
        match self.last {
            Some((count, since)) if count == callbacks => now_100ns - since >= STALL_100NS,
            _ => {
                self.last = Some((callbacks, now_100ns));
                false
            }
        }
    }
}

/// The ASIO driver-to-host message selectors (asio.h `kAsio…`).
pub mod selector {
    pub const SELECTOR_SUPPORTED: i32 = 1;
    pub const ENGINE_VERSION: i32 = 2;
    pub const RESET_REQUEST: i32 = 3;
    pub const BUFFER_SIZE_CHANGE: i32 = 4;
    pub const RESYNC_REQUEST: i32 = 5;
    pub const LATENCIES_CHANGED: i32 = 6;
    pub const SUPPORTS_TIME_INFO: i32 = 7;
    pub const SUPPORTS_TIME_CODE: i32 = 8;
    pub const OVERLOAD: i32 = 15;
}

/// The host's answer to `asioMessage` (iemmixer `telemetry.rs:79-99`).
pub fn reply(sel: i32, value: i32) -> i32 {
    use selector::*;
    match sel {
        SELECTOR_SUPPORTED => i32::from(matches!(
            value,
            ENGINE_VERSION | RESET_REQUEST | BUFFER_SIZE_CHANGE | RESYNC_REQUEST
                | LATENCIES_CHANGED | SUPPORTS_TIME_INFO | OVERLOAD
        )),
        ENGINE_VERSION => 2,
        RESET_REQUEST | RESYNC_REQUEST | LATENCIES_CHANGED | SUPPORTS_TIME_INFO => 1,
        _ => 0,
    }
}

/// The driver's rate, as the output follows it (never set).
pub fn admit_rate(rate: f64) -> Result<u32, Reason> {
    if rate.is_finite() && (MIN_RATE..=MAX_RATE).contains(&rate) {
        Ok(rate.round() as u32)
    } else {
        Err(Reason::Refused(format!("the driver reports {rate} Hz")))
    }
}

/// The status note of a driver off the network's rate (a WARN once).
pub fn rate_note(driver_rate: u32, network: u32) -> Option<String> {
    (driver_rate != network)
        .then(|| format!("the driver runs at {driver_rate} Hz, the network at {network} Hz"))
}

/// The ring's capacity: the target latency + four slots, + one block's output.
pub fn ring_capacity_frames(rate: f64, target_100ns: i64, max_block_frames: usize) -> usize {
    let span_100ns = target_100ns + 4 * GROSS_STEP_100NS;
    (span_100ns as f64 * rate / 1e7).round() as usize + max_block_frames
}

/// A re-centre in frames of the card's rate.
pub fn recentre_frames(recentre_100ns: i64, rate: f64) -> i64 {
    (recentre_100ns as f64 * rate / 1e7).round() as i64
}

/// An ASIO output's latency, ms: the servo's (ring + splice + hand-off), the
/// resampler's delay, the driver's output latency.
pub fn asio_latency_ms(servo_latency_ms: f64, asrc_delay_frames: usize, driver_latency_frames: u32, rate: f64) -> f64 {
    servo_latency_ms + (asrc_delay_frames as f64 + f64::from(driver_latency_frames)) * 1_000.0 / rate
}

#[cfg(test)]
#[path = "asio_state_tests.rs"]
mod tests;
```

(The ring pin: `(666_666 + 1_333_332) × 96_000 / 1e7 = 19_199.99…` → 19_200.)

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server asio_state` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the ASIO output's decisions — backoff, close reasons, replies, stall, rate, ring, latency` then `feat(#233): asio_state — the ASIO output's pure decisions`.

### Task 3.4: the ASIO worker over a driver trait (`asio_out.rs`) + its queue + the fake driver

**Files:**
- Create: `crates/sp-server/src/playback/audio_out_queue.rs`, `crates/sp-server/src/playback/asio_out.rs`, `crates/sp-server/src/playback/asio_out_tests.rs`, `crates/sp-server/src/playback/asio_out_fake.rs`
- Modify: `crates/sp-server/Cargo.toml` (`rtrb = "=0.4.0"` in `[dependencies]`, + `Cargo.lock` by `cargo metadata`, as in Task 1.5), `crates/sp-server/src/playback/mod.rs` (`pub mod audio_out_queue; // #233: a bounded drop-oldest block queue (the ASIO output's)` and `pub mod asio_out; // #233: the ASIO output — its worker over a driver trait (open / run / close / backoff)`)

**Interfaces:**
- Consumes: `ProgramBlock`, `Servo`, `Observation`, `Asrc`, `Splice`, `asio_state::*`, `AsioSample`, `VbanClock` (the worker's program-wall clock trait), `OutputEntry`, `BASE_LATENCY_100NS`, `VBAN_QUEUE_BOUND`, `queue_bound`.
- Produces: `audio_out_queue::{BlockQueue (new(bound), push(ProgramBlock) -> bool, take_timeout(Duration) -> Take, queued, stop, dropped), enum Take { Block(ProgramBlock), Idle, Stopped }}`; `asio_out::{trait AsioDevice { open(&mut self, driver: &str, channels: [u32; 2]) -> Result<Opened, Reason>; start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason>; poll(&mut self) -> DeviceEvents; consumed_frames(&self) -> u64; underruns(&self) -> u64; mark_primed(&mut self); close(&mut self) }, struct Opened { rate: f64, buffer_frames: u32, out_channels: u32, sample: AsioSample }, struct Started { output_latency_frames: u32 }, struct AsioStatus { driver, channels, driver_rate, buffer_frames, out_channels, sample_type: &'static str, ppm, rate_ppm, locked, latency_ms, underruns, resets, recentres, overflows, retry_in_s: Option<f64> } (Serialize), struct AsioOut (for_entry(&OutputEntry) -> Result<Self, String>, push, stop, snapshot() -> AsioSnapshot, is_running), struct AsioSnapshot { state: &'static str, reason: Option<Reason>, status: AsioStatus, blocks_sent: u64, blocks_dropped: u64 }, struct AsioWorker (new(now_100ns), step(&mut self, out: &AsioOut, device: &mut dyn AsioDevice, now_100ns: i64, block: Option<ProgramBlock>) -> i64, shutdown(&mut self, device: &mut dyn AsioDevice)), run_asio_worker(out: &AsioOut, device: &mut dyn AsioDevice, clock: &mut dyn VbanClock)}`; `asio_out::fake::FakeDevice` (`pub(crate)`, test-only).

- [ ] **Step 1: Write the fake driver and the failing tests.** `asio_out_fake.rs` (hook `#[cfg(test)] #[path = "asio_out_fake.rs"] pub(crate) mod fake;` in `asio_out.rs`):

```rust
//! #233: a scripted ASIO driver for the worker's tests (CI runners have no
//! ASIO driver): scripted open answers and messages; `drain` plays the card,
//! taking `buffer` frames per callback from the ring like the real callback.

use std::collections::VecDeque;

use super::*;

pub(crate) struct FakeDevice {
    pub opens: VecDeque<Result<Opened, Reason>>,
    pub events: VecDeque<DeviceEvents>,
    pub ring: Option<rtrb::Consumer<f32>>,
    pub buffer: u32,
    pub consumed: u64,
    pub underruns: u64,
    pub callbacks: u64,
    pub primed: bool,
    pub opened: Vec<(String, [u32; 2])>,
    pub closes: u32,
    pub played: Vec<f32>,
}

pub(crate) fn dvs(rate: f64) -> Opened {
    Opened { rate, buffer_frames: 128, out_channels: 2, sample: AsioSample::Int32 }
}

impl FakeDevice {
    pub fn answering(opens: Vec<Result<Opened, Reason>>) -> Self {
        Self {
            opens: opens.into(),
            events: VecDeque::new(),
            ring: None,
            buffer: 128,
            consumed: 0,
            underruns: 0,
            callbacks: 0,
            primed: false,
            opened: Vec::new(),
            closes: 0,
            played: Vec::new(),
        }
    }

    /// The card takes `n` callbacks.
    pub fn drain(&mut self, n: u32) {
        for _ in 0..n {
            let want = self.buffer as usize * 2;
            if let Some(ring) = self.ring.as_mut() {
                let mut got = vec![0.0f32; want];
                let (filled, _) = ring.pop_partial_slice(&mut got);
                let n_got = filled.len();
                self.played.extend_from_slice(&got[..n_got]);
                if n_got < want && self.primed {
                    self.underruns += 1;
                }
            }
            self.consumed += u64::from(self.buffer);
            self.callbacks += 1;
        }
    }
}

impl AsioDevice for FakeDevice {
    fn open(&mut self, driver: &str, channels: [u32; 2]) -> Result<Opened, Reason> {
        self.opened.push((driver.to_string(), channels));
        let answer = self.opens.pop_front().unwrap_or_else(|| Ok(dvs(96_000.0)));
        if let Ok(o) = &answer {
            self.buffer = o.buffer_frames;
        }
        answer
    }

    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        self.ring = Some(ring);
        self.consumed = 0;
        self.callbacks = 0;
        self.primed = false;
        Ok(Started { output_latency_frames: 128 })
    }

    fn poll(&mut self) -> DeviceEvents {
        let mut ev = self.events.pop_front().unwrap_or_default();
        ev.callbacks = self.callbacks;
        ev
    }

    fn consumed_frames(&self) -> u64 {
        self.consumed
    }

    fn underruns(&self) -> u64 {
        self.underruns
    }

    fn mark_primed(&mut self) {
        self.primed = true;
    }

    fn close(&mut self) {
        self.ring = None;
        self.closes += 1;
    }
}
```

`asio_out_tests.rs`:

```rust
//! #233: the ASIO worker over a scripted driver — it opens at the driver's
//! rate and sets nothing; blocks reach the card through the servo, the
//! resampler and the ring with no underrun; a reset, a stall and a rate change
//! close it and it reopens after the backoff; a busy driver is retried after
//! 2 / 10 / 30 / 60 s with its reason shown.

use super::fake::{FakeDevice, dvs};
use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use sp_core::audio_outputs::{AsioDest, OutputEntry};

const T0: i64 = 17_900_000_000_000_000;
const SLOT: i64 = 333_333;
const S: i64 = 10_000_000;
const DVS: &str = "Dante Virtual Soundcard (x64)";

fn out() -> AsioOut {
    AsioOut::for_entry(&OutputEntry::asio("out-3", "DVS", AsioDest { driver: DVS.into(), channels: [0, 1] }))
        .unwrap()
}

fn block(k: i64) -> ProgramBlock {
    ProgramBlock { due_100ns: T0 + k * SLOT, samples: Some(vec![0.25; 3200].into()), substituted: false }
}

/// Run `n` boundaries from `from` (each handled 5 ms late), the card draining
/// in between at 96 kHz / 128 frames (25 callbacks a boundary).
fn run(w: &mut AsioWorker, o: &AsioOut, d: &mut FakeDevice, from: i64, n: i64) {
    for k in from..from + n {
        w.step(o, d, T0 + k * SLOT + 50_000, Some(block(k)));
        d.drain(25);
    }
}

#[test]
fn it_opens_at_the_drivers_rate_and_runs_without_an_underrun() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    assert_eq!(d.opened, vec![(DVS.to_string(), [0, 1])]);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.status.driver_rate, snap.status.sample_type), ("running", 96_000, "Int32LSB"));
    run(&mut w, &o, &mut d, 1, 30 * 5);
    assert_eq!(d.underruns, 0);
    let snap = o.snapshot();
    assert_eq!(snap.blocks_sent, 150);
    assert!((snap.status.latency_ms - (66.67 + 2.67 + 1.33)).abs() < 5.0, "{:?}", snap.status);
    assert!(d.played.iter().any(|&x| (x - 0.25).abs() < 1e-3), "the program reached the card");
}

#[test]
fn a_reset_request_closes_and_reopens_after_2_s() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    run(&mut w, &o, &mut d, 1, 30);
    d.events.push_back(DeviceEvents { reset: true, ..Default::default() });
    let now = T0 + 31 * SLOT;
    w.step(&o, &mut d, now, None);
    assert_eq!(d.closes, 1);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.reason.as_ref().map(Reason::code)), ("waiting", Some("reset")));
    assert_eq!(snap.status.resets, 1);
    w.step(&o, &mut d, now + 2 * S - 1, None);
    assert_eq!(d.opened.len(), 1, "not before 2 s");
    w.step(&o, &mut d, now + 2 * S, None);
    assert_eq!(d.opened.len(), 2);
    assert_eq!(o.snapshot().state, "running");
}

#[test]
fn a_busy_driver_retries_after_2_10_30_then_60_s() {
    let busy = || Err(Reason::Busy("init failed".into()));
    let o = out();
    let mut d = FakeDevice::answering(vec![busy(), busy(), busy(), busy(), busy(), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    let mut attempts = Vec::new();
    let mut t = T0;
    while d.opened.len() < 6 && t < T0 + 300 * S {
        let before = d.opened.len();
        w.step(&o, &mut d, t, None);
        if d.opened.len() > before {
            attempts.push((t - T0) / S);
        }
        t += S / 10;
    }
    assert_eq!(attempts, vec![0, 2, 12, 42, 102, 162]);
    let reason = o.snapshot().reason;
    assert_eq!(reason, None, "running now");
    assert_eq!(o.snapshot().state, "running");
}

#[test]
fn a_busy_driver_shows_its_reason_and_the_retry() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Err(Reason::Busy("init failed".into()))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    let snap = o.snapshot();
    assert_eq!(snap.state, "waiting");
    assert_eq!(snap.reason.unwrap().text(), "the driver refused to start (in use by another program?): init failed");
    assert_eq!(snap.status.retry_in_s, Some(2.0));
    assert_eq!(d.closes, 1, "a failed open releases the driver");
}

#[test]
fn a_vanished_driver_stalls_and_closes_with_its_reason() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    run(&mut w, &o, &mut d, 1, 30);
    // no more callbacks: the card is gone
    let mut t = T0 + 31 * SLOT;
    while o.snapshot().state == "running" && t < T0 + 10 * S {
        w.step(&o, &mut d, t, None);
        t += SLOT;
    }
    let snap = o.snapshot();
    assert_eq!(snap.reason.as_ref().map(Reason::code), Some("stalled"));
    assert!(t - (T0 + 31 * SLOT) >= 2 * S, "not before 2 s without a callback");
}

#[test]
fn a_rate_change_reopens_at_the_drivers_new_rate() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(48_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    run(&mut w, &o, &mut d, 1, 30);
    d.events.push_back(DeviceEvents { rate_changed: Some(48_000.0), ..Default::default() });
    let now = T0 + 31 * SLOT;
    w.step(&o, &mut d, now, None);
    assert_eq!(o.snapshot().reason, Some(Reason::RateChanged(48_000)));
    w.step(&o, &mut d, now + 2 * S, None);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.status.driver_rate), ("running", 48_000));
    // the new resampler makes ~1600 frames a block at 48 kHz (blocks on
    // boundaries 5 ms before they are handled, as everywhere here)
    let before = d.played.len();
    for k in 0..30 {
        let handled = now + 2 * S + k * SLOT;
        let b = ProgramBlock { due_100ns: handled - 50_000, samples: Some(vec![0.25; 3200].into()), substituted: false };
        w.step(&o, &mut d, handled, Some(b));
        d.drain(13);
    }
    assert!(d.played.len() - before > 2 * 1600 * 25, "the 48 kHz card plays");
}

#[test]
fn an_unsupported_sample_type_waits_with_its_reason() {
    let o = out();
    let refused = Reason::Refused(crate::playback::asio_format::unsupported_sample_text(20));
    let mut d = FakeDevice::answering(vec![Err(refused.clone())]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    assert_eq!(o.snapshot().reason, Some(refused));
}

#[test]
fn shutdown_closes_the_driver_once() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    w.shutdown(&mut d);
    assert_eq!(d.closes, 1);
}

#[test]
fn the_queue_drops_its_oldest_block_and_holds_the_delay() {
    let mut e = OutputEntry::asio("out-3", "DVS", AsioDest { driver: DVS.into(), channels: [0, 1] });
    e.delay_ms = 100;
    let o = AsioOut::for_entry(&e).unwrap();
    assert_eq!(o.target_100ns(), crate::playback::asrc_servo::BASE_LATENCY_100NS + 1_000_000);
    for k in 0..30 {
        o.push(block(k));
    }
    assert!(o.snapshot().blocks_dropped > 0);
    assert_eq!(o.queued(), crate::playback::vban_out::queue_bound(1_000_000));
}
```

(The 48 kHz drain: 13 callbacks × 128 = 1_664 frames per 1_600-frame boundary is slightly more than the program makes — enough for "it plays at the new rate"; underruns are not asserted there.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server asio_out` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement.** `audio_out_queue.rs`:

```rust
//! #233: a bounded, never-blocking block queue for an output's thread (the
//! ASIO output's; #210's `VbanOut` keeps its own): over the bound the OLDEST
//! block is dropped and counted.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::playback::audio_out_block::ProgramBlock;

#[derive(Debug, PartialEq)]
pub enum Take {
    Block(ProgramBlock),
    Idle,
    Stopped,
}

struct Inner {
    blocks: VecDeque<ProgramBlock>,
    stop: bool,
}

pub struct BlockQueue {
    inner: Mutex<Inner>,
    ready: Condvar,
    bound: usize,
    dropped: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl BlockQueue {
    pub fn new(bound: usize) -> Self {
        Self {
            inner: Mutex::new(Inner { blocks: VecDeque::with_capacity(bound + 1), stop: false }),
            ready: Condvar::new(),
            bound,
            dropped: AtomicU64::new(0),
        }
    }

    /// Never blocks; returns whether the oldest block was dropped.
    pub fn push(&self, block: ProgramBlock) -> bool {
        let over = {
            let mut q = lock(&self.inner);
            q.blocks.push_back(block);
            let over = q.blocks.len() > self.bound;
            if over {
                q.blocks.pop_front();
            }
            over
        };
        self.ready.notify_one();
        if over {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        over
    }

    pub fn take_timeout(&self, wait: Duration) -> Take {
        let q = lock(&self.inner);
        let (mut q, _) = self
            .ready
            .wait_timeout_while(q, wait, |q| q.blocks.is_empty() && !q.stop)
            .unwrap_or_else(|p| p.into_inner());
        match q.blocks.pop_front() {
            Some(b) => Take::Block(b),
            None if q.stop => Take::Stopped,
            None => Take::Idle,
        }
    }

    pub fn queued(&self) -> usize {
        lock(&self.inner).blocks.len()
    }

    pub fn stop(&self) {
        lock(&self.inner).stop = true;
        self.ready.notify_all();
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
```

`asio_out.rs`:

```rust
//! #233: the ASIO output. One worker thread per entry owns its driver (every
//! driver call on it; `asio_win.rs` on Windows, a scripted fake in the tests)
//! and runs `AsioWorker::step`:
//! - closed: open when the backoff allows (`asio_state::backoff_100ns`); the
//!   driver's CURRENT rate, preferred buffer, channels and sample type are
//!   read, never set; a new servo (`asrc_servo.rs`), resampler and splice
//!   (`asrc.rs`) for that rate; a ring sized for the target + 4 slots; start.
//!   A failed open releases the driver and shows its reason.
//! - running: each program block → the servo's observation (ring + splice
//!   hold, hand-off lateness, the card's consumed frames) → its correction to
//!   the resampler, its re-centre to the splice → the ring. Then the driver's
//!   messages: a reset (or a size change), a rate change or 2 s without a
//!   callback closes the output (`asio_state::close_reason`, `StallWatch`).
//! A closed output drops the blocks it is handed (they would be stale).
//! Status: `AsioOut::snapshot` → `GET /api/v1/program` `outputs[i].asio`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use sp_core::audio_outputs::OutputEntry;

use crate::playback::asio_format::AsioSample;
use crate::playback::asio_state::{
    DeviceEvents, POLL_100NS, Reason, StallWatch, admit_rate, asio_latency_ms, backoff_100ns,
    close_reason, failures_after_close, recentre_frames, ring_capacity_frames,
};
use crate::playback::asrc::{Asrc, Splice};
use crate::playback::asrc_servo::{BASE_LATENCY_100NS, Observation, Servo};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::{BlockQueue, Take};
use crate::playback::vban_out::{VbanClock, queue_bound};
use crate::playback::vban_packet::{VBAN_BLOCK_SAMPLES, VBAN_CHANNELS};

/// What the worker needs from a driver (`asio_win::WinAsioDevice`, the fake).
/// Not `Send`: the device lives and dies on its worker thread (COM STA).
pub trait AsioDevice {
    fn open(&mut self, driver: &str, channels: [u32; 2]) -> Result<Opened, Reason>;
    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason>;
    fn poll(&mut self) -> DeviceEvents;
    fn consumed_frames(&self) -> u64;
    fn underruns(&self) -> u64;
    fn mark_primed(&mut self);
    fn close(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opened {
    pub rate: f64,
    pub buffer_frames: u32,
    pub out_channels: u32,
    pub sample: AsioSample,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Started {
    pub output_latency_frames: u32,
}

/// `outputs[i].asio`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AsioStatus {
    pub driver: String,
    pub channels: [u32; 2],
    pub driver_rate: u32,
    pub buffer_frames: u32,
    pub out_channels: u32,
    /// The driver's sample type (`AsioSample::name`), "" before the first open.
    pub sample_type: &'static str,
    pub ppm: f64,
    pub rate_ppm: f64,
    pub locked: bool,
    pub latency_ms: f64,
    pub underruns: u64,
    pub resets: u64,
    pub recentres: u64,
    pub overflows: u64,
    pub retry_in_s: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AsioSnapshot {
    pub state: &'static str,
    pub reason: Option<Reason>,
    pub status: AsioStatus,
    pub blocks_sent: u64,
    pub blocks_dropped: u64,
}

struct Live {
    state: &'static str,
    reason: Option<Reason>,
    status: AsioStatus,
    blocks_sent: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One ASIO entry's shared side: its queue and its status.
pub struct AsioOut {
    queue: BlockQueue,
    driver: String,
    channels: [u32; 2],
    target_100ns: i64,
    live: Mutex<Live>,
    running: AtomicBool,
}

impl AsioOut {
    pub fn for_entry(entry: &OutputEntry) -> Result<Self, String> {
        let a = entry.asio.as_ref().ok_or_else(|| "not an ASIO entry".to_string())?;
        let delay_100ns = i64::from(entry.delay_ms) * 10_000;
        Ok(Self {
            queue: BlockQueue::new(queue_bound(delay_100ns)),
            driver: a.driver.clone(),
            channels: a.channels,
            target_100ns: BASE_LATENCY_100NS + delay_100ns,
            live: Mutex::new(Live {
                state: crate::playback::audio_out::STATE_OPENING,
                reason: None,
                status: AsioStatus { driver: a.driver.clone(), channels: a.channels, ..AsioStatus::default() },
                blocks_sent: 0,
            }),
            running: AtomicBool::new(false),
        })
    }

    pub fn target_100ns(&self) -> i64 {
        self.target_100ns
    }

    pub fn push(&self, block: ProgramBlock) {
        self.queue.push(block);
    }

    pub fn queued(&self) -> usize {
        self.queue.queued()
    }

    pub fn stop(&self) {
        self.queue.stop();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> AsioSnapshot {
        let live = lock(&self.live);
        AsioSnapshot {
            state: live.state,
            reason: live.reason.clone(),
            status: live.status.clone(),
            blocks_sent: live.blocks_sent,
            blocks_dropped: self.queue.dropped(),
        }
    }

    /// Off Windows: never runs, says why.
    pub fn set_windows_only(&self) {
        let mut live = lock(&self.live);
        live.state = crate::playback::audio_out::STATE_WAITING;
        live.reason = Some(Reason::WindowsOnly);
    }

    fn update(&self, f: impl FnOnce(&mut Live)) {
        f(&mut lock(&self.live));
    }
}

struct Run {
    opened: Opened,
    opened_at_100ns: i64,
    producer: rtrb::Producer<f32>,
    servo: Servo,
    asrc: Asrc,
    splice: Splice,
    zeros: Vec<f32>,
    stall: StallWatch,
    driver_latency_frames: u32,
    overflows: u64,
    primed: bool,
}

enum State {
    Closed { retry_at_100ns: i64 },
    Running(Box<Run>),
}

/// The worker's state machine (`run_asio_worker` loops on `step`).
pub struct AsioWorker {
    state: State,
    failures: u32,
    resets: u64,
}

impl AsioWorker {
    /// Closed, due to open at `now_100ns`.
    pub fn new(now_100ns: i64) -> Self {
        Self { state: State::Closed { retry_at_100ns: now_100ns }, failures: 0, resets: 0 }
    }

    /// One step at `now_100ns`, with the block the queue gave (if any).
    /// Returns how long the loop may wait for the next block.
    pub fn step(
        &mut self,
        out: &AsioOut,
        device: &mut dyn AsioDevice,
        now_100ns: i64,
        block: Option<ProgramBlock>,
    ) -> i64 {
        if let State::Closed { retry_at_100ns } = self.state {
            if now_100ns < retry_at_100ns {
                let left = retry_at_100ns - now_100ns;
                out.update(|l| l.status.retry_in_s = Some(left as f64 / 1e7));
                return left.min(POLL_100NS);
            }
            self.open(out, device, now_100ns);
            return POLL_100NS;
        }
        let State::Running(run) = &mut self.state else {
            return POLL_100NS;
        };
        if let Some(b) = block {
            process(run, out, device, now_100ns, b);
        }
        let ev = device.poll();
        let reason = close_reason(&ev, run.opened.rate)
            .or_else(|| run.stall.stalled(ev.callbacks, now_100ns).then_some(Reason::Stalled));
        match reason {
            Some(reason) => {
                let ran = now_100ns - run.opened_at_100ns;
                self.close(out, device, now_100ns, reason, ran);
            }
            None => publish(run, out, device),
        }
        POLL_100NS
    }

    fn open(&mut self, out: &AsioOut, device: &mut dyn AsioDevice, now_100ns: i64) {
        match build(out, device, now_100ns) {
            Ok(run) => {
                out.update(|l| {
                    l.state = crate::playback::audio_out::STATE_RUNNING;
                    l.reason = None;
                    l.status.driver_rate = run.opened.rate.round() as u32;
                    l.status.buffer_frames = run.opened.buffer_frames;
                    l.status.out_channels = run.opened.out_channels;
                    l.status.sample_type = run.opened.sample.name();
                    l.status.retry_in_s = None;
                });
                self.state = State::Running(Box::new(run));
            }
            Err(reason) => {
                device.close();
                self.failures += 1;
                self.wait(out, now_100ns, reason);
            }
        }
    }

    fn close(&mut self, out: &AsioOut, device: &mut dyn AsioDevice, now_100ns: i64, reason: Reason, ran_100ns: i64) {
        device.close();
        self.resets += 1;
        self.failures = failures_after_close(self.failures, ran_100ns);
        let resets = self.resets;
        out.update(|l| l.status.resets = resets);
        self.wait(out, now_100ns, reason);
    }

    fn wait(&mut self, out: &AsioOut, now_100ns: i64, reason: Reason) {
        let wait = backoff_100ns(self.failures);
        self.state = State::Closed { retry_at_100ns: now_100ns + wait };
        out.update(|l| {
            l.state = crate::playback::audio_out::STATE_WAITING;
            l.reason = Some(reason);
            l.status.retry_in_s = Some(wait as f64 / 1e7);
        });
    }

    /// Process shutdown: release the driver.
    pub fn shutdown(&mut self, device: &mut dyn AsioDevice) {
        device.close();
        self.state = State::Closed { retry_at_100ns: i64::MAX };
    }
}

/// Open, admit the rate, build the resampler, the ring and the servo, start.
fn build(out: &AsioOut, device: &mut dyn AsioDevice, now_100ns: i64) -> Result<Run, Reason> {
    let opened = device.open(&out.driver, out.channels)?;
    let rate = f64::from(admit_rate(opened.rate)?);
    let asrc = Asrc::new(rate).map_err(Reason::Failed)?;
    let capacity = ring_capacity_frames(rate, out.target_100ns, asrc.max_out_frames());
    let (producer, consumer) = rtrb::RingBuffer::new(capacity * VBAN_CHANNELS);
    let started = device.start(consumer)?;
    Ok(Run {
        opened: Opened { rate, ..opened },
        opened_at_100ns: now_100ns,
        producer,
        servo: Servo::new(rate, out.target_100ns),
        splice: Splice::new(rate, capacity, asrc.max_out_frames()),
        asrc,
        zeros: vec![0.0; VBAN_BLOCK_SAMPLES],
        stall: StallWatch::default(),
        driver_latency_frames: started.output_latency_frames,
        overflows: 0,
        primed: false,
    })
}

/// One program block into the ring (see the module doc).
fn process(run: &mut Run, out: &AsioOut, device: &mut dyn AsioDevice, now_100ns: i64, block: ProgramBlock) {
    let ring_frames = (run.producer.buffer().capacity() - run.producer.slots()) / VBAN_CHANNELS;
    let action = run.servo.observe(Observation {
        handled_100ns: now_100ns,
        stamp_100ns: block.due_100ns,
        buffered_frames: (ring_frames + run.splice.held_frames()) as u64,
        consumed_frames: device.consumed_frames(),
    });
    // In bounds by construction: the servo clamps to ±300 ppm, the resampler allows ±1000.
    let _ = run.asrc.set_correction_ppm(action.correction_ppm);
    let frames = recentre_frames(action.recentre_100ns, run.opened.rate);
    if frames > 0 {
        run.splice.insert(frames.unsigned_abs() as usize);
    } else if frames < 0 {
        run.splice.skip(frames.unsigned_abs() as usize);
    }
    let input = block.samples.as_deref().filter(|s| s.len() == VBAN_BLOCK_SAMPLES).unwrap_or(&run.zeros);
    let Ok(resampled) = run.asrc.process(input) else {
        return;
    };
    let spliced = run.splice.process(resampled);
    let (_, rest) = run.producer.push_partial_slice(spliced);
    run.overflows += (rest.len() / VBAN_CHANNELS) as u64;
    if !run.primed {
        device.mark_primed();
        run.primed = true;
    }
    out.update(|l| l.blocks_sent += 1);
}

/// The running output's numbers into its status.
fn publish(run: &Run, out: &AsioOut, device: &dyn AsioDevice) {
    let servo = run.servo.status();
    let latency = asio_latency_ms(servo.latency_ms, run.asrc.delay_frames(), run.driver_latency_frames, run.opened.rate);
    out.update(|l| {
        l.status.ppm = servo.correction_ppm;
        l.status.rate_ppm = servo.rate_ppm;
        l.status.locked = servo.locked;
        l.status.latency_ms = latency;
        l.status.recentres = servo.recentres;
        l.status.underruns = device.underruns();
        l.status.overflows = run.overflows;
    });
}

/// The worker thread's loop: wait for a block (bounded by the step's answer),
/// step, until stopped; then release the driver.
#[cfg_attr(test, mutants::skip)] // a blocking loop around AsioWorker::step (tested step by step)
pub fn run_asio_worker(out: &AsioOut, device: &mut dyn AsioDevice, clock: &mut dyn VbanClock) {
    out.running.store(true, Ordering::SeqCst);
    let mut worker = AsioWorker::new(clock.now_100ns());
    let mut wait_100ns = 0;
    loop {
        let block = match out.queue.take_timeout(Duration::from_nanos(wait_100ns.max(0) as u64 * 100)) {
            Take::Block(b) => Some(b),
            Take::Idle => None,
            Take::Stopped => break,
        };
        wait_100ns = worker.step(out, device, clock.now_100ns(), block);
    }
    worker.shutdown(device);
    out.running.store(false, Ordering::SeqCst);
}

#[cfg(test)]
#[path = "asio_out_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "asio_out_fake.rs"]
pub(crate) mod fake;
```

Notes for the implementer:
- In `step`, the first `if let State::Closed { retry_at_100ns } = self.state` copies the `i64` (the enum is not `Copy`; match on `&self.state` and copy the field if the borrow checker asks).
- `publish(run, out, device)` takes `&dyn AsioDevice`: pass `&*device`.
- The latency pin in `it_opens_at_the_drivers_rate…` (`± 5 ms` around 70.67) comes from the servo holding 66.67 + the resampler's 2.67 + the fake's 128-frame driver latency; verify with the scratch model before tightening.
- `the_queue_drops_its_oldest_block…`: `queue_bound(1_000_000)` = 10 + 4 = 14; 30 pushes drop 16.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server asio_out` and `cargo test -p sp-server audio_out_queue` — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the ASIO worker over a scripted driver — open, run, reset, stall, rate change, busy backoff` then `feat(#233): asio_out — the ASIO worker (servo, resampler, splice, ring) over a driver trait; rtrb`.

### Task 3.5: the Windows glue over `azo` (`asio_win.rs`), the mutation exclusion, the CI scan

**Files:**
- Create: `crates/sp-server/src/playback/asio_win.rs`
- Modify: `crates/sp-server/Cargo.toml` (`[target.'cfg(windows)'.dependencies]` `azo = "=0.2.1"`; windows-sys feature `"Win32_UI_WindowsAndMessaging"`), `Cargo.lock` (`cargo metadata`, as in Task 1.5: only added packages — azo, azo-sys, windows-bindgen / windows-core / windows-registry 0.100 …), `crates/sp-server/src/playback/mod.rs` (`#[cfg(windows)] pub mod asio_win; // #233: the ASIO output's azo (COM) glue — Windows only, out of the mutation gate`, next to the other `#[cfg(windows)]` modules), `.cargo/mutants.toml`, `.github/workflows/ci.yml` (`test-integrity` job: one step)

**Interfaces:**
- Consumes: `AsioDevice`, `Opened`, `Started`, `Reason`, `DeviceEvents`, `AsioSample`, `fill_channel`, `source_of`, `unsupported_sample_text`, `reply`, `selector`, `run_asio_worker`, `AsioOut`, `WallVbanClock`, `WallClock::system`, `pipeline_paced::request_high_res_timer`.
- Produces: `asio_win::{ASIO_SLOTS = 4, WinAsioDevice (new, impl AsioDevice), list_drivers() -> Vec<String>, spawn_asio_thread(out: Arc<AsioOut>, id: String)}`.

- [ ] **Step 1: Write the Windows-job tests** (inside `asio_win.rs`, `#[cfg(test)] mod tests`, so they compile and run only on the `Build (Windows)` job's `cargo test --workspace`; CI's hosted runner has no ASIO driver, the box several — the asserts hold on both):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_never_fails_and_an_unknown_driver_is_not_found_with_the_list() {
        let names = list_drivers();
        let mut d = WinAsioDevice::new();
        match d.open("No Such Card (songplayer test)", [0, 1]) {
            Err(Reason::NotFound { present }) => assert_eq!(present, names),
            other => panic!("{other:?}"),
        }
        d.close();
        d.close(); // idempotent
        assert_eq!(d.poll(), DeviceEvents::default(), "nothing open, nothing counted");
    }

    #[test]
    fn one_slot_per_asio_entry() {
        assert_eq!(ASIO_SLOTS, sp_core::audio_outputs::MAX_ASIO_OUTPUTS);
    }
}
```

- [ ] **Step 2: Run (CI only):** the `Build (Windows)` job — Expected now: FAIL to compile (`asio_win` missing).

- [ ] **Step 3: Implement** — `asio_win.rs` (the whole file is `#[cfg(windows)]` through its `mod` line; read `iemmixer/crates/iem-audio-io/src/asio.rs:580-830` first — this is the same slot / in-flight / zero-before-start discipline, output-only):

```rust
//! #233: the ASIO output's Windows glue over azo 0.2.1 (pure-Rust COM ASIO
//! host, MIT, no Steinberg SDK — iemmixer's choice,
//! `iemmixer/crates/iem-audio-io/src/asio.rs`). It only CALLS the driver;
//! every decision is in the Linux-tested `asio_format`, `asio_state`,
//! `asio_out`, `asrc_servo` and `asrc`, so this file is out of the mutation
//! gate (`.cargo/mutants.toml`, like `sp-gpu/src/win/`).
//! - Every driver call is made on the output's worker thread, which created
//!   the driver (COM STA: `create_instance` initialises it) and pumps its
//!   window messages in `poll`.
//! - It reads the driver's rate, preferred buffer, output channels and their
//!   sample type, and never sets the rate, the clock source or the buffer,
//!   nor opens the control panel (`ci.yml` scans `crates/` for those calls).
//! - ASIO callbacks carry no user pointer: [`ASIO_SLOTS`] static slots, each
//!   with its own four `extern "system"` callbacks, hold the running stream;
//!   an in-flight counter lets `close` free a stream only after the last
//!   callback left it (iemmixer `asio.rs:581-590`).
//! - The buffer switch copies only: it pops its frames from the ring
//!   (`rtrb`), writes L/R into the two configured channels and zeroes every
//!   other one (`asio_format::fill_channel`), and counts in atomics — no
//!   allocation, lock, log or syscall (iemmixer I7). Driver messages are
//!   answered by `asio_state::reply` and counted for `poll`.

use std::cell::UnsafeCell;
use std::ffi::{c_long, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use azo::dto::ChannelId;
use azo::sys::{Bool, Callbacks, MessageSelector, SampleRate, Time};
use azo::utils::com::InitGuard;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};

use crate::playback::asio_format::{AsioSample, fill_channel, source_of, unsupported_sample_text};
use crate::playback::asio_out::{AsioDevice, AsioOut, Opened, Started};
use crate::playback::asio_state::{DeviceEvents, Reason, reply, selector};

/// Static callback slots: one per ASIO entry (`MAX_ASIO_OUTPUTS`).
pub const ASIO_SLOTS: usize = 4;

/// One running stream, owned by a slot while its driver runs.
struct Stream {
    ring: UnsafeCell<rtrb::Consumer<f32>>,
    scratch: UnsafeCell<Vec<f32>>,
    buffers: Vec<[*mut c_void; 2]>,
    frames: usize,
    sample: AsioSample,
    left: usize,
    right: usize,
}

/// A slot's stream pointer and counters (static: a message that comes before
/// the stream exists is still counted).
struct Slot {
    stream: AtomicPtr<Stream>,
    in_flight: AtomicUsize,
    claimed: AtomicBool,
    primed: AtomicBool,
    callbacks: AtomicU64,
    consumed: AtomicU64,
    underruns: AtomicU64,
    reset: AtomicBool,
    resync: AtomicBool,
    size_change: AtomicBool,
    latencies: AtomicBool,
    overloads: AtomicU64,
    /// A new rate's f64 bits; 0 = none.
    rate_bits: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            stream: AtomicPtr::new(ptr::null_mut()),
            in_flight: AtomicUsize::new(0),
            claimed: AtomicBool::new(false),
            primed: AtomicBool::new(false),
            callbacks: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            reset: AtomicBool::new(false),
            resync: AtomicBool::new(false),
            size_change: AtomicBool::new(false),
            latencies: AtomicBool::new(false),
            overloads: AtomicU64::new(0),
            rate_bits: AtomicU64::new(0),
        }
    }

    fn clear_counters(&self) {
        for a in [&self.callbacks, &self.consumed, &self.underruns, &self.overloads, &self.rate_bits] {
            a.store(0, Ordering::SeqCst);
        }
        for f in [&self.primed, &self.reset, &self.resync, &self.size_change, &self.latencies] {
            f.store(false, Ordering::SeqCst);
        }
    }
}

static SLOTS: [Slot; ASIO_SLOTS] = [const { Slot::new() }; ASIO_SLOTS];

/// The buffer switch of `slot`: copy, convert, count — nothing else.
fn on_buffer(slot: &Slot, second: bool) {
    slot.in_flight.fetch_add(1, Ordering::SeqCst);
    let raw = slot.stream.load(Ordering::SeqCst);
    // SAFETY: a non-null slot points to a live stream: `close` clears the
    // slot and waits for `in_flight == 0` before it frees the stream, and
    // this callback counted itself before it read the slot.
    if let Some(stream) = unsafe { raw.as_ref() } {
        // SAFETY: one driver's callbacks never overlap; only they touch the ring + scratch.
        let (ring, scratch) = unsafe { (&mut *stream.ring.get(), &mut *stream.scratch.get()) };
        let want = stream.frames * 2;
        let got = ring.pop_partial_slice(&mut scratch[..want]).0.len();
        let bytes = stream.frames * stream.sample.bytes();
        for (ch, halves) in stream.buffers.iter().enumerate() {
            let p = halves[usize::from(second)];
            if p.is_null() {
                continue;
            }
            // SAFETY: the driver's half-buffer: `frames` samples of the
            // driver's type, which the driver does not touch during the callback.
            let dst = unsafe { std::slice::from_raw_parts_mut(p.cast::<u8>(), bytes) };
            fill_channel(stream.sample, &scratch[..got], source_of(ch, stream.left, stream.right), dst);
        }
        if got < want && slot.primed.load(Ordering::Relaxed) {
            slot.underruns.fetch_add(1, Ordering::Relaxed);
        }
        slot.consumed.fetch_add(stream.frames as u64, Ordering::Relaxed);
        slot.callbacks.fetch_add(1, Ordering::Relaxed);
    }
    slot.in_flight.fetch_sub(1, Ordering::SeqCst);
}

fn on_message(slot: &Slot, sel: c_long, value: c_long) -> c_long {
    match sel {
        selector::RESET_REQUEST => slot.reset.store(true, Ordering::SeqCst),
        selector::BUFFER_SIZE_CHANGE => slot.size_change.store(true, Ordering::SeqCst),
        selector::RESYNC_REQUEST => slot.resync.store(true, Ordering::SeqCst),
        selector::LATENCIES_CHANGED => slot.latencies.store(true, Ordering::SeqCst),
        selector::OVERLOAD => {
            slot.overloads.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    reply(sel, value)
}

macro_rules! slot_callbacks {
    ($i:literal, $bs:ident, $bsti:ident, $msg:ident, $rate:ident) => {
        unsafe extern "system" fn $bs(index: c_long, _direct: Bool) {
            on_buffer(&SLOTS[$i], index != 0);
        }
        unsafe extern "system" fn $bsti(params: *mut Time, index: c_long, _direct: Bool) -> *mut Time {
            on_buffer(&SLOTS[$i], index != 0);
            params
        }
        unsafe extern "system" fn $msg(
            sel: MessageSelector,
            value: c_long,
            _message: *const c_void,
            _opt: *const f64,
        ) -> c_long {
            on_message(&SLOTS[$i], sel.0, value)
        }
        unsafe extern "system" fn $rate(rate: SampleRate) {
            SLOTS[$i].rate_bits.store(rate.to_bits(), Ordering::SeqCst);
        }
    };
}

slot_callbacks!(0, bs0, bsti0, msg0, rate0);
slot_callbacks!(1, bs1, bsti1, msg1, rate1);
slot_callbacks!(2, bs2, bsti2, msg2, rate2);
slot_callbacks!(3, bs3, bsti3, msg3, rate3);

static CALLBACKS: [Callbacks; ASIO_SLOTS] = [
    Callbacks { buffer_switch: bs0, sample_rate_did_change: rate0, asio_message: msg0, buffer_switch_time_info: bsti0 },
    Callbacks { buffer_switch: bs1, sample_rate_did_change: rate1, asio_message: msg1, buffer_switch_time_info: bsti1 },
    Callbacks { buffer_switch: bs2, sample_rate_did_change: rate2, asio_message: msg2, buffer_switch_time_info: bsti2 },
    Callbacks { buffer_switch: bs3, sample_rate_did_change: rate3, asio_message: msg3, buffer_switch_time_info: bsti3 },
];

/// The registered ASIO drivers' descriptions (HKLM\SOFTWARE\ASIO, read only;
/// no driver is loaded). None registered → empty.
pub fn list_drivers() -> Vec<String> {
    azo::get_drivers()
        .map(|d| d.iter().map(|m| m.description.to_string_lossy()).collect())
        .unwrap_or_default()
}

fn text(s: &std::ffi::CStr) -> String {
    s.to_string_lossy().into_owned()
}

/// One ASIO output's driver, on its worker thread.
pub struct WinAsioDevice {
    driver: Option<InitGuard<azo::Driver>>,
    channels: [usize; 2],
    opened: Option<Opened>,
    slot: Option<usize>,
    stream: *mut Stream,
}

impl WinAsioDevice {
    pub fn new() -> Self {
        Self { driver: None, channels: [0, 1], opened: None, slot: None, stream: ptr::null_mut() }
    }

    fn call<'a>(driver: &'a azo::Driver, what: &'a str) -> impl Fn(azo::Error) -> Reason + 'a {
        move |e| Reason::Failed(format!("{what}: {e} ({})", text(&driver.last_error())))
    }
}

impl AsioDevice for WinAsioDevice {
    fn open(&mut self, name: &str, channels: [u32; 2]) -> Result<Opened, Reason> {
        self.close();
        let drivers = azo::get_drivers().unwrap_or_default();
        let present: Vec<String> = drivers.iter().map(|d| d.description.to_string_lossy()).collect();
        let Some(meta) = drivers.iter().find(|d| d.description.to_string_lossy() == name) else {
            return Err(Reason::NotFound { present });
        };
        let driver = meta.create_instance().map_err(|e| Reason::Busy(e.to_string()))?;
        if !driver.init(None) {
            return Err(Reason::Busy(text(&driver.last_error())));
        }
        let rate = driver.get_sample_rate().map_err(Self::call(&driver, "getSampleRate"))?;
        let size = driver.buffer_size().map_err(Self::call(&driver, "getBufferSize"))?;
        let counts = driver.channel_counts().map_err(Self::call(&driver, "getChannels"))?;
        let out_channels = u32::try_from(counts.out).unwrap_or(0);
        if channels.iter().any(|&c| c >= out_channels) {
            return Err(Reason::Refused(format!(
                "the driver has {out_channels} output channels (asked for {} and {})",
                channels[0] + 1,
                channels[1] + 1
            )));
        }
        let mut sample: Option<AsioSample> = None;
        for index in 0..counts.out {
            let info = driver
                .channel_info(ChannelId { input: false, index })
                .map_err(Self::call(&driver, "getChannelInfo"))?;
            let this = AsioSample::from_code(info.sample_type.0)
                .map_err(|code| Reason::Refused(unsupported_sample_text(code)))?;
            if sample.is_some_and(|s| s != this) {
                return Err(Reason::Refused("the driver's output channels mix sample types".into()));
            }
            sample = Some(this);
        }
        let sample = sample.ok_or_else(|| Reason::Refused("the driver has no output channel".into()))?;
        let buffer_frames = u32::try_from(size.preferred)
            .ok()
            .filter(|f| *f > 0)
            .ok_or_else(|| Reason::Refused(format!("the driver prefers a buffer of {}", size.preferred)))?;
        let opened = Opened { rate, buffer_frames, out_channels, sample };
        self.driver = Some(driver);
        self.channels = [channels[0] as usize, channels[1] as usize];
        self.opened = Some(opened);
        Ok(opened)
    }

    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        let (Some(driver), Some(opened)) = (self.driver.as_ref(), self.opened) else {
            return Err(Reason::Failed("start before open".into()));
        };
        let slot = (0..ASIO_SLOTS)
            .find(|&i| SLOTS[i].claimed.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok())
            .ok_or_else(|| Reason::Refused("four ASIO outputs are open already".into()))?;
        SLOTS[slot].clear_counters();
        self.slot = Some(slot);
        let frames = opened.buffer_frames as usize;
        let ids = (0..opened.out_channels as c_long).map(|index| ChannelId { input: false, index });
        // SAFETY: CALLBACKS is a static, so it outlives the buffers; the
        // buffers are written only by this slot's callback (between start and
        // stop) and zeroed below before start.
        let buffers: Vec<[*mut c_void; 2]> =
            unsafe { driver.create_buffers(ids, frames as c_long, &CALLBACKS[slot]) }
                .map_err(Self::call(driver, "createBuffers"))?
                .collect();
        let bytes = frames * opened.sample.bytes();
        for half in buffers.iter().flatten().filter(|p| !p.is_null()) {
            // SAFETY: a fresh half-buffer of `bytes`, no callback runs before start.
            unsafe { ptr::write_bytes(half.cast::<u8>(), 0, bytes) };
        }
        let stream = Box::into_raw(Box::new(Stream {
            ring: UnsafeCell::new(ring),
            scratch: UnsafeCell::new(vec![0.0; frames * 2]),
            buffers,
            frames,
            sample: opened.sample,
            left: self.channels[0],
            right: self.channels[1],
        }));
        self.stream = stream;
        SLOTS[slot].stream.store(stream, Ordering::SeqCst);
        driver.start().map_err(Self::call(driver, "start"))?;
        let latency = driver.latencies().map(|l| l.out).unwrap_or(0);
        Ok(Started { output_latency_frames: u32::try_from(latency).unwrap_or(0) })
    }

    fn poll(&mut self) -> DeviceEvents {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: a plain message pump on this (the driver's STA) thread.
        while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let Some(i) = self.slot else {
            return DeviceEvents::default();
        };
        let s = &SLOTS[i];
        let rate_bits = s.rate_bits.swap(0, Ordering::SeqCst);
        DeviceEvents {
            reset: s.reset.swap(false, Ordering::SeqCst),
            resync: s.resync.swap(false, Ordering::SeqCst),
            buffer_size_change: s.size_change.swap(false, Ordering::SeqCst),
            latencies_changed: s.latencies.swap(false, Ordering::SeqCst),
            rate_changed: (rate_bits != 0).then(|| f64::from_bits(rate_bits)),
            overloads: s.overloads.load(Ordering::Relaxed),
            callbacks: s.callbacks.load(Ordering::Relaxed),
        }
    }

    fn consumed_frames(&self) -> u64 {
        self.slot.map_or(0, |i| SLOTS[i].consumed.load(Ordering::Relaxed))
    }

    fn underruns(&self) -> u64 {
        self.slot.map_or(0, |i| SLOTS[i].underruns.load(Ordering::Relaxed))
    }

    fn mark_primed(&mut self) {
        if let Some(i) = self.slot {
            SLOTS[i].primed.store(true, Ordering::SeqCst);
        }
    }

    fn close(&mut self) {
        if let Some(d) = self.driver.as_ref() {
            let _ = d.stop();
        }
        if let Some(i) = self.slot.take() {
            let s = &SLOTS[i];
            s.stream.store(ptr::null_mut(), Ordering::SeqCst);
            // Bounded: a driver stuck inside a callback for a second leaks
            // its stream instead of freeing it under the callback.
            for _ in 0..1_000 {
                if s.in_flight.load(Ordering::SeqCst) == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if s.in_flight.load(Ordering::SeqCst) == 0 && !self.stream.is_null() {
                // SAFETY: the slot is cleared and no callback is inside it.
                drop(unsafe { Box::from_raw(self.stream) });
            }
            s.claimed.store(false, Ordering::SeqCst);
        }
        self.stream = ptr::null_mut();
        if let Some(d) = self.driver.as_ref() {
            let _ = d.dispose_all_buffers();
        }
        self.driver = None; // drops the driver, then CoUninitialize
        self.opened = None;
    }
}

impl Drop for WinAsioDevice {
    fn drop(&mut self) {
        self.close();
    }
}

/// The ASIO output's worker thread (`asio-<id>`): its own driver, its own
/// program-wall clock (`WallVbanClock::new`, following the wall: a date step
/// shows to the servo as a step it re-centres).
#[cfg_attr(test, mutants::skip)]
pub fn spawn_asio_thread(out: Arc<AsioOut>, id: String) {
    let name = format!("asio-{id}");
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        crate::playback::pipeline_paced::request_high_res_timer();
        tracing::info!(id = %id, "asio output thread started");
        let mut device = WinAsioDevice::new();
        let wall = crate::playback::wallclock::WallClock::system();
        let mut clock = crate::playback::vban_out::WallVbanClock::new(wall);
        crate::playback::asio_out::run_asio_worker(&out, &mut device, &mut clock);
        tracing::info!(id = %id, "asio output thread stopped");
    });
    if let Err(e) = spawned {
        tracing::error!(%e, "asio output: spawning the thread failed");
    }
}

// (the Step 1 tests module goes here)
```

Before writing it, read in `~/.cargo/registry/src/*/` (Tier-0: read, never build): `azo-0.2.1/src/lib.rs` (`get_drivers`, `create_instance`, `init`, `get_sample_rate`, `buffer_size`, `channel_counts`, `channel_info`, `create_buffers`, `start`, `stop`, `latencies`, `dispose_all_buffers`, `last_error`), `azo-sys-0.2.1/src/lib.rs` (`Callbacks`, the four fn-pointer types, `MessageSelector(pub c_long)`, `SampleType(pub c_long)`), and windows-sys 0.59's `PeekMessageW` signature (`HWND` is a pointer in 0.59; `rust-workspace.md` "Which windows-sys feature a Win32 call needs"). If `WallVbanClock` is not exported from `vban_out` under that path, use `crate::playback::vban_clock::WallVbanClock`.

`crates/sp-server/Cargo.toml`, under `[target.'cfg(windows)'.dependencies]`:

```toml
# #233: the ASIO output — azo, a pure-Rust COM host (MIT, no Steinberg SDK,
# no LLVM; iemmixer's choice). Pinned exactly (the spec's choice).
azo = "=0.2.1"
```

and add `"Win32_UI_WindowsAndMessaging",  # #233: the ASIO worker pumps its driver's window messages` to the windows-sys features.

`.cargo/mutants.toml`, in the STRUCTURAL block after the `sp-gpu/src/win/` entry:

```toml
  # playback/asio_win.rs (#233): the azo (COM) calls of the ASIO output, a
  #   #[cfg(windows)] module: dead on the Linux runner, so every mutant builds
  #   and survives. Its decisions live in the Linux-tested asio_format (the
  #   sample types, the channel fill), asio_state (backoff, close reasons,
  #   message replies, stall), asio_out (the worker's open / run / close over
  #   a fake driver), asrc_servo and asrc; the glue itself runs in the Windows
  #   job's no-driver test and the SNV / PP post-deploy ASIO gate.
  'sp-server/src/playback/asio_win',
```

`.github/workflows/ci.yml`, a step in the `test-integrity` job (ubuntu, bash):

```yaml
      - name: The ASIO host never sets the rate, the clock source or opens the panel (#233)
        run: |
          set -euo pipefail
          # Dante Controller / SoundGrid own the rate and the clock; the
          # output follows the driver (spec section 3; iemmixer I2).
          if grep -rnE '\.(set_sample_rate|set_clock_source|open_control_panel)\(' crates/; then
            echo "FAIL: SongPlayer's ASIO host must never set the driver's rate or clock, nor open its panel"
            exit 1
          fi
          echo "OK: no ASIO setter is called"
```

- [ ] **Step 4: Run (CI only):** the `Build (Windows)` job (compiles azo, runs the two tests), the Test Integrity job, `actionlint .github/workflows/ci.yml` locally — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): the ASIO glue on the Windows job — listing, an unknown driver, one slot per entry` then `feat(#233): asio_win — azo driver glue, four callback slots, the copy-only buffer switch` then `ci(#233): asio_win out of the mutation gate; scan for ASIO setters`.

### Task 3.6: ASIO outputs in the fan-out, the status, the task and the driver-list route

**Files:**
- Create: `crates/sp-server/src/api/audio.rs`, `crates/sp-server/src/api/audio_tests.rs`
- Modify: `crates/sp-server/src/playback/audio_out.rs`, `crates/sp-server/src/playback/audio_out_tests.rs`, `crates/sp-server/src/playback/audio_out_task.rs`, `crates/sp-server/src/playback/audio_out_task_tests.rs`, `crates/sp-server/src/api/mod.rs` (the module + one route), `crates/sp-server/src/api/program_tests_outputs.rs`

**Interfaces:**
- Consumes: `AsioOut`, `AsioSnapshot`, `AsioStatus`, `Reason`, `rate_note`, `asio_win::{spawn_asio_thread, list_drivers}` (Windows).
- Produces: `OutputSink::Asio(Arc<AsioOut>)`; `OutputStatus.{note: Option<String>, asio: Option<AsioStatus>}`; `RunningOutput::status(&self, network_rate: u32)`; `build_rate(Asio) = 0`; `GET /api/v1/audio/asio-drivers` → `{"drivers": [..]}`.

- [ ] **Step 1: Write the failing tests.** Append to `audio_out_tests.rs`:

```rust
#[test]
fn the_status_notes_a_driver_rate_off_the_network_rate() {
    use crate::playback::asio_out::fake::{FakeDevice, dvs};
    use crate::playback::asio_out::{AsioOut, AsioWorker};
    use sp_core::audio_outputs::AsioDest;
    let entry = OutputEntry::asio("out-3", "DVS", AsioDest { driver: "Dante Virtual Soundcard (x64)".into(), channels: [0, 1] });
    let out = Arc::new(AsioOut::for_entry(&entry).unwrap());
    let mut d = FakeDevice::answering(vec![Ok(dvs(48_000.0))]);
    AsioWorker::new(0).step(&out, &mut d, 0, None);
    let running = RunningOutput { entry, built_rate: 0, sink: Some(OutputSink::Asio(out)), error: None };
    let at96 = running.status(96_000);
    assert_eq!((at96.kind, at96.state, at96.rate, at96.format), ("asio", STATE_RUNNING, 48_000, "Int32LSB"));
    assert_eq!(at96.note.as_deref(), Some("the driver runs at 48000 Hz, the network at 96000 Hz"));
    assert_eq!(at96.asio.as_ref().unwrap().driver, "Dante Virtual Soundcard (x64)");
    assert!(at96.vban.is_none());
    assert_eq!(running.status(48_000).note, None);
}
```

Append to `audio_out_task_tests.rs`:

```rust
#[test]
fn a_network_rate_change_never_rebuilds_an_asio_entry() {
    use sp_core::audio_outputs::AsioDest;
    let dvs = OutputEntry::asio("out-3", "DVS", AsioDest { driver: "d".into(), channels: [0, 1] });
    assert_eq!(build_rate(&dvs, 96_000), 0, "the driver's rate, not the network's");
    let p = plan(&[ran(&dvs, 0)], std::slice::from_ref(&dvs), 44_100);
    assert_eq!(p.steps, vec![Step::Keep(0)]);
}
```

(and in that file's `vban()` helper add the arm `Some(OutputSink::Asio(_)) => panic!("{} is an ASIO output", o.entry.id),`.)

Append to `program_tests_outputs.rs`:

```rust
/// Off Windows an ASIO output never opens; it says why. (On Windows a real
/// worker opens the named driver; the Windows job's `asio_win` tests and the
/// box gate cover that path.)
#[cfg(not(windows))]
#[tokio::test]
async fn an_asio_entry_off_windows_waits_and_says_why() {
    use sp_core::audio_outputs::AsioDest;
    let state = test_state().await;
    let dvs = OutputEntry::asio("out-3", "DVS", AsioDest { driver: "Dante Virtual Soundcard (x64)".into(), channels: [0, 1] });
    let settings = crate::playback::audio_out_config::OutputsSettings { entries: vec![dvs], network_rate: 96_000, problems: vec![] };
    crate::playback::audio_out_task::apply(state.program_bus.outputs(), settings, &mut HashMap::new()).await;
    let json = get_program(&state).await;
    let o = &json["outputs"][0];
    assert_eq!((o["type"].as_str(), o["state"].as_str()), (Some("asio"), Some("waiting")));
    assert_eq!(o["reason"], "ASIO runs on Windows only");
    assert_eq!(o["asio"]["driver"], "Dante Virtual Soundcard (x64)");
    assert_eq!(o["asio"]["channels"], serde_json::json!([0, 1]));
}
```

`api/audio_tests.rs`:

```rust
//! #233: `GET /api/v1/audio/asio-drivers` through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};

#[tokio::test]
async fn the_driver_list_answers_a_list() {
    let state = test_state().await;
    let req = Request::builder().uri("/api/v1/audio/asio-drivers").body(Body::empty()).unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["drivers"].is_array());
    #[cfg(not(windows))]
    assert_eq!(json["drivers"], serde_json::json!([]), "no ASIO off Windows");
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server audio_out` and `cargo test -p sp-server api::` — Expected now: FAIL to compile.

- [ ] **Step 3: Implement.**

`audio_out.rs`: `OutputSink` gains `Asio(Arc<AsioOut>)` (its `push` → `out.push(block)`, `stop` → `out.stop()`); `OutputStatus` gains, after `vban`,

```rust
    /// #233: a driver whose rate is not the network's (a WARN once).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asio: Option<AsioStatus>,
```

(`note: None, asio: None` in the VBAN arm); `RunningOutput::status` takes `network_rate: u32` and dispatches to `vban_status(&self)` (Lane 1's body, its closure pattern now a `match` on `OutputSink::Vban(out)`) or

```rust
    fn asio_status(&self, network_rate: u32) -> OutputStatus {
        let e = &self.entry;
        let snap = match &self.sink {
            Some(OutputSink::Asio(out)) => Some(out.snapshot()),
            _ => None,
        };
        let (state, reason) = match (&snap, &self.error) {
            _ if !e.enabled => (STATE_DISABLED, None),
            (_, Some(err)) => (STATE_WAITING, Some(err.clone())),
            (Some(s), None) => (s.state, s.reason.as_ref().map(Reason::text)),
            (None, None) => (STATE_OPENING, None),
        };
        let status = snap.as_ref().map(|s| s.status.clone());
        let rate = status.as_ref().map_or(0, |s| s.driver_rate);
        OutputStatus {
            id: e.id.clone(),
            kind: e.kind.as_str(),
            name: e.name.clone(),
            enabled: e.enabled,
            state,
            reason,
            rate,
            format: status.as_ref().map_or("", |s| s.sample_type),
            channels: 2,
            delay_ms: e.delay_ms,
            latency_ms: status.as_ref().map_or(0.0, |s| s.latency_ms),
            blocks_sent: snap.as_ref().map_or(0, |s| s.blocks_sent),
            blocks_dropped: snap.as_ref().map_or(0, |s| s.blocks_dropped),
            vban: None,
            note: (state == STATE_RUNNING).then(|| rate_note(rate, network_rate)).flatten(),
            asio: status,
        }
    }
```

and `AudioOutputs::status` maps `|o| o.status(self.network_rate())`.

`audio_out_task.rs`: `build_rate` gains `OutputType::Asio => 0,` (doc: "an ASIO output follows its driver"); `build` gains

```rust
        OutputType::Asio => match AsioOut::for_entry(entry) {
            Ok(out) => {
                let out = Arc::new(out);
                start_asio_thread(&out, &entry.id);
                log_started(entry, built_rate);
                output.sink = Some(OutputSink::Asio(out));
            }
            Err(e) => output.error = Some(e),
        },
```

with

```rust
/// The output's worker thread on Windows; elsewhere it says why it never opens.
fn start_asio_thread(out: &Arc<AsioOut>, id: &str) {
    #[cfg(windows)]
    crate::playback::asio_win::spawn_asio_thread(out.clone(), id.to_string());
    #[cfg(not(windows))]
    {
        let _ = id;
        out.set_windows_only();
    }
}
```

(not `mutants::skip`: on Linux its effect is pinned by `an_asio_entry_off_windows_waits_and_says_why`).

`api/audio.rs`:

```rust
//! #233: `GET /api/v1/audio/asio-drivers` — the ASIO drivers this box has
//! registered (HKLM\SOFTWARE\ASIO, read only; no driver is loaded), for the
//! dashboard's driver list. Empty off Windows.

use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AsioDrivers {
    pub drivers: Vec<String>,
}

pub async fn get_asio_drivers() -> Json<AsioDrivers> {
    #[cfg(windows)]
    let drivers = tokio::task::spawn_blocking(crate::playback::asio_win::list_drivers)
        .await
        .unwrap_or_default();
    #[cfg(not(windows))]
    let drivers = Vec::new();
    Json(AsioDrivers { drivers })
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
```

`api/mod.rs`: `mod audio;` (alphabetical among the api modules) and, next to `/api/v1/program`: `.route("/api/v1/audio/asio-drivers", axum::routing::get(audio::get_asio_drivers))` (grep the router first, `rust-workspace.md` "Use the route's REAL method").

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server audio_out`, `cargo test -p sp-server api::`, and the Windows job — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): ASIO outputs in the status and the task; the driver-list route` then `feat(#233): ASIO outputs run from the list — status with the rate note, GET /api/v1/audio/asio-drivers`.

### Task 3.7: the ASIO fields in "Zvukové výstupy"

**Files:**
- Modify: `sp-ui/src/components/audio_outputs.rs`, `e2e/settings-audio-outputs.spec.ts`, `e2e/mock-api.mjs`

**Interfaces:**
- Consumes: `sp_core::audio_outputs::{new_asio, OutputType, AsioDest}`, `GET /api/v1/audio/asio-drivers`, `outputs[i].asio`.
- Produces: testids `audio-outputs-add-asio`, `audio-output-asio-driver` (select), `audio-output-asio-left`, `audio-output-asio-right` (1-based number inputs), `audio-output-type` (text: "VBAN" / "ASIO"); the VBAN fields render only for a VBAN entry, the ASIO ones only for an ASIO entry; the rate select shows "podľa ovládača" (disabled) for ASIO.

- [ ] **Step 1: Write the failing Playwright tests** (append to `settings-audio-outputs.spec.ts`):

```ts
test("an ASIO output: pick the driver, channels 3 and 4, save; it runs at the network rate (#233)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await page.locator('[data-testid="audio-outputs-add-asio"]').click();
  const row = page.locator('[data-testid="audio-output-row"]');
  await expect(row.locator('[data-testid="audio-output-type"]')).toHaveText("ASIO");
  await expect(row.locator('[data-testid="audio-output-vban-host"]')).toHaveCount(0);
  await row.locator('[data-testid="audio-output-asio-driver"]').selectOption("Blackmagic ASIO");
  await row.locator('[data-testid="audio-output-asio-left"]').fill("3");
  await row.locator('[data-testid="audio-output-asio-right"]').fill("4");
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Uložené");
  expect(JSON.parse(patches[0]["audio_outputs"] as string)).toEqual([
    { id: "out-1", name: "ASIO 1", type: "asio", enabled: true, rate: "network", delay_ms: 0,
      asio: { driver: "Blackmagic ASIO", channels: [2, 3] } },
  ]);
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.outputs[0].type).toBe("asio");
  await expect(page.locator('[data-testid="audio-output-state"]')).toContainText("ppm", { timeout: 10000 });
  expect(realConsoleErrors()).toEqual([]);
});

test("two ASIO outputs on one driver are refused in Slovak (#233)", async ({ page }) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await page.locator('[data-testid="audio-outputs-add-asio"]').click();
  await page.locator('[data-testid="audio-outputs-add-asio"]').click();
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText(
    "Výstup 2 (out-2): ovládač už má iný výstup ASIO (ovládač berie jedného klienta)",
  );
  expect(patches).toHaveLength(0);
  expect(realConsoleErrors()).toEqual([]);
});
```

- [ ] **Step 2: Run** (mock suite on the current dist) — Expected: FAIL (`audio-outputs-add-asio` missing).

- [ ] **Step 3: Implement.** In `audio_outputs.rs`:
  - a `drivers: RwSignal<Vec<String>>` loaded once on mount: `leptos::task::spawn_local(async move { if let Ok(d) = api::get::<AsioDriverList>("/api/v1/audio/asio-drivers").await { let _ = drivers.try_set(d.drivers); } })` with `#[derive(Deserialize)] struct AsioDriverList { drivers: Vec<String> }`;
  - the button `<button type="button" data-testid="audio-outputs-add-asio" on:click=add_asio>"Pridať výstup ASIO"</button>` where `add_asio` pushes `new_asio(l, drivers.get_untracked().first().map(String::as_str).unwrap_or(""))`;
  - `OutputLive` gains `#[serde(default)] pub asio: Option<AsioLive>` with `#[derive(Clone, Debug, Default, Deserialize, PartialEq)] pub struct AsioLive { #[serde(default)] pub ppm: f64, #[serde(default)] pub underruns: u64 }`; `live_text` for a running ASIO output: `format!("{} · {:.0} ms · {:+.1} ppm · výpadky {}", state_sk(..), o.latency_ms, a.ppm, a.underruns)`;
  - in `OutputRow`: `let kind = move || read(entries, &id.get_value(), |e| Some(e.kind));` then a `<span data-testid="audio-output-type">{move || if kind() == Some(OutputType::Asio) { "ASIO" } else { "VBAN" }}</span>`; wrap the VBAN fields in `<Show when=move || kind() == Some(OutputType::Vban)>`; add, in `<Show when=move || kind() == Some(OutputType::Asio)>`:

```rust
            <label>
                "Ovládač"
                <select
                    data-testid="audio-output-asio-driver"
                    prop:value=move || asio(|a| a.driver.clone())
                    on:change=move |ev| {
                        let v = event_target_value(&ev);
                        edit(entries, &id.get_value(), |e| {
                            if let Some(a) = e.asio.as_mut() {
                                a.driver = v;
                            }
                        });
                    }
                >
                    {move || {
                        let current = asio(|a| a.driver.clone());
                        let mut names = drivers.get();
                        if !current.is_empty() && !names.contains(&current) {
                            names.push(current.clone());
                        }
                        names
                            .into_iter()
                            .map(|n| {
                                let label = if drivers.get().contains(&n) { n.clone() } else { format!("{n} (nenájdený)") };
                                view! { <option value=n>{label}</option> }
                            })
                            .collect_view()
                    }}
                </select>
            </label>
            <label>
                "Kanál ľavý"
                <input
                    type="number"
                    min="1"
                    max="512"
                    data-testid="audio-output-asio-left"
                    prop:value=move || asio(|a| (a.channels[0] + 1).to_string())
                    on:input=move |ev| {
                        let v = event_target_value(&ev).trim().parse::<u32>().unwrap_or(1).max(1) - 1;
                        edit(entries, &id.get_value(), |e| {
                            if let Some(a) = e.asio.as_mut() {
                                a.channels[0] = v;
                            }
                        });
                    }
                />
            </label>
```

    and the same for "Kanál pravý" (`audio-output-asio-right`, `channels[1]`), where `asio` is the row's twin of `vban` (`read(entries, &id.get_value(), move |e| e.asio.as_ref().map(f).unwrap_or_default())`); the rate `<select>` gets `disabled=move || kind() == Some(OutputType::Asio)` and, for ASIO, a first option `<option value="network">"podľa ovládača"</option>` label via a `move ||` text.

`e2e/mock-api.mjs`: `app.get("/api/v1/audio/asio-drivers", (_req, res) => res.json({ drivers: ["Dante Virtual Soundcard (x64)", "Blackmagic ASIO"] }));`; `outputsRefusal` gains, per entry `e.type === "asio"`: `!e.asio || !e.asio.driver` → `${at}: asio.driver is empty`; `e.asio.channels[0] === e.asio.channels[1]` → `${at}: asio.channels must name two different channels`; an earlier ASIO entry with the same driver → `${at}: asio.driver is already used by an earlier ASIO entry (a driver takes one client)`; `mockOutputs` gives an ASIO entry `rate: network`, `format: "Int32LSB"`, `latency_ms: 70.7`, and `asio: { driver, channels, driver_rate: network, sample_type: "Int32LSB", ppm: 0, underruns: 0, resets: 0, latency_ms: 70.7 }`.

- [ ] **Step 4: Run** — `rustfmt --edition 2024 --check sp-ui/src/components/audio_outputs.rs`; `node --check e2e/mock-api.mjs`; CI `Build WASM` + `Frontend E2E` (the whole settings + slovak-only suite) — Expected: PASS.

- [ ] **Step 5: Commit** — `test(#233): Zvukové výstupy — an ASIO output, one driver twice refused` then `feat(#233): the ASIO fields in Zvukové výstupy (driver list, channels 1-based, live ppm)`.

### Task 3.8: the ASIO live gates (SNV, PP), docs, push, the box entries

**Files:**
- Create: `e2e/post-deploy-audio-asio.spec.ts`
- Modify: `e2e/audio-outputs-gate.ts`, `e2e/audio-outputs-gate.spec.ts`, `.github/workflows/ci.yml` (one env line in the post-deploy step), `.claude/rules/audio-outputs.md`, `CLAUDE.md` (the router line), and — only if #229 lane 6 landed — `e2e/post-deploy-pp.config.ts` + `.github/workflows/deploy-pp.yml`

**Interfaces:**
- Consumes: `outputs[i].asio`, `GET /api/v1/audio/asio-drivers`.
- Produces: `audio-outputs-gate.ts::{AsioTelemetry, DVS_DRIVER = "Dante Virtual Soundcard (x64)", WINDOW_BLOCKS = 1800, asioGateFailures(first: OutputStatus, second: OutputStatus): string[]}`.

- [ ] **Step 1: Write the failing unit tests** (append to `audio-outputs-gate.spec.ts`):

```ts
import { asioGateFailures } from "./audio-outputs-gate";

const dvs = (blocks: number, over: Partial<OutputStatus> = {}, asio: Record<string, number> = {}): OutputStatus => ({
  id: "out-3", type: "asio", name: "DVS", enabled: true, state: "running", reason: null,
  rate: 96000, format: "Int32LSB", channels: 2, delay_ms: 0, latency_ms: 70.7, blocks_sent: blocks,
  blocks_dropped: 0,
  asio: { driver: "Dante Virtual Soundcard (x64)", channels: [0, 1], driver_rate: 96000, sample_type: "Int32LSB",
    ppm: 3.2, underruns: 0, resets: 0, latency_ms: 70.7, ...asio },
  ...over,
});

test.describe("ASIO gate (#233)", () => {
  test("60 s at the driver's rate with no underrun passes", () => {
    expect(asioGateFailures(dvs(100), dvs(1900))).toEqual([]);
  });
  test("each failure is named", () => {
    const bad = dvs(1000, { state: "waiting", reason: "the driver asked for a reset", rate: 48000, latency_ms: 0 },
      { underruns: 4, resets: 1, ppm: 301 });
    expect(asioGateFailures(dvs(100), bad)).toEqual([
      "the ASIO output is waiting (the driver asked for a reset)",
      "it runs at 48000 Hz, the driver at 96000 Hz",
      "4 underruns in the window",
      "the driver was reopened 1 times in the window",
      "its correction is 301 ppm (bound 300)",
      "its latency is 0 ms",
      "only 900 blocks in the window, want 1800",
    ]);
  });
});
```

- [ ] **Step 2: Run** (mock suite) — Expected: FAIL (`asioGateFailures` missing).

- [ ] **Step 3: Implement.** In `audio-outputs-gate.ts`:

```ts
export interface AsioTelemetry {
  driver: string;
  channels: number[];
  driver_rate: number;
  sample_type: string;
  ppm: number;
  underruns: number;
  resets: number;
  latency_ms: number;
}
// and in OutputStatus: `asio?: AsioTelemetry; note?: string;`

export const DVS_DRIVER = "Dante Virtual Soundcard (x64)";
/** One minute of program blocks. */
export const WINDOW_BLOCKS = 1800;

export function asioGateFailures(first: OutputStatus, second: OutputStatus): string[] {
  const f: string[] = [];
  const a = first.asio;
  const b = second.asio;
  if (second.state !== "running") f.push(`the ASIO output is ${second.state}${second.reason ? ` (${second.reason})` : ""}`);
  if (!a || !b) return [...f, "no ASIO telemetry"];
  if (b.driver_rate <= 0 || second.rate !== b.driver_rate) f.push(`it runs at ${second.rate} Hz, the driver at ${b.driver_rate} Hz`);
  if (b.underruns > a.underruns) f.push(`${b.underruns - a.underruns} underruns in the window`);
  if (b.resets > a.resets) f.push(`the driver was reopened ${b.resets - a.resets} times in the window`);
  if (Math.abs(b.ppm) > 300) f.push(`its correction is ${b.ppm} ppm (bound 300)`);
  if (!(second.latency_ms > 0 && second.latency_ms < 1000)) f.push(`its latency is ${second.latency_ms} ms`);
  const blocks = second.blocks_sent - first.blocks_sent;
  if (blocks < WINDOW_BLOCKS) f.push(`only ${blocks} blocks in the window, want ${WINDOW_BLOCKS}`);
  return f;
}
```

`e2e/post-deploy-audio-asio.spec.ts`:

```ts
/**
 * #233 post-deploy ASIO gate (SNV; PP's subset when it exists):
 * 1. The box has Dante Virtual Soundcard registered as an ASIO driver.
 * 2. Exactly SP_ASIO_OUTPUTS_EXPECTED enabled ASIO outputs exist (ci.yml; the
 *    main session sets it after it adds the box's entry), and each one runs at
 *    its driver's rate with 0 underruns, no reopen, |ppm| ≤ 300 and a latency
 *    over one minute of program blocks (`asioGateFailures`). Read-only.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import { DVS_DRIVER, WINDOW_BLOCKS, asioGateFailures, type OutputStatus } from "./audio-outputs-gate";

const EXPECTED = Number(process.env.SP_ASIO_OUTPUTS_EXPECTED ?? "0");

async function outputs(request: APIRequestContext): Promise<OutputStatus[]> {
  const resp = await request.get("/api/v1/program");
  expect(resp.status(), "GET /api/v1/program").toBe(200);
  return (await resp.json()).outputs as OutputStatus[];
}

function byId(list: OutputStatus[], id: string): OutputStatus {
  const o = list.find((x) => x.id === id);
  expect(o, `output ${id}`).toBeDefined();
  return o as OutputStatus;
}

test.describe("ASIO output (#233)", () => {
  test("the box has Dante Virtual Soundcard as an ASIO driver", async ({ request }) => {
    const resp = await request.get("/api/v1/audio/asio-drivers");
    expect(resp.status()).toBe(200);
    const drivers = (await resp.json()).drivers as string[];
    console.log(`[#233 asio] drivers: ${JSON.stringify(drivers)}`);
    expect(drivers).toContain(DVS_DRIVER);
  });

  test("every enabled ASIO output runs at its driver's rate with 0 underruns over a minute", async ({ request }) => {
    test.setTimeout(150_000);
    const asio = (await outputs(request)).filter((o) => o.type === "asio" && o.enabled);
    expect(asio.length, "enabled ASIO outputs (SP_ASIO_OUTPUTS_EXPECTED)").toBe(EXPECTED);
    for (const first of asio) {
      console.log(`[#233 asio] first: ${JSON.stringify(first)}`);
      await expect
        .poll(async () => byId(await outputs(request), first.id).blocks_sent - first.blocks_sent, {
          message: "one minute of program blocks",
          timeout: 120_000,
        })
        .toBeGreaterThanOrEqual(WINDOW_BLOCKS);
      const second = byId(await outputs(request), first.id);
      console.log(`[#233 asio] second: ${JSON.stringify(second)}`);
      expect(asioGateFailures(first, second), `ASIO output ${first.id}`).toEqual([]);
    }
  });
});
```

`.github/workflows/ci.yml`, in the "Feature-level Playwright (post-deploy spec)" step's `env:`:

```yaml
          # #233: how many enabled ASIO outputs the box must run (the main
          # session adds SNV's DVS entry after the lane-3 deploy, then sets
          # "1"); e2e/post-deploy-audio-asio.spec.ts.
          SP_ASIO_OUTPUTS_EXPECTED: "0"
```

PP (only if `deploy-pp.yml` + `e2e/post-deploy-pp.config.ts` exist): add `"**/post-deploy-audio-asio.spec.ts"` to the PP config's `testMatch` and the same env line (`"0"`) to `deploy-pp.yml`'s Playwright step; `actionlint` both.

Docs: append to `.claude/rules/audio-outputs.md` a section "## ASIO outputs (`asio_out.rs`, `asio_win.rs`, `asio_format.rs`, `asio_state.rs`)": one entry per driver (DVS takes one client), at most 4; the worker thread owns the driver (COM STA), reads and never sets rate / clock / buffer (CI scan); the four callback slots and the copy-only callback; sample types (incl. `Int32LSB16–24`); close reasons and the 2 / 10 / 30 / 60 s backoff (a 60 s run resets it); the rate note; latency = servo + resampler + driver; `GET /api/v1/audio/asio-drivers`; the fake driver for tests; the box gate and `SP_ASIO_OUTPUTS_EXPECTED`; "the ASIO worker is not MMCSS (never above the driver's thread)". Update the `CLAUDE.md` router line's tail "lanes 2–3: the drift servo and ASIO" to "ASIO outputs: `azo` on a per-output COM thread, read-only driver settings, four callback slots, the camera-box servo + rubato `Async`, 2/10/30/60 s reopen, `GET /api/v1/audio/asio-drivers`, the SNV/PP gate".

- [ ] **Step 4: Run** — mock suite (`audio-outputs-gate.spec.ts`) PASS locally; `actionlint .github/workflows/ci.yml`.

- [ ] **Step 5: Commit** — `test(#233): the ASIO gate function` then `test(#233): SNV post-deploy ASIO gate — DVS registered; enabled ASIO outputs run clean for a minute` then `ci(#233): SP_ASIO_OUTPUTS_EXPECTED for the box gate` then `docs(#233): ASIO outputs in the audio-outputs rules + CLAUDE.md`.

- [ ] **Step 6: Lane checks, then push.** `cargo fmt --all --check`; `wc -l` ≤ 1000 for every touched `.rs` (`asio_out.rs` and `asio_win.rs` each < 500); `cargo mutants --in-diff … --list` mapped (no `asio_win` mutant listed; `start_asio_thread`'s body killed by `an_asio_entry_off_windows_waits_and_says_why`); `git push origin dev`; CI to terminal state (incl. the Windows job's azo build and `asio_win` tests, the integrity scan, the box E2E with `SP_ASIO_OUTPUTS_EXPECTED: "0"`).

- [ ] **Step 7: MAIN SESSION OPS — SNV, after the deploy is green:**
  1. `curl -s http://10.77.9.201:8920/api/v1/audio/asio-drivers` → contains "Dante Virtual Soundcard (x64)".
  2. Add the entry (keep the stored ones byte-for-byte; the task keeps them running):
     ```bash
     B=http://10.77.9.201:8920
     cur=$(curl -s "$B/api/v1/settings" | jq -r '.audio_outputs')
     new=$(jq -c '. + [{"id":"out-3","name":"DVS","type":"asio","enabled":true,"rate":"network","delay_ms":0,"asio":{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}]' <<<"$cur")
     jq -n --arg v "$new" '{audio_outputs:$v}' | curl -s -X PATCH -H 'content-type: application/json' --data @- "$B/api/v1/settings" -w '%{http_code}\n'
     ```
  3. Watch `curl -s $B/api/v1/program | jq '.outputs[] | select(.type=="asio")'` for 2 minutes: `running`, `rate` = DVS's 96000, `asio.sample_type`, `asio.underruns` 0, `asio.ppm` small (SNV follows the Dante PTP leader: expect |ppm| ≲ 5), `latency_ms` ≈ 70. SongPlayer's log: `asio output thread started`. If `waiting` with `busy`: another program holds DVS's ASIO — stop and ask the owner (❓), never kill it. The owner may listen on FOH's cg channel; fohabl is not touched.
  4. Set `SP_ASIO_OUTPUTS_EXPECTED: "1"` in `ci.yml` (`ci(#233): the SNV box runs one ASIO output`), push, CI to terminal state: the gate now measures the DVS output for a minute on every SNV deploy.
  5. PP, after the main release carrying Lane 3 reaches PP: the same `curl` steps against PP's SongPlayer (reached the way #229's phase 0 reaches PP), then read the output for 2 minutes (PP's dantesync is on NTP fallback: expect a real |ppm| and watch it lock after ~60 s). If PP's CI subset exists, set its `SP_ASIO_OUTPUTS_EXPECTED` to `"1"` the same way.
  6. Post the evidence on #233 (both sites' `outputs[]` JSON, the CI run ids); tell the camera-box session that SongPlayer is now DVS's ASIO client at SNV (and PP).

---

## Self-review (run against the spec)

**Spec coverage — every requirement → its task:**

| Spec item | Where |
|---|---|
| §1 one setting `audio_outputs`, the field table (`id`, `name`, `type`, `enabled`, `rate`, `delay_ms` 0..=2000, `vban`, `asio`) | 1.1 (model), 1.2 (parse), 3.1 (`asio`) |
| §1 `audio_network_rate` (48000 default, SNV 96000) | 1.1, 1.2; SNV value: Lane 1 ops Step 7.2 |
| §1 ASIO ignores `rate`; a mismatch is a WARN + status note | 3.6 (`build_rate` 0, `note`), 3.3 (`rate_note`) |
| §1 typed validation, never `Value`; 400 names entry + field | 1.2 (`Reader` over `RawValue` maps), 3.1 |
| §1 re-read every 5 s; only a changed entry rebuilt | 1.8 (`plan`, `apply`) |
| §1 migration (one entry per target, int24, 48000 fixed, same stream, old keys deleted) | 1.3; FOH byte pin 1.4 + 1.6 |
| §2 fan-out after the limiter, own queue + thread each, drop-oldest, not in `program_bus.rs` | 1.6 (queue bound), 1.7 (fan-out), 3.4 (ASIO queue) |
| §3 VBAN: genlock-paced, fixed ratio with rubato, rate index + format per destination, ≤ 256 frames / ≤ 1436 B | 1.4, 1.5, 1.6 |
| §3 ASIO via azo; read the current rate / buffer / channels / sample type, never set; never change the buffer while open | 3.5 (+ CI scan) |
| §3 L/R to the two channels, others silent; f32 → Int32/Float32/Int24/Int16 (+ Int32LSB16–24); unknown refused | 3.2, 3.5 |
| §3 callback copies only from an `rtrb` SPSC ring; empty ring = silence + an underrun | 3.5 (`on_buffer`), 3.2 (`fill_channel`) |
| §3 a per-device worker runs the resampler and fills the ring | 3.4 |
| §4 one rubato `Async` sinc stage with `set_resample_ratio_relative` | 2.3 |
| §4 servo: long-window LS rate (applied after 60 s), PI ±50 / ±3, ±300 ppm, ≤ 5 ppm/s, re-centre on a step, rate estimate undisturbed | 2.1, 2.2, 2.3 (`Splice`) |
| §4 pure, Linux-tested, mutation-gated; only the azo glue Windows-only | 2.x, 3.2–3.4; 3.5 (mutants exclusion) |
| §5 each output reports its real latency; `delay_ms` per output | 1.6 (VBAN delay), 1.7 (`vban_latency_ms`), 3.3–3.4 (ASIO latency, target + delay) |
| §6 close on reset / vanished / busy; 2 / 10 / 30 / 60 s; reason shown; start backoff after release; others unaffected | 3.3, 3.4; isolation 1.7 |
| §7 `outputs[]` with id, type, name, state, rate, format, channels, latency, sent / dropped; ASIO ppm, fill, underruns, resets; VBAN packets, errors, late events | 1.9, 3.6 (`fill_ms` is reported as the servo's `latency_ms` + `asio.latency_ms`; see gaps) |
| §7 Nastavenia "Zvukové výstupy" (list, add, remove, enable, per-type fields, network rate, live state); the ASIO driver dropdown from `GET /api/v1/audio/asio-drivers` | 1.10, 3.6, 3.7 |
| §7 Playwright on the mock, zero console errors | 1.10, 3.7 |
| §8 pure tests: parse, migration, rate plan, VBAN headers per rate + format, f32 → driver type, channel map, servo simulation (±50 ppm, ±1 / ±44 ms, dropped buffer, ≤ 300, ≤ 5 ppm/s, fill held) | 1.1–1.4, 1.8, 3.2, 2.4 |
| §8 ASIO sink through a fake driver: rate, reset, vanished, busy, backoff | 3.4 |
| §8 live gates: VBAN FOH 48 kHz `sp-program`; ASIO at SNV and PP; a 96 kHz VBAN receiver reads index 4 | 1.11, 3.8 |
| §9 three serial lanes | Lanes 1–3 |
| Out of scope: program at 96 kHz, ASIO input, fohabl / VB-Matrix / Ableton | nothing in the plan touches them |
| Open points: DVS settings at SNV, `audio_network_rate` 96000, DVS channels | Lane 1 ops 7.2; Lane 3 ops 7.2–7.3 (channels 1/2 default) |

**Placeholder scan:** no TBD / TODO / "similar to"; every code step carries its code. Two pins are marked "verify with the scratch model before tightening" (the ASIO worker's ±5 ms latency band, rubato's ±4-frame count) — tolerances given, not left open.

**Type consistency checked:** `ProgramBlock { due_100ns, samples: Option<Arc<[f32]>>, substituted }` everywhere (1.6 → 1.7 → 3.4); `OutputEntry` built only by `OutputEntry::vban` / `::asio` / the parser (1.1 rule, honoured in 1.3, 1.6, 1.8, 1.9, 3.x tests); `RunningOutput { entry, built_rate, sink, error }` (1.7, 1.8, 1.9, 3.6); `OutputSink::{Vban, Asio}` (1.7 → 3.6, with the exhaustive matches updated in 3.6); `RunningOutput::status()` gains `network_rate` in 3.6 (one caller, `AudioOutputs::status`); `Servo::new(rate, target_100ns)` / `observe(Observation) -> ServoAction` (2.2 → 3.4); `Asrc::{new, set_correction_ppm, process, delay_frames, max_out_frames}` and `Splice::{new(rate, max_insert, max_block), insert, skip, process, held_frames}` (2.3 → 3.4); `AsioDevice` (3.4 → 3.5 fake + Windows); `AsioStatus.sample_type: &'static str` (3.4 → 3.6 `format`); `queue_bound(delay_100ns)` (1.6 → 3.4); `VbanClock` reused as the worker's wall clock (3.4, 3.5).

**Review Focus:** five lines, each with its test in the owning task (1 → 1.10, 2 → 1.8, 3 → 1.2 + 1.9, 4 → 3.1 + 3.4, 5 → 3.4 + 3.6).

## Spec gaps and disagreements (for the main session / the owner)

1. **The ASIO fill target "2–3 driver buffers plus a few ms" (§4) cannot hold** with one 33 ms program block per boundary arriving 10–33 ms late in normal operation; the plan holds the boundary-to-play latency at two slots (66.7 ms, VBAN's budget) + `delay_ms`. Expect ~71 ms ASIO latency (+ DVS's own 10 ms "Low" latency inside the driver figure).
2. **ASIO sample types beyond the spec's four:** `Int32LSB16/18/20/24` are accepted (DVS "AsioEncoding 24" may report code 27); refusing them could fail the SNV gate on day one.
3. **VBAN's fixed ratio uses rubato `Fft` (both sides fixed), not `Async`** — exact `rate/30` frames per block for the packet schedule; ~8.3 ms delay at every rate.
4. **The migration is a Rust startup step, not a numbered migration** (V30 is #229's, in flight). A rollback to ≤ 0.72 after Lane 1 has deleted `vban_*` leaves VBAN off until the keys are PATCHed back (FOH would lose SongPlayer — FOH listens to VBAN6 since 4.10).
5. **`GET /api/v1/program` drops its top-level `vban`** (now `outputs[i].vban`); camera-box's script reads only `source` / `remote.program_scene`.
6. **"Re-centre under a short crossfade"** is implemented as a 5 ms fade out, the gap or the skip, a 5 ms fade in (no click; a ±44 ms step is a short gap / skip, not an inaudible crossfade).
7. **`fill_ms` (§7)** is reported as the servo's measured boundary-to-play latency (`asio.latency_ms`), which is the ring fill plus the hand-off lateness; a raw instantaneous ring fill is sawtooth noise (0–33 ms per block). Say if the raw figure is wanted too.
8. **The PP ASIO gate** depends on #229 lane 6 (PP's CI subset); until it lands, PP is a manual read after a main release. PP gets main releases only, so PP's ASIO output starts only after the release PR.
9. **DVS is single-client at SNV:** who holds its ASIO driver today is unknown (cg OBS / camera-box?) — Lane 3 ops step 1 checks before the entry is added; a holder means an owner decision, not a kill.
10. **azo `=0.2.1` is pinned per the spec**, but azo 0.4.0 exists (30.9.2026; 0.3.x on 27–28.9). iemmixer validated 0.2.1 on hardware; moving up is a separate decision.
11. **The VBAN stall window counts packets** (`VbanStallLog`), so at 96 kHz `late_max_us` covers 30–60 s instead of 60–120 s; documented, not changed.
12. **The MIT notices of rubato / rtrb / azo** are not added to `THIRD-PARTY-NOTICES.txt` (the project lists no Rust crates there today); one file if the owner wants them.

## Main-session rulings on the gaps above (7.10.2026)

These are technical decisions. They bind the lane workers.

1. **Accepted:** the two-slot ASIO latency (~71 ms plus `delay_ms`). It matches the VBAN budget, and `delay_ms` aligns each destination.
2. **Accepted:** `Int32LSB16/18/20/24` are accepted.
3. **Accepted:** VBAN uses rubato `Fft`.
4. **Changed for rollback safety:** Lane 1 migrates into `audio_outputs` but does **NOT delete** the `vban_*` keys. The new code ignores them once `audio_outputs` exists, so a rollback to ≤ 0.72 still finds them and FOH keeps sound. Lane 3 deletes them, once the list has run a release.
5. **Accepted:** the top-level `vban` block moves into `outputs[i].vban`. Lane 1 updates every e2e/post-deploy reader of it in the same lane.
6. **Accepted:** the 5 ms fade out / gap or skip / 5 ms fade in.
   **Reversed 8.10.2026 by the owner** ("prečo sa tu bavíme o preskokoch, keď
   sa má jednať o inteligentný resampling?", #233 comment 6053850076; design
   6054367985): the resampler absorbs a difference smoothly, by its ratio
   (a stop curve within ±300 ppm at ≤ 5 ppm/s); the fade / gap / fade is now
   only the last resort when the ring would otherwise run dry (50 ms short) or
   overflow (four slots over), counted as a fault (`hard_recentres`), and the
   priming of an open, which is no re-centre (`.claude/rules/audio-outputs.md`).
7. **Accepted:** `fill_ms` is the boundary-to-play latency. The raw ring fill is not exposed.
8. **Accepted:** PP's ASIO gate rides #229 lane 6 (integrated 7.10.2026, runner `resolume-pp` online).
9. **Resolved 7.10.2026:** no process holds `dvs_asio_x64.dll` at SNV (`tasklist /m`), so DVS's single ASIO client is free. Lane 3 still re-checks it before the entry is added.
10. **Changed:** Lane 3 evaluates **azo 0.4.0** first (newest, per the owner's newest-only rule). If its API fits, it takes 0.4.0; if not, it pins 0.2.1, with the reason on #233.
11. **Accepted:** the documented packet-count window.
12. **Changed:** the MIT notices of rubato, rtrb and azo are added to `THIRD-PARTY-NOTICES.txt` in the lane that adds each crate (Lane 1: rubato; Lane 2: none; Lane 3: rtrb, azo). The installer ships that file.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-10-07-audio-outputs-asio.md`. Execution: the main session dispatches the three lanes serially (one worker per lane, one push per lane, CI to terminal state before the next), doing the **MAIN SESSION OPS** steps itself at the points marked (camera-box heads-ups, SNV's `audio_network_rate`, the DVS holder check, the SNV and PP ASIO entries and the gate's env flip).
