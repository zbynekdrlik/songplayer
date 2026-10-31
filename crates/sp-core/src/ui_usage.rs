//! #256: the dashboard's usage log. Every node records how its own web UI
//! is used (which control, on which page, while a playlist was on air or
//! not), the measured basis the #248 redesign starts from (design comment
//! 6226648888 on #256). Everything here is pure and shared by the three
//! sides:
//!
//! - **The wire.** A batch `{"events": [{ts_ms, page, control, kind}]}`
//!   ([`UsageBatch`]) is POSTed to [`INGEST_PATH`]. The server takes it
//!   only through [`parse_batch`] (a typed parse, never through
//!   `serde_json::Value`, then [`check_batch`], then every stamp capped at
//!   the server's clock); anything else is refused whole with
//!   [`BATCH_REFUSAL`].
//! - **The client's rules.** sp-ui's `usage.rs` only wires the browser:
//!   whether this browser records at all ([`excluded`]), the name of a
//!   control ([`control_name`]), an event's bounds ([`UsageEvent::new`]),
//!   the batch buffer ([`UsageBuffer`]) and which page views count
//!   ([`PageViews`]: only a page the operator can see).
//! - **The summary.** [`SUMMARY_PATH`]`?days=N` ([`parse_days`]) answers a
//!   [`UsageSummary`]; Nastavenia shows its [`settings_line`].

use serde::{Deserialize, Serialize};

use crate::audio_faults::counted;
use crate::query_param::whole_count;

/// Where the dashboard sends its batches (POST).
pub const INGEST_PATH: &str = "/api/v1/ui-usage";

/// The summary (GET, `?days=N`).
pub const SUMMARY_PATH: &str = "/api/v1/ui-usage/summary";

/// Nastavenia's link: the summary over everything kept.
pub const SUMMARY_LINK: &str = "/api/v1/ui-usage/summary?days=400";

/// At most this many events in one batch: the server refuses more, and the
/// client sends a buffer that reaches it at once.
pub const MAX_EVENTS: usize = 200;

/// A page or a control is at most this many characters (not bytes).
pub const MAX_TEXT_CHARS: usize = 120;

/// How long the log is kept, in days: a yearly action still shows.
pub const RETENTION_DAYS: u32 = 400;

/// The client sends what it recorded this often, when it recorded anything.
pub const FLUSH_EVERY_MS: u32 = 10_000;

/// The localStorage key that turns the log off in one browser (any value):
/// for tools that drive the dashboard and are not Playwright.
pub const NO_USAGE_KEY: &str = "sp_no_usage";

const DAY_MS: i64 = 86_400_000;

/// The 400 text of a batch the server does not take (never the body sent).
pub const BATCH_REFUSAL: &str = "a usage batch is {\"events\": [{\"ts_ms\", \"page\", \"control\", \
     \"kind\"}]}: at most 200 events, ts_ms not negative, page and control at most 120 \
     characters each, kind click, change, submit or page";

/// The 400 text of a `days` that is not 1..=400 (never the value sent).
pub const DAYS_REFUSAL: &str = "days must be a whole number from 1 to 400";

/// Nastavenia's line while the summary loads.
pub const LINE_LOADING: &str = "Záznam používania: načítavam…";

/// Nastavenia's line when the summary could not be read.
pub const LINE_UNAVAILABLE: &str = "Záznam používania: nedostupný";

/// What the operator did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageKind {
    Click,
    Change,
    Submit,
    /// The dashboard showed a page (a route change, or the page a load
    /// opened).
    Page,
}

impl UsageKind {
    /// The wire / DOM event name (`click`, `change`, `submit`, `page`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Change => "change",
            Self::Submit => "submit",
            Self::Page => "page",
        }
    }
}

/// One recorded use of the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// When, by the browser's clock (UTC unix ms).
    pub ts_ms: i64,
    /// The dashboard's page (its router path: `/`, `/live`, `/settings`, …).
    pub page: String,
    /// The control ([`control_name`]); empty for a [`UsageKind::Page`].
    pub control: String,
    pub kind: UsageKind,
}

impl UsageEvent {
    /// An event as the client records it: page and control [`clip`]ped, so
    /// one long id never makes the server refuse the whole batch.
    pub fn new(ts_ms: i64, page: &str, control: &str, kind: UsageKind) -> Self {
        Self {
            ts_ms,
            page: clip(page),
            control: clip(control),
            kind,
        }
    }
}

/// One POST of the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBatch {
    pub events: Vec<UsageEvent>,
}

/// `text`'s first [`MAX_TEXT_CHARS`] characters.
pub fn clip(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

fn too_long(text: &str) -> bool {
    text.chars().count() > MAX_TEXT_CHARS
}

/// Whether the server takes `batch`: at most [`MAX_EVENTS`] events, no
/// stamp before 1970, page and control at most [`MAX_TEXT_CHARS`]
/// characters each (the kind is already one of the four, by its type).
pub fn check_batch(batch: &UsageBatch) -> Result<(), &'static str> {
    if batch.events.len() > MAX_EVENTS {
        return Err(BATCH_REFUSAL);
    }
    if batch
        .events
        .iter()
        .any(|event| event.ts_ms < 0 || too_long(&event.page) || too_long(&event.control))
    {
        return Err(BATCH_REFUSAL);
    }
    Ok(())
}

/// The server's read of a POST body at `now_ms`, whatever its content type
/// (a `navigator.sendBeacon` string arrives as `text/plain`): the typed
/// batch (serde_json skips an unknown field without building a `Value`),
/// checked, each stamp capped at `now_ms`. A use cannot come after its
/// arrival: a browser clock running ahead, or a made-up stamp years ahead,
/// would otherwise sit in every summary window and never be pruned.
pub fn parse_batch(body: &[u8], now_ms: i64) -> Result<UsageBatch, &'static str> {
    let mut batch: UsageBatch = serde_json::from_slice(body).map_err(|_| BATCH_REFUSAL)?;
    check_batch(&batch)?;
    for event in &mut batch.events {
        event.ts_ms = event.ts_ms.min(now_ms);
    }
    Ok(batch)
}

/// Whether this browser records nothing: an automated session
/// (`navigator.webdriver === true`, which every Playwright run sets, the
/// post-deploy specs on the boxes among them) or a browser carrying the
/// [`NO_USAGE_KEY`] localStorage key.
pub fn excluded(webdriver: bool, no_usage_key: Option<&str>) -> bool {
    webdriver || no_usage_key.is_some()
}

/// The control an event names: the nearest `data-testid` (every control
/// carries one), else the element's own tag as `<tag>` (lower case), so a
/// control with no id shows up in the summary as a gap.
pub fn control_name(testid: Option<&str>, tag: &str) -> String {
    match testid.filter(|id| !id.is_empty()) {
        Some(id) => clip(id),
        None => clip(&format!("<{}>", tag.to_ascii_lowercase())),
    }
}

/// The client's events not sent yet.
#[derive(Debug, Default)]
pub struct UsageBuffer {
    events: Vec<UsageEvent>,
}

impl UsageBuffer {
    /// Record `event`. A buffer that reaches [`MAX_EVENTS`] is handed back
    /// whole, to be sent at once (nothing is dropped between two flushes).
    pub fn push(&mut self, event: UsageEvent) -> Option<UsageBatch> {
        self.events.push(event);
        if self.events.len() >= MAX_EVENTS {
            return self.take();
        }
        None
    }

    /// The batch to send now: every event recorded since the last one, or
    /// none when nothing was recorded.
    pub fn take(&mut self) -> Option<UsageBatch> {
        if self.events.is_empty() {
            return None;
        }
        Some(UsageBatch {
            events: std::mem::take(&mut self.events),
        })
    }
}

/// Which page views are recorded: only a page the operator can SEE
/// (visible and focused). SongPlayer's own app window loads the dashboard
/// at every start and stays hidden and unfocused until the tray shows it:
/// counting that load would add a made-up view of `/` per restart (each
/// deploy). A page shown while it cannot be seen owes its view until it
/// can (a `focus` or `visibilitychange`); a click needs no such gate (only
/// a person clicks).
#[derive(Debug, Default)]
pub struct PageViews {
    owed: bool,
}

impl PageViews {
    /// The shown page changed (or the load opened it); `seen`: the
    /// operator can see the page now. Whether to record its view now.
    pub fn changed(&mut self, seen: bool) -> bool {
        self.owed = !seen;
        seen
    }

    /// The page may have come into view; `seen` as above. Whether the view
    /// it owes is recorded now (once).
    pub fn came_into_view(&mut self, seen: bool) -> bool {
        let due = self.owed && seen;
        if due {
            self.owed = false;
        }
        due
    }
}

/// The summary's window: `days` (1..=[`RETENTION_DAYS`], default all of
/// it), by the one whole-count parse (`query_param::whole_count`).
pub fn parse_days(raw: Option<&str>) -> Result<u32, &'static str> {
    whole_count(raw, RETENTION_DAYS, RETENTION_DAYS).ok_or(DAYS_REFUSAL)
}

/// The first instant of a window of `days` ending at `now_ms`. The prune
/// keeps exactly the window of [`RETENTION_DAYS`].
pub fn window_start(now_ms: i64, days: u32) -> i64 {
    now_ms - i64::from(days) * DAY_MS
}

/// One control on one page, one kind, over the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRow {
    pub page: String,
    pub control: String,
    pub kind: String,
    pub count: i64,
    /// Distinct days with a use, by the node's local calendar.
    pub days_used: i64,
    pub first_ms: i64,
    pub last_ms: i64,
    /// The uses while a playlist was on SP-program (not the NDI input
    /// "OBS manuál", not Blank, not before the first selection).
    pub on_air: i64,
}

/// `GET /api/v1/ui-usage/summary?days=N`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub now_ms: i64,
    pub days: u32,
    pub from_ms: i64,
    /// Every event in the window.
    pub events: i64,
    /// The oldest event in the window.
    pub first_ms: Option<i64>,
    /// Most used first.
    pub rows: Vec<UsageRow>,
}

impl UsageSummary {
    /// The summary of `rows` (the window's GROUP BY) at `now_ms`.
    pub fn from_rows(now_ms: i64, days: u32, rows: Vec<UsageRow>) -> Self {
        Self {
            now_ms,
            days,
            from_ms: window_start(now_ms, days),
            events: rows.iter().map(|row| row.count).sum(),
            first_ms: rows.iter().map(|row| row.first_ms).min(),
            rows,
        }
    }
}

/// The days a log whose oldest event is `first_ms` covers at `now_ms`,
/// counted up: an event an hour old is 1 day, 25 h is 2. A first event
/// "after" now (a browser clock ahead) is 1 day.
pub fn span_days(first_ms: i64, now_ms: i64) -> i64 {
    let span = now_ms.saturating_sub(first_ms).max(1).unsigned_abs();
    i64::try_from(span.div_ceil(DAY_MS.unsigned_abs())).unwrap_or(i64::MAX)
}

/// Nastavenia's line: `Záznam používania: 1234 udalostí za 12 dní`, or
/// that nothing was recorded yet.
pub fn settings_line(summary: &UsageSummary) -> String {
    match summary.first_ms {
        None => "Záznam používania: zatiaľ žiadna udalosť".to_string(),
        Some(first) => format!(
            "Záznam používania: {} za {}",
            counted(summary.events, "udalosť", "udalosti", "udalostí"),
            counted(span_days(first, summary.now_ms), "deň", "dni", "dní")
        ),
    }
}

#[cfg(test)]
#[path = "ui_usage_tests.rs"]
mod tests;
