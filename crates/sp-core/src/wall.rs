//! #259 (SNV, 13.10.2026): the LED wall report — its vocabulary: the
//! state ([`WallState`]), what keeps the wall from showing Arena
//! ([`WallProblem`]: a stable code and Slovak texts), what was read
//! ([`WallRead`]), the status `GET /api/v1/wall` carries ([`WallStatus`])
//! and the health bar's segment ([`label`], [`marker`]).
//!
//! **The owner's rule (ROZHODNUTÉ 13.10.2026, #259 comment 6142937258):**
//! SongPlayer never enforces a display or Resolume configuration. Whatever
//! the operator sets up — cabling, NVIDIA Surround, Windows' layout,
//! Arena's outputs — is the truth, and it may change at any time. So the
//! report compares the box only with Resolume Arena's OWN saved output
//! setup (the displays its advanced output wants) and says what it sees;
//! nothing acts on the box, and a deliberate change is never an error.
//!
//! #254 (ROZHODNUTÉ 14.10.2026, #254 comment 6169316862): the one exception
//! is the guard ([`WallGuard`]), which ends a crashed or hung Arena so the
//! operator's relaunch script brings it back; it never touches Arena's
//! setup or its clips.
//!
//! #275: the status also carries whether SongPlayer's clip runs in Arena
//! ([`SongplayerClip`], `crate::wall_clip`): a calm amber note on the
//! segment while a song plays on program and none runs, never a fault (the
//! wall's own state is unchanged by it).
//!
//! sp-server's `wall` reads the facts and decides; this module only names
//! the outcome (WASM-safe: the dashboard shows it).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::health::HealthTone;
use crate::wall_clip::SongplayerClip;

/// An NVIDIA display id (NVAPI `displayId`): one per GPU connector. Sent as
/// `0x80061086`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DisplayId(pub u32);

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}", self.0)
    }
}

impl FromStr for DisplayId {
    type Err = String;

    /// `0x` (or `0X`) and 1 to 8 hex digits, nothing else (no sign, no
    /// space).
    fn from_str(s: &str) -> Result<Self, String> {
        let digits = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .filter(|d| (1..=8).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_hexdigit()));
        let Some(digits) = digits else {
            return Err(format!("{s:?} is no display id (0x and 1 to 8 hex digits)"));
        };
        u32::from_str_radix(digits, 16)
            .map(DisplayId)
            .map_err(|e| format!("{s:?}: {e}"))
    }
}

impl Serialize for DisplayId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DisplayId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// One NVIDIA Mosaic (Surround) grid as read: its rows and columns, its
/// displays in row-major order and each display's mode.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grid {
    #[serde(default)]
    pub rows: u32,
    #[serde(default)]
    pub columns: u32,
    #[serde(default)]
    pub displays: Vec<DisplayId>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub hz: u32,
}

/// A display an NVIDIA GPU reports connected (NVAPI `NV_GPU_DISPLAYIDS`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connected {
    #[serde(default)]
    pub id: DisplayId,
    /// The GPU connector: `HDMI`, `DisplayPort`, else `type N`.
    #[serde(default)]
    pub connector: String,
    /// NVAPI's flag word as read (bit 2 active, 4 OS visible, 6 connected,
    /// 17 physically connected): shown only.
    #[serde(default)]
    pub flags: u32,
}

/// A Windows display attached to the desktop (`\\.\DISPLAY1`): its device
/// key (`\Registry\Machine\System\CurrentControlSet\Control\Video\{GUID}\0000`,
/// the id Arena saves for it), its place and its mode, in physical pixels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Display {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub device_key: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub hz: u32,
}

/// A top-level window of Resolume Arena, its rectangle in physical pixels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaWindow {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub left: i32,
    #[serde(default)]
    pub top: i32,
    #[serde(default)]
    pub right: i32,
    #[serde(default)]
    pub bottom: i32,
}

/// A physical display Arena's advanced output wants (an
/// `OutputDeviceDisplay` of its preset): Arena's name for it (`Display 1`),
/// the Windows device key it saved (`deviceId`) and the size it expects.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WantedDisplay {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// A fullscreen output (Arena covers the display with its window
    /// `Display`); a windowed one is never checked for drawing.
    #[serde(default)]
    pub fullscreen: bool,
}

/// Arena's saved output setup: its advanced output on or off, the preset
/// its advanced output was saved as (`AdvancedOutput.xml`'s `presetFile`;
/// Arena's Save & Close writes the setup into that preset file), whether
/// that file was found, and the physical displays it wants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaOutput {
    #[serde(default)]
    pub advanced: bool,
    #[serde(default)]
    pub preset: Option<String>,
    /// The preset file exists and was read (false: no `AdvancedOutput.xml`,
    /// no preset named, a name that is no plain file name, or no such file).
    #[serde(default)]
    pub preset_found: bool,
    #[serde(default)]
    pub displays: Vec<WantedDisplay>,
}

/// How far NVAPI was read. It is read only when the decision needs it (a
/// display Arena wants is gone or has another size), so a healthy wall never
/// asks the driver.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NvidiaRead {
    /// Not needed (or not reached).
    #[default]
    NotRead,
    /// No NVIDIA driver on the box (no `System32\nvapi64.dll`).
    NoDriver,
    /// Asked: `connected` and `grids` hold what each read gave.
    Read,
}

/// What the check read: `None` = could not be read (why: `read_errors`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallRead {
    #[serde(default)]
    pub arena_running: Option<bool>,
    #[serde(default)]
    pub arena_windows: Option<Vec<ArenaWindow>>,
    #[serde(default)]
    pub arena_output: Option<ArenaOutput>,
    #[serde(default)]
    pub displays: Option<Vec<Display>>,
    #[serde(default)]
    pub nvidia: NvidiaRead,
    /// NVAPI's connected displays (`None`: not read, or that read failed).
    #[serde(default)]
    pub connected: Option<Vec<Connected>>,
    /// NVAPI's grids (`None`: not read, or that read failed).
    #[serde(default)]
    pub grids: Option<Vec<Grid>>,
}

/// The wall's state, in the order the check decides it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WallState {
    /// A read the check needs failed: it cannot tell (the post-deploy
    /// gate's only failure: SongPlayer's own readers are broken).
    #[default]
    Unknown,
    /// Resolume Arena is not running.
    ArenaOff,
    /// Arena uses its simple output (no advanced output to compare).
    SimpleOutput,
    /// Arena's advanced output is on but no preset of it is saved (no
    /// `AdvancedOutput.xml`, no preset named, or no such preset file): the
    /// operator's own setup, nothing to compare, never a fault.
    NoPreset,
    /// Arena's advanced output drives no physical display (Spout, NDI,
    /// virtual outputs only).
    NoDisplayOutput,
    /// A display Arena wants is gone and no NVIDIA screen is connected: the
    /// screens are switched off (the church does it at night).
    ScreensOff,
    /// A display Arena wants is gone, or has another size, while screens
    /// are connected.
    DisplayMissing,
    /// Every display Arena wants is there, but Arena's output window does
    /// not cover a fullscreen one.
    ArenaNotDrawing,
    /// Arena draws on every display it wants.
    Ok,
}

impl WallState {
    /// The state's name on the wire (`data-state`, the gate).
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ArenaOff => "arena_off",
            Self::SimpleOutput => "simple_output",
            Self::NoPreset => "no_preset",
            Self::NoDisplayOutput => "no_display_output",
            Self::ScreensOff => "screens_off",
            Self::DisplayMissing => "display_missing",
            Self::ArenaNotDrawing => "arena_not_drawing",
            Self::Ok => "ok",
        }
    }

    /// The state on the health bar, after `Stena: `.
    pub fn text_sk(self) -> &'static str {
        match self {
            Self::Unknown => "nedá sa overiť",
            Self::ArenaOff => "Resolume nebeží",
            Self::SimpleOutput => "Resolume má jednoduchý výstup",
            Self::NoPreset => "Resolume nemá uložený výstup",
            Self::NoDisplayOutput => "Resolume nekreslí na žiadnu obrazovku",
            Self::ScreensOff => "obrazovky vypnuté",
            Self::DisplayMissing => "Resolume nemá svoju obrazovku",
            Self::ArenaNotDrawing => "Resolume nekreslí na stenu",
            Self::Ok => "OK",
        }
    }

    /// The segment's colour: red only while Arena cannot show what it was
    /// set up to show, green when it does, grey otherwise.
    pub fn tone(self) -> HealthTone {
        match self {
            Self::DisplayMissing | Self::ArenaNotDrawing => HealthTone::Error,
            Self::Ok => HealthTone::Ok,
            Self::Unknown
            | Self::ArenaOff
            | Self::SimpleOutput
            | Self::NoPreset
            | Self::NoDisplayOutput
            | Self::ScreensOff => HealthTone::Off,
        }
    }

    /// The health bar shows a segment: not for an Arena that drives no
    /// physical display (nothing to report).
    pub fn shown(self) -> bool {
        !matches!(self, Self::SimpleOutput | Self::NoDisplayOutput)
    }

    /// #275: the segment may name a missing SongPlayer clip instead of this
    /// state's words: over `ok`, `no_preset` and `unknown` (the clip is a
    /// fact of its own), never over a red state, the screens switched off or
    /// Resolume not running (those say more).
    pub fn clip_note_shown(self) -> bool {
        matches!(self, Self::Ok | Self::NoPreset | Self::Unknown)
    }

    /// The wall is fine as the operator set it up, or there is nothing to
    /// tell: every state but `unknown`, `display_missing` and
    /// `arena_not_drawing` (each logged as a WARN).
    pub fn holds(self) -> bool {
        !matches!(
            self,
            Self::Unknown | Self::DisplayMissing | Self::ArenaNotDrawing
        )
    }
}

/// A fact the check reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallFact {
    /// Arena's process and its windows.
    Arena,
    /// Arena's saved output setup (its preferences and preset files).
    ArenaOutput,
    /// Windows' displays.
    Displays,
    /// NVAPI: the connected NVIDIA displays and the Surround grids.
    Nvapi,
}

impl WallFact {
    /// The fact's name in a problem code (`unreadable:<name>`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Arena => "arena",
            Self::ArenaOutput => "arena_output",
            Self::Displays => "displays",
            Self::Nvapi => "nvapi",
        }
    }

    /// The fact in a Slovak problem text.
    pub fn name_sk(self) -> &'static str {
        match self {
            Self::Arena => "proces a okná Resolume Arena",
            Self::ArenaOutput => "uložený výstup Resolume Arena",
            Self::Displays => "zoznam obrazoviek Windows",
            Self::Nvapi => "obrazovky karty NVIDIA (NVAPI)",
        }
    }
}

/// One thing that keeps the wall from showing Arena, or keeps the check
/// from telling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WallProblem {
    /// A read the check needs failed (why: `read_errors`).
    Unreadable(WallFact),
    /// The display Arena wants as `name` is gone, and no NVIDIA screen is
    /// connected.
    ScreensOff {
        name: String,
        width: u32,
        height: u32,
    },
    /// The display Arena wants as `name` is gone while screens are
    /// connected; `not_joined`: NVAPI shows them in no multi-display grid.
    DisplayAbsent {
        name: String,
        width: u32,
        height: u32,
        not_joined: bool,
    },
    /// The display Arena wants as `name` is there with another size.
    DisplaySize {
        name: String,
        width: u32,
        height: u32,
        actual_width: u32,
        actual_height: u32,
        not_joined: bool,
    },
    /// No visible Arena window `Display` covers the fullscreen display
    /// `name` (its rectangle).
    ArenaNotDrawing {
        name: String,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    },
}

/// The detail of a display that is not the one Arena wants while its
/// screens are connected but not joined.
const NOT_JOINED_SK: &str = " — obrazovky sú pripojené, ale nie spojené (Surround)";

/// [`NOT_JOINED_SK`] when `not_joined`, else nothing.
fn not_joined_sk(not_joined: bool) -> &'static str {
    if not_joined { NOT_JOINED_SK } else { "" }
}

impl WallProblem {
    /// The problem's stable code: `unreadable:<fact>`, `screens_off`,
    /// `display_absent`, `display_size`, `arena_not_drawing`.
    pub fn code(&self) -> String {
        match self {
            Self::Unreadable(fact) => format!("unreadable:{}", fact.name()),
            Self::ScreensOff { .. } => "screens_off".to_string(),
            Self::DisplayAbsent { .. } => "display_absent".to_string(),
            Self::DisplaySize { .. } => "display_size".to_string(),
            Self::ArenaNotDrawing { .. } => "arena_not_drawing".to_string(),
        }
    }

    /// The problem in a few words, for the health bar after `Stena: `.
    pub fn short_sk(&self) -> String {
        match self {
            Self::Unreadable(_) => WallState::Unknown.text_sk().to_string(),
            Self::ScreensOff { .. } => WallState::ScreensOff.text_sk().to_string(),
            Self::DisplayAbsent {
                name,
                width,
                height,
                ..
            } => format!("Resolume chce obrazovku {width}×{height} ({name}), Windows ju nemá"),
            Self::DisplaySize {
                name,
                width,
                height,
                actual_width,
                actual_height,
                ..
            } => format!(
                "Resolume chce obrazovku {width}×{height} ({name}), Windows ju má \
                 {actual_width}×{actual_height}"
            ),
            Self::ArenaNotDrawing { name, .. } => format!("Resolume nekreslí na {name}"),
        }
    }

    /// The problem in full, for the tooltip.
    pub fn text_sk(&self) -> String {
        match self {
            Self::Unreadable(fact) => format!("{} sa nepodarilo zistiť", fact.name_sk()),
            Self::ScreensOff {
                name,
                width,
                height,
            } => format!(
                "Resolume chce obrazovku {width}×{height} ({name}); Windows ju nemá a žiadna \
                 obrazovka karty NVIDIA nie je pripojená — obrazovky sú vypnuté"
            ),
            Self::DisplayAbsent { not_joined, .. } | Self::DisplaySize { not_joined, .. } => {
                format!("{}{}", self.short_sk(), not_joined_sk(*not_joined))
            }
            Self::ArenaNotDrawing {
                name,
                left,
                top,
                right,
                bottom,
            } => format!(
                "Resolume Arena nemá na {name} ({left},{top} – {right},{bottom}) viditeľné \
                 výstupné okno „Display“"
            ),
        }
    }

    /// The problem as the status carries it.
    pub fn view(&self) -> WallProblemView {
        WallProblemView {
            code: self.code(),
            short_sk: self.short_sk(),
            text_sk: self.text_sk(),
        }
    }
}

/// One problem on the status: its code, its few words and its full text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallProblemView {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub short_sk: String,
    #[serde(default)]
    pub text_sk: String,
}

/// #254: why the guard ended Resolume Arena.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardWhy {
    /// Arena's crash reporter (BugSplat's `BugSplatMonitor.exe`, Arena's
    /// child) shows its window "Crash Report": Arena crashed and waits on
    /// a click nobody gives on a headless PC.
    #[default]
    CrashReport,
    /// A visible window of Arena did not answer and its CPU time stood still
    /// for 30 s.
    Hung,
}

impl GuardWhy {
    /// The reason's name on the wire and in the log.
    pub fn name(self) -> &'static str {
        match self {
            Self::CrashReport => "crash_report",
            Self::Hung => "hung",
        }
    }

    /// The reason in a few words, for the health bar.
    pub fn short_sk(self) -> &'static str {
        match self {
            Self::CrashReport => "Resolume spadol",
            Self::Hung => "Resolume zamrzol",
        }
    }

    /// The reason in full, for the tooltip.
    pub fn text_sk(self) -> &'static str {
        match self {
            Self::CrashReport => {
                "Resolume Arena spadol a jeho hlásenie chýb (BugSplat) čakalo na klik v okne \
                 „Crash Report“"
            }
            Self::Hung => "Resolume Arena neodpovedal a jeho procesor stál 30 s",
        }
    }
}

/// #254: where the guard stands after its last end of Arena.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardPhase {
    /// No end yet, or Arena runs again since the last one.
    #[default]
    Idle,
    /// Arena was ended less than 60 s ago and no new Arena runs yet: the
    /// operator's relaunch script brings it back.
    Relaunching,
    /// Arena was ended 60 s ago or more and no new Arena runs.
    NotBack,
}

impl GuardPhase {
    /// The phase's name on the wire (`data-guard`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Relaunching => "relaunching",
            Self::NotBack => "not_back",
        }
    }
}

/// #254: one end of Arena by the guard.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardEnd {
    /// When (UTC unix ms).
    #[serde(default)]
    pub at_ms: i64,
    #[serde(default)]
    pub why: GuardWhy,
    /// Arena's process id.
    #[serde(default)]
    pub pid: u32,
    /// The crash reporter's processes, ended before Arena.
    #[serde(default)]
    pub monitors: Vec<u32>,
    /// Why ending a process failed (`None`: every one was ended).
    #[serde(default)]
    pub error: Option<String>,
}

/// #254: the guard on the status: how many times it ended Arena since
/// SongPlayer started, the last end, and whether Arena runs again since.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallGuard {
    #[serde(default)]
    pub ends: u32,
    #[serde(default)]
    pub last: Option<GuardEnd>,
    #[serde(default)]
    pub phase: GuardPhase,
}

impl WallGuard {
    /// The end the health bar names: the last one, until Arena runs again.
    pub fn pending(&self) -> Option<&GuardEnd> {
        if self.phase == GuardPhase::Idle {
            return None;
        }
        self.last.as_ref()
    }

    /// The pending end in a few words, for the health bar after `Stena: `.
    pub fn short_sk(&self) -> Option<String> {
        let end = self.pending()?;
        let what = if end.error.is_some() {
            "nepodarilo sa ho ukončiť"
        } else if self.phase == GuardPhase::NotBack {
            "ukončený, ale znova sa nespustil"
        } else {
            "ukončený, čaká na nové spustenie"
        };
        Some(format!("{} — {what}", end.why.short_sk()))
    }

    /// The pending end in full, for the tooltip.
    pub fn text_sk(&self) -> Option<String> {
        let end = self.pending()?;
        let why = end.why.text_sk();
        let pid = end.pid;
        let mut text = match &end.error {
            Some(error) => format!(
                "SongPlayer chcel ukončiť Resolume Arena (proces {pid}), lebo {why}, ale \
                 nepodarilo sa: {error}."
            ),
            None => format!(
                "SongPlayer ukončil Resolume Arena (proces {pid}), lebo {why}. Znova ho spúšťa \
                 skript operátora; klipy spúšťa len Stream Deck."
            ),
        };
        if self.phase == GuardPhase::NotBack {
            text.push_str(" Do 60 s sa znova nespustil.");
        }
        Some(text)
    }
}

/// The wall as `GET /api/v1/wall` shows it. The default is the state before
/// the first check: not checked.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallStatus {
    /// A check has run (false only right after the start).
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub state: WallState,
    /// What keeps the wall from showing Arena (the displays missing first).
    #[serde(default)]
    pub problems: Vec<WallProblemView>,
    #[serde(default)]
    pub read: WallRead,
    /// Why a fact could not be read (English, for the log and the box).
    #[serde(default)]
    pub read_errors: Vec<String>,
    /// When the last check ran (UTC unix ms).
    #[serde(default)]
    pub checked_at_ms: Option<i64>,
    /// #254: the guard that ends a crashed or hung Arena.
    #[serde(default)]
    pub guard: WallGuard,
    /// #275: whether SongPlayer's clip runs in Arena (the Resolume host
    /// driver's read; `unknown` from an older server).
    #[serde(default)]
    pub songplayer_clip: SongplayerClip,
}

/// The health bar's `data-wall` marker of the last poll: `unchecked` (no
/// check yet, or no answer yet: the default status), else the state's name.
/// A spec asserts it before it asserts the segment's ABSENCE, so a page
/// whose poll has not landed cannot pass for one with nothing to show.
pub fn marker(status: &WallStatus) -> &'static str {
    if !status.checked {
        "unchecked"
    } else {
        status.state.name()
    }
}

/// What the health bar's tooltip says first. #254: the one act is the
/// owner's exception (ROZHODNUTÉ 14.10.2026, #254 comment 6169316862): a
/// crashed or hung Arena is ended so its relaunch script brings it back.
pub const RULE_SK: &str = "SongPlayer porovnáva obrazovky Windows s výstupom, ktorý má Resolume \
     Arena uložený, a len hlási — nič nenastavuje ani nemení. Jediná výnimka: spadnutý alebo \
     zamrznutý Resolume ukončí, aby sa mohol znova spustiť.";

/// `Display 1 7680×1080 · Display 2 1920×1080`.
pub fn wanted_sk(displays: &[WantedDisplay]) -> String {
    let each: Vec<String> = displays
        .iter()
        .map(|d| format!("{} {}×{}", d.name, d.width, d.height))
        .collect();
    each.join(" · ")
}

/// The health bar's segment: `Stena: ` and the first problem in a few words
/// (the state's own words when there is none: a grey state's problems say
/// the same), with the rule, what Arena wants and every problem as its
/// tooltip. #254: while the guard's last end is pending (Arena has not run
/// again since), the segment names that end instead, red, and the tooltip
/// ends with it. #275: while a song plays on program (`song_on_program`,
/// `wall_clip::song_on_program`) and no SongPlayer clip runs, the segment
/// names that, amber, over the states [`WallState::clip_note_shown`] allows;
/// the tooltip says which SongPlayer clips run, or that none does. Nothing
/// before the first check, nor for an Arena that drives no physical display.
pub fn label(status: &WallStatus, song_on_program: bool) -> Option<(HealthTone, String, String)> {
    if !status.checked || !status.state.shown() {
        return None;
    }
    let guard = status.guard.short_sk();
    let clip = &status.songplayer_clip;
    let note = clip_note(status, song_on_program);
    let tone = if guard.is_some() {
        HealthTone::Error
    } else if note {
        HealthTone::Warn
    } else {
        status.state.tone()
    };
    let text = guard
        .or_else(|| note.then(|| clip.short_sk()))
        .or_else(|| status.problems.first().map(|first| first.short_sk.clone()))
        .unwrap_or_else(|| status.state.text_sk().to_string());
    let mut tip = RULE_SK.to_string();
    if let Some(output) = status
        .read
        .arena_output
        .as_ref()
        .filter(|output| !output.displays.is_empty())
    {
        let preset = output.preset.as_deref().unwrap_or("bez názvu");
        tip.push_str(&format!(
            " Resolume (preset „{preset}“) chce: {}.",
            wanted_sk(&output.displays)
        ));
    }
    if !status.problems.is_empty() {
        let found: Vec<&str> = status.problems.iter().map(|p| p.text_sk.as_str()).collect();
        tip.push_str(&format!(" Zistené: {}.", found.join("; ")));
    }
    if let Some(clip) = clip.text_sk() {
        tip.push_str(&format!(" {clip}"));
    }
    if let Some(end) = status.guard.text_sk() {
        tip.push_str(&format!(" {end}"));
    }
    Some((tone, format!("Stena: {text}"), tip))
}

/// #275: the segment names a missing SongPlayer clip (the segment's
/// `data-clip-missing`): a song plays on program, no SongPlayer clip runs,
/// the wall's state lets the note through and no guard's end is pending.
pub fn clip_note(status: &WallStatus, song_on_program: bool) -> bool {
    status.guard.pending().is_none()
        && status.state.clip_note_shown()
        && status.songplayer_clip.missing(song_on_program)
}

#[cfg(test)]
#[path = "wall_tests.rs"]
mod tests;
