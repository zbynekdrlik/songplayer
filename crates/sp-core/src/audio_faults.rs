//! #257 lane 2: the audio fault ledger as the dashboard shows it.
//!
//! The owner (12.10.2026): "song player zobrazoval pocet problemov a nejaky
//! cas/graph aby bolo videt ako dlho to je bez chyby alebo ked sa problem
//! udial", and "akékoľvek puknutie, strata samplu je neakceptovateľná" for
//! every audio path. sp-server's ledger answers `GET
//! /api/v1/audio/faults?hours=N` with [`FaultsAnswer`]: ONE wire type, the
//! server serializes it and the dashboard reads it.
//!
//! Placed by usage frequency (#248): the HealthBar's one segment for the
//! whole chain is the first glance of every service ([`segment`]); the
//! per-path rows, their timelines and the event list are one click away
//! (the panel's texts below, the timeline's maths in
//! `audio_faults_timeline`). WASM-safe, unit-tested and mutation-gated:
//! sp-ui only renders what these return.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::audio_outputs::PROGRAM_RATE;
use crate::health::HealthTone;

/// The window the HealthBar's segment reads (hours).
pub const SEGMENT_HOURS: u32 = 24;

/// The panel's windows (hours): 1 h, 24 h, 7 d, 30 d.
pub const WINDOW_HOURS: [u32; 4] = [1, 24, 168, 720];

/// A loss this recent turns the segment red (ms; older: amber).
pub const RECENT_MS: i64 = 600_000;

/// How often the HealthBar asks the ledger (ms, the page's clock).
pub const POLL_EVERY_MS: u64 = 10_000;

/// Failed requests in a row after which the segment no longer shows its
/// last answer as current: grey `Zvuk: nedostupné` (#257 review).
pub const MAX_FAILED_POLLS: u32 = 3;

/// A row's severity: samples lost, inserted or wrong.
pub const LOSS: &str = "loss";

/// A row's severity: late or squeezed, the samples still whole.
pub const RISK: &str = "risk";

/// A path's (re)start row's kind: no fault, a mark on the timeline.
pub const START: &str = "start";

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

/// One row of a path's answer: a fault (`loss`, `risk`) or the path's
/// start (`info`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultEvent {
    /// UTC, ms.
    pub ts_ms: i64,
    pub kind: String,
    /// `loss` | `risk` | `info`.
    pub severity: String,
    /// A counter's step, an event's magnitude (frames, µs), 1 for a state
    /// or a start.
    pub amount: i64,
}

/// A loop check's time in the window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopSeconds {
    pub checked_s: f64,
    pub blind_s: f64,
}

/// One audio path of the answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathFaults {
    /// `program`, `pipeline:<playlist id>`, `out:<output id>`,
    /// `loop:<output id>`.
    pub path: String,
    /// The path's name (an output's configured name, a pipeline's NDI
    /// name).
    pub detail: String,
    /// The path runs now.
    pub live: bool,
    /// The path's last loss the ledger keeps (looked up beyond the asked
    /// window), else its first start the ledger keeps (a planned restart is
    /// no fault: it never resets this), else the ledger's oldest instant
    /// (`kept_from_ms`) when it has rows of the path, else now.
    pub clean_since_ms: i64,
    pub last_loss_ms: Option<i64>,
    pub last_risk_ms: Option<i64>,
    /// The window's `loss` rows: each one fault recorded at its time (a
    /// counter that grew in one 5 s tick, an event, a state that began).
    #[serde(default)]
    pub losses: i64,
    /// The window's `risk` rows, counted the same way.
    #[serde(default)]
    pub risks: i64,
    /// Faults per kind: a counter's steps summed, one per event or state.
    #[serde(default)]
    pub counts: BTreeMap<String, i64>,
    /// The rows that are no meter, oldest first (capped: the newest
    /// rows of the whole answer, `truncated`); the event list only.
    #[serde(default)]
    pub events: Vec<FaultEvent>,
    /// The window cut into `audio_faults_timeline::BUCKETS` buckets (edges
    /// `bucket_edges`): each bucket's loss rows, ALL of them — the
    /// timeline's bars never come from the capped `events`.
    #[serde(default)]
    pub loss_buckets: Vec<u32>,
    /// Each bucket's risk rows, the same way.
    #[serde(default)]
    pub risk_buckets: Vec<u32>,
    /// The path's (re)starts in the window (UTC ms, oldest first; the
    /// newest the server keeps per path): the timeline's marks.
    #[serde(default)]
    pub starts: Vec<i64>,
    #[serde(rename = "loop", default, skip_serializing_if = "Option::is_none")]
    pub loop_check: Option<LoopSeconds>,
}

/// `GET /api/v1/audio/faults`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaultsAnswer {
    /// The server's clock when it answered (UTC, ms): every "ago" counts
    /// from it, never from the browser's clock (another machine).
    pub now_ms: i64,
    pub from_ms: i64,
    pub hours: u32,
    /// The oldest instant the ledger keeps (its 90 days): a clean time that
    /// reaches it is "more than" it.
    #[serde(default)]
    pub kept_from_ms: i64,
    /// Since when the whole chain has lost no sample: the newest loss of any
    /// path the ledger keeps (beyond the asked window), else the first start
    /// it recorded. A planned restart (a deploy) never resets it.
    #[serde(default)]
    pub clean_since_ms: i64,
    /// The path with the most recent loss (`null`: none in the window).
    pub worst: Option<String>,
    /// More events than the answer carries: the oldest are left out.
    #[serde(default)]
    pub truncated: bool,
    pub paths: Vec<PathFaults>,
}

/// The URL of the answer over `hours`.
pub fn faults_url(hours: u32) -> String {
    format!("/api/v1/audio/faults?hours={hours}")
}

/// The segment's level, the HealthBar's `data-level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioLevel {
    /// No answer yet (or one with no path): grey.
    Unknown,
    /// No loss and no risk in the window: green.
    Ok,
    /// No loss, but risks: green, the risks counted beside it.
    Risk,
    /// A loss in the window, none in the last [`RECENT_MS`]: amber.
    Loss,
    /// A loss in the last [`RECENT_MS`]: red.
    Recent,
}

impl AudioLevel {
    /// The `data-level` value.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ok => "ok",
            Self::Risk => "risk",
            Self::Loss => "loss",
            Self::Recent => "recent",
        }
    }

    /// The segment's colour: a risk is no loss, so it stays green.
    pub fn tone(self) -> HealthTone {
        match self {
            Self::Unknown => HealthTone::Off,
            Self::Ok | Self::Risk => HealthTone::Ok,
            Self::Loss => HealthTone::Warn,
            Self::Recent => HealthTone::Error,
        }
    }
}

/// Every path's loss rows in the window.
pub fn total_losses(answer: &FaultsAnswer) -> i64 {
    answer.paths.iter().map(|p| p.losses).sum()
}

/// Every path's risk rows in the window.
pub fn total_risks(answer: &FaultsAnswer) -> i64 {
    answer.paths.iter().map(|p| p.risks).sum()
}

/// The path with the most recent loss (the answer's `worst`; on a tie the
/// later path, as the server picks it).
pub fn worst_path(answer: &FaultsAnswer) -> Option<&PathFaults> {
    answer
        .paths
        .iter()
        .filter_map(|p| p.last_loss_ms.map(|t| (t, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// The level of `answer` (`None`: not answered yet).
pub fn level(answer: Option<&FaultsAnswer>) -> AudioLevel {
    let Some(answer) = answer.filter(|a| !a.paths.is_empty()) else {
        return AudioLevel::Unknown;
    };
    match worst_path(answer).and_then(|p| p.last_loss_ms) {
        Some(at) if answer.now_ms - at < RECENT_MS => AudioLevel::Recent,
        Some(_) => AudioLevel::Loss,
        None if total_risks(answer) > 0 => AudioLevel::Risk,
        None => AudioLevel::Ok,
    }
}

/// A span in Slovak, rounded down: `menej ako minútu`, `12 min`,
/// `2 h 14 min`, `2 h`, `3 d 4 h`, `3 d`.
pub fn duration_sk(ms: i64) -> String {
    if ms < MINUTE_MS {
        return "menej ako minútu".to_string();
    }
    let minutes = ms / MINUTE_MS;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, mins) {
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// How long ago, in Slovak: `pred 5 min`, `pred menej ako minútou`.
pub fn ago_sk(ms: i64) -> String {
    if ms < MINUTE_MS {
        "pred menej ako minútou".to_string()
    } else {
        format!("pred {}", duration_sk(ms))
    }
}

/// A window as the selector names it: `1 h`, `24 h`, `7 d`, `30 d`.
pub fn window_label(hours: u32) -> String {
    if hours < 48 {
        format!("{hours} h")
    } else {
        duration_sk(i64::from(hours) * HOUR_MS)
    }
}

/// `n` with its Slovak noun: 1 `one`, 2–4 `few`, else `many` (#256's
/// Nastavenia line uses it too).
pub(crate) fn counted(n: i64, one: &str, few: &str, many: &str) -> String {
    let word = match n {
        1 => one,
        2..=4 => few,
        _ => many,
    };
    format!("{n} {word}")
}

/// `1 chyba`, `3 chyby`, `5 chýb`.
pub fn losses_sk(n: i64) -> String {
    counted(n, "chyba", "chyby", "chýb")
}

/// `1 riziko`, `3 riziká`, `5 rizík`.
pub fn risks_sk(n: i64) -> String {
    counted(n, "riziko", "riziká", "rizík")
}

/// A path's name for the operator: `Program`, `Vstup OBS manuál` (the NDI
/// input), an output's configured name, `Slučka – <output name>`, a
/// playlist's name (`playlist`, when the dashboard knows it) or `Playlist
/// <id>`; an unknown path its `detail`.
pub fn path_name(path: &str, detail: &str, playlist: Option<&str>) -> String {
    let named = |fallback: &str| {
        if detail.trim().is_empty() {
            fallback.to_string()
        } else {
            detail.to_string()
        }
    };
    if path == "program" {
        "Program".to_string()
    } else if path == "input" {
        "Vstup OBS manuál".to_string()
    } else if let Some(id) = path.strip_prefix("loop:") {
        format!("Slučka – {}", named(id))
    } else if let Some(id) = path.strip_prefix("out:") {
        named(id)
    } else if let Some(id) = path.strip_prefix("pipeline:") {
        match playlist {
            Some(name) if !name.trim().is_empty() => name.to_string(),
            _ => format!("Playlist {id}"),
        }
    } else {
        named(path)
    }
}

/// The playlist id of a `pipeline:<id>` path.
pub fn pipeline_id(path: &str) -> Option<i64> {
    path.strip_prefix("pipeline:")?.parse().ok()
}

/// The output id of an `out:<id>` or `loop:<id>` path.
pub fn output_id(path: &str) -> Option<&str> {
    path.strip_prefix("out:")
        .or_else(|| path.strip_prefix("loop:"))
}

/// A path's name, its playlist looked up by `playlist_name`.
pub fn name_of(p: &PathFaults, playlist_name: &dyn Fn(i64) -> Option<String>) -> String {
    let playlist = pipeline_id(&p.path).and_then(playlist_name);
    path_name(&p.path, &p.detail, playlist.as_deref())
}

/// The sample rate a path's frames count at: the program's for the program
/// and a pipeline, the output's (`rate_by_output`, by id) for an output or its
/// loop.
pub fn rate_of(path: &str, rate_by_output: &dyn Fn(&str) -> Option<u32>) -> Option<u32> {
    if path == "program" || pipeline_id(path).is_some() {
        Some(PROGRAM_RATE)
    } else {
        output_id(path).and_then(rate_by_output)
    }
}

/// An output's rate as `GET /api/v1/program` lists it: an ASIO driver's
/// own rate when it reads one, else the output's rate; 0 is unknown.
pub fn output_rate(rate: u32, driver_rate: Option<u32>) -> Option<u32> {
    driver_rate
        .filter(|r| *r > 0)
        .or(Some(rate))
        .filter(|r| *r > 0)
}

/// A kind in Slovak; an unknown kind as sent.
pub fn kind_sk(kind: &str) -> &str {
    match kind {
        "dropout" => "výpadok",
        "mismatch" => "nesedí zvuk",
        "slip" => "posun",
        "underrun" => "podtečenie",
        // #257: a pipeline's underrun at a song's start / at its end.
        "underrun_start" => "podtečenie na začiatku",
        "underrun_end" => "podtečenie na konci",
        "resync" => "resync",
        "hard_recentre" => "tvrdé re-centrovanie",
        "reset" => "reset",
        "blocks_dropped" => "zahodený blok",
        "blocks_substituted" => "nahradený blok",
        "send_error" => "chyba odoslania",
        "late_packet" => "oneskorený paket",
        "filled" => "vyplnené tichom",
        "late_callback" => "oneskorené volanie",
        "overflow" => "pretečenie",
        "host_down" => "hostiteľ ASIO nebeží",
        "no_return" => "slučka sa nevracia",
        "late_dropped" => "neskoro zahodené",
        "coalesced" => "zahodený blok programu",
        "interrupted" => "prerušenie",
        "fast_callback" => "predčasné volanie",
        "overload" => "preťaženie",
        "no_source" => "bez zdroja",
        "unsupported" => "nepodporovaný formát",
        "vban_feed_late" => "VBAN dodaný neskoro",
        "limited" => "limiter",
        "ndi_late" => "NDI neskoro",
        "unserviced" => "neobslúžený slot",
        "pump_missed" => "vynechaný blok Dante",
        "pump_late" => "oneskorený blok Dante",
        "pump_overrun" => "prekročený blok Dante",
        START => "spustenie",
        other => other,
    }
}

/// What an event's amount measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// A counter's step: how many.
    Count,
    /// Audio frames (one sample per channel).
    Frames,
    /// Microseconds.
    Micros,
    /// A state or a start: no amount.
    Nothing,
}

/// The unit of `kind`'s amount on `path`: an ASIO output's `overflow`
/// counts frames, a pipeline's counts the times its audio grid dropped
/// its oldest samples.
pub fn unit_of(path: &str, kind: &str) -> Unit {
    match kind {
        "overflow" if path.starts_with("pipeline:") => Unit::Count,
        "mismatch" | "dropout" | "slip" | "overflow" | "limited" => Unit::Frames,
        "late_packet" => Unit::Micros,
        "host_down" | "no_return" | "start" => Unit::Nothing,
        _ => Unit::Count,
    }
}

/// Milliseconds with one decimal and a decimal comma: `12,5 ms`.
fn ms_sk(ms: f64) -> String {
    format!("{ms:.1} ms").replace('.', ",")
}

/// An event's amount on `path` with its unit: `×2` (a count of 1 says
/// nothing), `12,5 ms`, frames as ms at `rate_hz` (a slip keeps its sign),
/// else `480 vzoriek`; `None` for a state.
pub fn amount_sk(path: &str, kind: &str, amount: i64, rate_hz: Option<u32>) -> Option<String> {
    match unit_of(path, kind) {
        Unit::Nothing => None,
        Unit::Count => (amount != 1).then(|| format!("×{amount}")),
        Unit::Micros => Some(ms_sk(amount as f64 / 1000.0)),
        Unit::Frames => Some(match rate_hz.filter(|r| *r > 0) {
            Some(rate) => ms_sk(amount as f64 * 1000.0 / f64::from(rate)),
            None => format!("{amount} {}", frames_word(amount.unsigned_abs())),
        }),
    }
}

/// `vzorka`, `vzorky`, `vzoriek` for `n`.
fn frames_word(n: u64) -> &'static str {
    match n {
        1 => "vzorka",
        2..=4 => "vzorky",
        _ => "vzoriek",
    }
}

/// The HealthBar's segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub level: AudioLevel,
    /// `Zvuk: bez chyby 2 h 14 min`, `Zvuk: 3 chyby za 24 h · posledná pred
    /// 5 min · <path>`, `Zvuk: —`.
    pub text: String,
    /// ` · riziká: N` beside a green text with risks (a risk is no loss,
    /// but it is never hidden).
    pub risks: Option<String>,
    /// The tooltip.
    pub tip: String,
}

/// The HealthBar's one segment for the whole audio chain, from the
/// [`SEGMENT_HOURS`] answer.
pub fn segment(
    answer: Option<&FaultsAnswer>,
    playlist_name: &dyn Fn(i64) -> Option<String>,
) -> Segment {
    let level = level(answer);
    let Some(a) = answer.filter(|_| level != AudioLevel::Unknown) else {
        return Segment {
            level,
            text: "Zvuk: —".to_string(),
            risks: None,
            tip: "História chýb zvuku sa ešte nenačítala.".to_string(),
        };
    };
    let (losses, risks) = (total_losses(a), total_risks(a));
    let window = window_label(a.hours);
    let tip = format!(
        "Všetky zvukové cesty za posledných {window}: {}, {}. Kliknutím zobrazíte podrobnosti.",
        losses_sk(losses),
        risks_sk(risks)
    );
    let worst = worst_path(a);
    let text = match worst.and_then(|p| p.last_loss_ms.map(|at| (p, at))) {
        Some((p, at)) => format!(
            "Zvuk: {} za {window} · posledná {} · {}",
            losses_sk(losses),
            ago_sk(a.now_ms - at),
            name_of(p, playlist_name)
        ),
        None => format!("Zvuk: bez chyby {}", clean_span_sk(a.clean_since_ms, a)),
    };
    let suffix = (worst.is_none() && risks > 0).then(|| format!(" · riziká: {risks}"));
    Segment {
        level,
        text,
        risks: suffix,
        tip,
    }
}

/// A local time, as the browser reads an instant in its own time zone
/// (sp-ui asks JavaScript's `Date`, which knows the zone's rules).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalTime {
    pub day: u32,
    /// 1..=12.
    pub month: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// `14:05`.
pub fn hm_sk(t: LocalTime) -> String {
    format!("{:02}:{:02}", t.hour, t.minute)
}

/// `12.10.`.
pub fn dm_sk(t: LocalTime) -> String {
    format!("{}.{}.", t.day, t.month)
}

/// `12.10. 14:05`.
pub fn dm_hm_sk(t: LocalTime) -> String {
    format!("{} {}", dm_sk(t), hm_sk(t))
}

/// `12.10. 14:05:07`: an event's time.
pub fn time_sk(t: LocalTime) -> String {
    format!("{}:{:02}", dm_hm_sk(t), t.second)
}

/// How long a clean time since `clean_since_ms` is: `3 d 4 h`, or `viac
/// ako 90 d` when it reaches the oldest instant the ledger keeps.
pub fn clean_span_sk(clean_since_ms: i64, answer: &FaultsAnswer) -> String {
    if clean_since_ms > answer.kept_from_ms {
        duration_sk(answer.now_ms - clean_since_ms)
    } else {
        format!(
            "viac ako {}",
            duration_sk(answer.now_ms - answer.kept_from_ms)
        )
    }
}

/// A path's "clean since" line: `bez chyby od 9.10. 14:05 (3 d 4 h)`
/// (`since` is `clean_since_ms` read in local time; a loss before the asked
/// window counts, a restart does not), or `bez chyby viac ako 90 d` when it
/// reaches the oldest instant the ledger keeps.
pub fn clean_sk(clean_since_ms: i64, answer: &FaultsAnswer, since: LocalTime) -> String {
    if clean_since_ms > answer.kept_from_ms {
        format!(
            "bez chyby od {} ({})",
            dm_hm_sk(since),
            duration_sk(answer.now_ms - clean_since_ms)
        )
    } else {
        format!("bez chyby {}", clean_span_sk(clean_since_ms, answer))
    }
}

/// Seconds as a span, 0 as `0 min`.
fn seconds_sk(s: f64) -> String {
    if s > 0.0 {
        duration_sk((s * 1000.0) as i64)
    } else {
        "0 min".to_string()
    }
}

/// A loop check's line: `skontrolované 59 min · ticho 1 min`.
pub fn loop_sk(l: &LoopSeconds) -> String {
    format!(
        "skontrolované {} · ticho {}",
        seconds_sk(l.checked_s),
        seconds_sk(l.blind_s)
    )
}

/// The order of the panel's rows: the program, the NDI input, each output
/// followed by its loop (by id), the pipelines (by playlist id), then
/// anything else.
pub fn row_order(path: &str) -> (u8, i64, String, u8) {
    if path == "program" {
        (0, 0, String::new(), 0)
    } else if path == "input" {
        (0, 1, String::new(), 0)
    } else if let Some(id) = path.strip_prefix("out:") {
        (1, 0, id.to_string(), 0)
    } else if let Some(id) = path.strip_prefix("loop:") {
        (1, 0, id.to_string(), 1)
    } else if let Some(id) = pipeline_id(path) {
        (2, id, String::new(), 0)
    } else {
        (3, 0, path.to_string(), 0)
    }
}

/// The answer's paths in [`row_order`].
pub fn ordered_paths(answer: &FaultsAnswer) -> Vec<&PathFaults> {
    let mut paths: Vec<&PathFaults> = answer.paths.iter().collect();
    paths.sort_by_key(|p| row_order(&p.path));
    paths
}

/// The event list: every loss and risk row of every path, newest first
/// (rows of one instant in [`row_order`]).
pub fn listed_events(answer: &FaultsAnswer) -> Vec<(&PathFaults, &FaultEvent)> {
    let mut rows: Vec<(&PathFaults, &FaultEvent)> = ordered_paths(answer)
        .into_iter()
        .flat_map(|p| p.events.iter().map(move |e| (p, e)))
        .filter(|(_, e)| e.severity == LOSS || e.severity == RISK)
        .collect();
    rows.sort_by_key(|(_, e)| std::cmp::Reverse(e.ts_ms));
    rows
}

/// What the HealthBar has asked so far (the page's monotonic ms).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Polls {
    /// When the segment's [`SEGMENT_HOURS`] answer was last asked.
    pub day_at: Option<u64>,
    /// The panel's window last asked, and when.
    pub window: Option<(u32, u64)>,
}

/// The next request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// The segment's [`SEGMENT_HOURS`] answer (it also serves the panel at
    /// that window).
    Day,
    /// The panel's window.
    Window(u32),
}

/// The ONE poll's next request, at most one at a time: a window the panel
/// just switched to at once, else whichever answer is [`POLL_EVERY_MS`]
/// old, the segment's first; `None` = nothing is due. `panel` = the open
/// panel's window (its [`SEGMENT_HOURS`] is the segment's own answer).
pub fn next_fetch(polls: Polls, now_ms: u64, panel: Option<u32>) -> Option<Fetch> {
    let due = |at: u64| now_ms.saturating_sub(at) >= POLL_EVERY_MS;
    let window = panel.filter(|h| *h != SEGMENT_HOURS);
    if let Some(h) = window
        && polls.window.is_none_or(|(asked, _)| asked != h)
    {
        return Some(Fetch::Window(h));
    }
    if polls.day_at.is_none_or(due) {
        return Some(Fetch::Day);
    }
    match (window, polls.window) {
        (Some(h), Some((_, at))) if due(at) => Some(Fetch::Window(h)),
        _ => None,
    }
}

/// The poll's memory for an open or a closed panel: a closed panel
/// forgets its window, so it is asked at once when the panel opens again
/// (#257 review; the dashboard drops that window's answer with it, so a
/// reopened panel never shows a stale one).
pub fn polls_for(polls: Polls, open: bool) -> Polls {
    if open {
        polls
    } else {
        Polls {
            window: None,
            ..polls
        }
    }
}

/// The poll's memory when it looks again: [`polls_for`], and a panel
/// OPENED AGAIN since the poll last looked forgets its window too. A close
/// and a reopen inside one of the poll's naps is never seen as closed, so
/// without this the reopened panel would wait out the old window's 10 s
/// while its answer was already dropped (#257, CI 38305196432).
pub fn polls_seen(polls: Polls, open: bool, reopened: bool) -> Polls {
    if reopened {
        Polls {
            window: None,
            ..polls
        }
    } else {
        polls_for(polls, open)
    }
}

/// The failed requests in a row after one more request: an answer ends
/// the run.
pub fn failed_after(failed: u32, answered: bool) -> u32 {
    if answered {
        0
    } else {
        failed.saturating_add(1)
    }
}

/// The segment while the ledger does not answer: after
/// [`MAX_FAILED_POLLS`] failed requests in a row the last answer is no
/// longer shown as current (`None`: the answer's own segment).
pub fn unavailable(failed: u32) -> Option<Segment> {
    (failed >= MAX_FAILED_POLLS).then(|| Segment {
        level: AudioLevel::Unknown,
        text: "Zvuk: nedostupné".to_string(),
        risks: None,
        tip: format!("História chýb zvuku sa nedá načítať ({failed}× po sebe)."),
    })
}

/// The panel's note while the ledger does not answer: how old the answer
/// it shows is (`age_ms` by the page's clock since that answer landed;
/// `None`: no answer landed yet).
pub fn stale_sk(failed: u32, age_ms: Option<u64>) -> Option<String> {
    (failed >= MAX_FAILED_POLLS).then(|| match age_ms {
        Some(age) => format!(
            "Server neodpovedá ({failed}× po sebe) — údaje sú z poslednej odpovede {}.",
            ago_sk(i64::try_from(age).unwrap_or(i64::MAX))
        ),
        None => format!("Server neodpovedá ({failed}× po sebe) — história sa ešte nenačítala."),
    })
}

#[cfg(test)]
#[path = "audio_faults_tests.rs"]
mod tests;
