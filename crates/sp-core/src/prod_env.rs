//! #233 (PP's SoundGrid pops, 11.10.2026): the production environment's
//! vocabulary — what can be wrong with a box ([`EnvProblem`]), its stable
//! code and its Slovak text, the status `GET /api/v1/status.environment` and
//! `GET /api/v1/environment` carry ([`EnvironmentStatus`]), and the health
//! bar's segment ([`label`]).
//!
//! The owner's rule: a production PC runs Windows LTSC or Windows IoT
//! Enterprise (his ruling of 11.10.2026), with Windows Defender's real-time
//! protection and every firewall profile OFF. Nothing may scan or filter what
//! SongPlayer launches: on PP every Defender scan of a launched yt-dlp made
//! the Waves SoundGrid driver resync (A/B/A, kernel trace). sp-server's
//! `prod_env` reads the facts and decides; this module only names the outcome
//! (WASM-safe: the dashboard shows it).
//!
//! #255 adds the production PC's audio profile ([`audio`]): the network
//! adapter the SoundGrid driver uses (its chosen port) runs an NDIS driver
//! with Waves' settings, automatic updates are off, MMCSS is set for Pro
//! Audio; the rest is reported only ([`notes_label`]).

use serde::{Deserialize, Serialize};

use crate::health::HealthTone;

#[path = "prod_env_audio.rs"]
pub mod audio;

use audio::{AudioProfile, MmcssSetting, NicSetting};

/// A Windows Firewall profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirewallProfile {
    Domain,
    Private,
    Public,
}

impl FirewallProfile {
    /// Every profile, in the order the status lists them.
    pub const ALL: [FirewallProfile; 3] = [Self::Domain, Self::Private, Self::Public];

    /// The profile's name in a problem code (`firewall_on:<name>`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Private => "private",
            Self::Public => "public",
        }
    }

    /// The profile's name in a Slovak problem text.
    pub fn name_sk(self) -> &'static str {
        match self {
            Self::Domain => "doménový",
            Self::Private => "súkromný",
            Self::Public => "verejný",
        }
    }
}

/// A fact the check could not read: never taken as fine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    Edition,
    /// The build number, needed for an IoT Enterprise edition (Windows 11
    /// only).
    Build,
    Defender,
    /// The antivirus products the Security Center lists.
    Antivirus,
    Firewall(FirewallProfile),
    /// #255: SoundGrid (the service, its driver's chosen port, the
    /// interfaces).
    SoundGrid,
    /// #255: Windows Update's automatic-update policy.
    WindowsUpdate,
    /// #255: MMCSS's system profile.
    Mmcss,
}

/// One way a box is not a production environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvProblem {
    /// The Windows edition (its `EditionID`, and the build when it was read)
    /// is neither an LTSC edition nor Windows 11 IoT Enterprise.
    EditionNotAllowed { edition: String, build: Option<u32> },
    /// Windows Defender's real-time protection is on.
    DefenderRealtimeOn,
    /// Windows Defender runs in a mode that still scans files while another
    /// product leads (`AMRunningMode`: passive, EDR block, side-by-side
    /// passive).
    DefenderPassive(String),
    /// Another antivirus product is on (its name, as the Security Center
    /// lists it).
    AntivirusOn(String),
    /// A firewall profile is on.
    FirewallOn(FirewallProfile),
    /// #255: SoundGrid is installed and its driver has no port chosen (no
    /// preferences, no or an empty `LocalLanPort`).
    SoundGridPortUnset,
    /// #255: no adapter has the MAC of the port the SoundGrid driver chose
    /// (12 hex digits).
    SoundGridPortMissing { mac: String },
    /// #255: the adapter the SoundGrid driver uses runs a NetAdapterCx
    /// driver (named by [`audio::nic_label`]).
    SoundGridNicNetAdapterCx { nic: String },
    /// #255: a SoundGrid adapter's setting is not off: its value as read,
    /// or `None` when a setting every adapter must have is missing.
    SoundGridNicSetting {
        nic: String,
        setting: NicSetting,
        value: Option<String>,
    },
    /// #255: a SoundGrid adapter's driver or a setting could not be read.
    SoundGridNicUnreadable { nic: String },
    /// #255: Windows Update's automatic updates are not off
    /// (`NoAutoUpdate` is not 1).
    AutoUpdateOn,
    /// #255: an MMCSS value is not the production one: its value as read,
    /// or `None` when it is missing.
    Mmcss {
        setting: MmcssSetting,
        value: Option<String>,
    },
    /// A fact could not be read, so the box cannot be verified.
    Unreadable(Unreadable),
}

impl EnvProblem {
    /// The problem's stable code: `edition_not_allowed`,
    /// `defender_realtime_on`, `firewall_on:<profile>`, `unreadable:edition`,
    /// `unreadable:defender`, `unreadable:firewall:<profile>`.
    pub fn code(&self) -> String {
        match self {
            Self::EditionNotAllowed { .. } => "edition_not_allowed".to_string(),
            Self::DefenderRealtimeOn => "defender_realtime_on".to_string(),
            Self::DefenderPassive(_) => "defender_passive_mode".to_string(),
            Self::AntivirusOn(_) => "antivirus_on".to_string(),
            Self::FirewallOn(p) => format!("firewall_on:{}", p.name()),
            Self::SoundGridPortUnset => "soundgrid_port_unset".to_string(),
            Self::SoundGridPortMissing { .. } => "soundgrid_port_missing".to_string(),
            Self::SoundGridNicNetAdapterCx { .. } => "soundgrid_nic_netadaptercx".to_string(),
            Self::SoundGridNicSetting { setting, .. } => {
                format!("soundgrid_nic_setting:{}", setting.name())
            }
            Self::SoundGridNicUnreadable { .. } => "unreadable:soundgrid_nic".to_string(),
            Self::AutoUpdateOn => "windows_auto_update_on".to_string(),
            Self::Mmcss { setting, .. } => format!("mmcss:{}", setting.name()),
            Self::Unreadable(Unreadable::Edition) => "unreadable:edition".to_string(),
            Self::Unreadable(Unreadable::Build) => "unreadable:build".to_string(),
            Self::Unreadable(Unreadable::Defender) => "unreadable:defender".to_string(),
            Self::Unreadable(Unreadable::Antivirus) => "unreadable:antivirus".to_string(),
            Self::Unreadable(Unreadable::Firewall(p)) => {
                format!("unreadable:firewall:{}", p.name())
            }
            Self::Unreadable(Unreadable::SoundGrid) => "unreadable:soundgrid".to_string(),
            Self::Unreadable(Unreadable::WindowsUpdate) => "unreadable:windows_update".to_string(),
            Self::Unreadable(Unreadable::Mmcss) => "unreadable:mmcss".to_string(),
        }
    }

    /// The problem in Slovak, for the dashboard.
    pub fn text_sk(&self) -> String {
        match self {
            Self::EditionNotAllowed { edition, build } => {
                let build = build.map(|b| format!(" (zostava {b})")).unwrap_or_default();
                format!(
                    "edícia Windows „{edition}“{build} nie je povolená (povolené sú len \
                     Windows LTSC a Windows 11 IoT Enterprise)"
                )
            }
            Self::DefenderRealtimeOn => {
                "ochrana Windows Defender v reálnom čase je zapnutá".to_string()
            }
            Self::DefenderPassive(mode) => {
                format!("Windows Defender beží v režime „{mode}“ a súbory stále kontroluje")
            }
            Self::AntivirusOn(name) => format!("antivírus „{name}“ je zapnutý"),
            Self::FirewallOn(p) => format!("brána firewall je zapnutá ({} profil)", p.name_sk()),
            Self::SoundGridPortUnset => {
                "SoundGrid nemá zvolený sieťový port — nastav ho v SoundGrid \
                 Studio / ovládači"
                    .to_string()
            }
            Self::SoundGridPortMissing { mac } => format!(
                "sieťová karta s MAC {}, ktorú má ovládač SoundGrid zvolenú ako port, sa nenašla",
                audio::mac_colons(mac)
            ),
            Self::SoundGridNicNetAdapterCx { nic } => format!(
                "sieťová karta SoundGrid „{nic}“ beží na ovládači NetAdapterCx (musí bežať na \
                 ovládači NDIS)"
            ),
            Self::SoundGridNicSetting {
                nic,
                setting,
                value: Some(value),
            } => format!(
                "sieťová karta SoundGrid „{nic}“: nastavenie „{}“ nie je vypnuté (hodnota {value})",
                setting.name_sk()
            ),
            Self::SoundGridNicSetting {
                nic,
                setting,
                value: None,
            } => format!(
                "sieťová karta SoundGrid „{nic}“: nastavenie „{}“ chýba",
                setting.name_sk()
            ),
            Self::SoundGridNicUnreadable { nic } => format!(
                "ovládač alebo nastavenia sieťovej karty SoundGrid „{nic}“ sa nepodarilo zistiť"
            ),
            Self::AutoUpdateOn => {
                "automatické aktualizácie Windows nie sú vypnuté (NoAutoUpdate)".to_string()
            }
            Self::Mmcss {
                setting,
                value: Some(value),
            } => format!(
                "MMCSS: {} je {value} (má byť {})",
                setting.label(),
                setting.wanted()
            ),
            Self::Mmcss {
                setting,
                value: None,
            } => format!(
                "MMCSS: {} chýba (má byť {})",
                setting.label(),
                setting.wanted()
            ),
            Self::Unreadable(Unreadable::Edition) => {
                "edíciu Windows sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::Build) => {
                "zostavu Windows sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::Defender) => {
                "stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::Antivirus) => {
                "zoznam antivírusových programov sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::Firewall(p)) => format!(
                "stav brány firewall ({} profil) sa nepodarilo zistiť",
                p.name_sk()
            ),
            Self::Unreadable(Unreadable::SoundGrid) => {
                "väzby protokolu SoundGrid na sieťové karty sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::WindowsUpdate) => {
                "nastavenie automatických aktualizácií Windows sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::Mmcss) => {
                "nastavenia MMCSS sa nepodarilo zistiť".to_string()
            }
        }
    }

    /// The problem as the status carries it.
    pub fn view(&self) -> ProblemView {
        ProblemView {
            code: self.code(),
            text_sk: self.text_sk(),
        }
    }
}

/// One problem on the status: its code and its Slovak text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemView {
    pub code: String,
    pub text_sk: String,
}

/// Each firewall profile: on, off, or `None` when it could not be read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallState {
    #[serde(default)]
    pub domain: Option<bool>,
    #[serde(default)]
    pub private: Option<bool>,
    #[serde(default)]
    pub public: Option<bool>,
}

/// The production environment as `GET /api/v1/status.environment` and
/// `GET /api/v1/environment` show it. The default is the state before the
/// first check: not checked, not ok, no problem named.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentStatus {
    /// A check has run (false only right after the start).
    #[serde(default)]
    pub checked: bool,
    /// Checked, and no problem. False before the first check.
    #[serde(default)]
    pub ok: bool,
    /// Why nothing is checked here (off Windows), else none.
    #[serde(default)]
    pub note: Option<String>,
    /// The Windows `EditionID` (`IoTEnterpriseS`, `IoTEnterprise`, …).
    #[serde(default)]
    pub edition_id: Option<String>,
    /// The Windows `ProductName`, for the operator.
    #[serde(default)]
    pub product_name: Option<String>,
    /// The Windows build (`CurrentBuildNumber`, 22000 and up = Windows 11).
    #[serde(default)]
    pub build_number: Option<u32>,
    /// The edition is an LTSC edition.
    #[serde(default)]
    pub ltsc: Option<bool>,
    /// The edition is allowed in production (LTSC, or Windows 11 IoT
    /// Enterprise).
    #[serde(default)]
    pub edition_allowed: Option<bool>,
    /// Windows Defender's real-time protection is on.
    #[serde(default)]
    pub defender_realtime: Option<bool>,
    /// Windows Defender runs in a passive / EDR block mode (still scans).
    #[serde(default)]
    pub defender_passive: Option<bool>,
    /// Windows Defender's running mode (`AMRunningMode`), for the operator.
    #[serde(default)]
    pub defender_mode: Option<String>,
    /// Windows Defender's antivirus engine is on (`AntivirusEnabled`), for
    /// the operator.
    #[serde(default)]
    pub defender_antivirus_enabled: Option<bool>,
    /// The other antivirus products that are on (`[]`: none; `None`: not
    /// read).
    #[serde(default)]
    pub antivirus_on: Option<Vec<String>>,
    /// Each firewall profile.
    #[serde(default)]
    pub firewall: FirewallState,
    /// #255: the production PC's audio profile (the SoundGrid and Dante
    /// adapters, Windows Update, MMCSS, power) and its report-only notes.
    #[serde(default)]
    pub audio: AudioProfile,
    /// Every problem, in a fixed order: the edition, Defender, the firewall's
    /// domain, private and public profile, then the audio profile's.
    #[serde(default)]
    pub problems: Vec<ProblemView>,
    /// Why a fact could not be read (English, for the log and the box).
    #[serde(default)]
    pub read_errors: Vec<String>,
    /// When the last check ran (UTC unix ms).
    #[serde(default)]
    pub checked_at_ms: Option<i64>,
}

/// The health bar's `data-environment` marker of the last poll: `unchecked`
/// (no check yet, or no answer yet: the default status), `ok` (checked,
/// no problem), `violated` (anything else). A spec asserts it before it
/// asserts the red segment's ABSENCE, so a page whose poll has not landed
/// cannot pass for one that holds (review finding 5).
pub fn marker(status: &EnvironmentStatus) -> &'static str {
    if !status.checked {
        "unchecked"
    } else if status.ok && status.problems.is_empty() {
        "ok"
    } else {
        "violated"
    }
}

/// What the health bar's tooltip says before the problems.
pub const RULE_SK: &str = "Produkčný počítač musí bežať na Windows LTSC alebo Windows 11 IoT \
     Enterprise, s vypnutou ochranou Windows Defender v reálnom čase (ani v pasívnom režime), \
     bez iného antivírusu a s vypnutou bránou firewall — nič nesmie kontrolovať ani filtrovať programy, ktoré SongPlayer spúšťa. \
     Sieťová karta, ktorú má ovládač SoundGrid zvolenú ako port, beží na ovládači NDIS s vypnutým moderovaním \
     prerušení, riadením toku a úsporou energie, automatické aktualizácie Windows sú vypnuté a MMCSS je nastavené \
     pre Pro Audio.";

/// The health bar's segment, shown only while a check names a problem:
/// `Prostredie: <the first problem>` (and ` (+N)` for N more), red, with the
/// rule and every problem as the tooltip. Nothing before the first check or
/// while the box holds.
pub fn label(status: &EnvironmentStatus) -> Option<(HealthTone, String, String)> {
    let first = status.problems.first()?;
    let more = status.problems.len() - 1;
    let text = if more == 0 {
        format!("Prostredie: {}", first.text_sk)
    } else {
        format!("Prostredie: {} (+{more})", first.text_sk)
    };
    let found = status
        .problems
        .iter()
        .map(|p| p.text_sk.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let tip = format!("{RULE_SK} Zistené: {found}.");
    Some((HealthTone::Error, text, tip))
}

/// What the notes segment's tooltip says before the notes.
pub const NOTES_SK: &str = "Len hlásené, nie je to chyba (#255):";

/// #255: the health bar's grey segment for the audio profile's report-only
/// facts that are not the production ones (`status.audio.notes`):
/// `Profil PC: <N> poznámky`, the notes and the power scheme as its tooltip.
/// Nothing while there is none. Never a problem: the box still holds.
pub fn notes_label(status: &EnvironmentStatus) -> Option<(HealthTone, String, String)> {
    let notes = &status.audio.notes;
    if notes.is_empty() {
        return None;
    }
    let n = notes.len();
    let text = format!("Profil PC: {n} {}", notes_word(n));
    let mut tip = format!("{NOTES_SK} {}.", notes.join("; "));
    let power = &status.audio.power;
    if let Some(guid) = &power.scheme_guid {
        let name = power.scheme_name.as_deref().unwrap_or("bez názvu");
        tip.push_str(&format!(" Plán napájania: {name} ({guid})."));
    }
    Some((HealthTone::Off, text, tip))
}

/// "poznámka" in Slovak after a count: 1, 2–4, else.
pub fn notes_word(n: usize) -> &'static str {
    match n {
        1 => "poznámka",
        2..=4 => "poznámky",
        _ => "poznámok",
    }
}

#[cfg(test)]
#[path = "prod_env_tests.rs"]
mod tests;
