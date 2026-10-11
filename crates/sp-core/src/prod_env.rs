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

use serde::{Deserialize, Serialize};

use crate::health::HealthTone;

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
    Defender,
    Firewall(FirewallProfile),
}

/// One way a box is not a production environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvProblem {
    /// The Windows edition (its `EditionID`) is neither an LTSC edition nor
    /// IoT Enterprise.
    EditionNotAllowed(String),
    /// Windows Defender's real-time protection is on.
    DefenderRealtimeOn,
    /// A firewall profile is on.
    FirewallOn(FirewallProfile),
    /// A fact could not be read, so the box cannot be verified.
    Unreadable(Unreadable),
}

impl EnvProblem {
    /// The problem's stable code: `edition_not_allowed`,
    /// `defender_realtime_on`, `firewall_on:<profile>`, `unreadable:edition`,
    /// `unreadable:defender`, `unreadable:firewall:<profile>`.
    pub fn code(&self) -> String {
        match self {
            Self::EditionNotAllowed(_) => "edition_not_allowed".to_string(),
            Self::DefenderRealtimeOn => "defender_realtime_on".to_string(),
            Self::FirewallOn(p) => format!("firewall_on:{}", p.name()),
            Self::Unreadable(Unreadable::Edition) => "unreadable:edition".to_string(),
            Self::Unreadable(Unreadable::Defender) => "unreadable:defender".to_string(),
            Self::Unreadable(Unreadable::Firewall(p)) => {
                format!("unreadable:firewall:{}", p.name())
            }
        }
    }

    /// The problem in Slovak, for the dashboard.
    pub fn text_sk(&self) -> String {
        match self {
            Self::EditionNotAllowed(edition) => format!(
                "edícia Windows „{edition}“ nie je povolená (povolené sú len Windows LTSC \
                 a Windows IoT Enterprise)"
            ),
            Self::DefenderRealtimeOn => {
                "ochrana Windows Defender v reálnom čase je zapnutá".to_string()
            }
            Self::FirewallOn(p) => format!("brána firewall je zapnutá ({} profil)", p.name_sk()),
            Self::Unreadable(Unreadable::Edition) => {
                "edíciu Windows sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::Defender) => {
                "stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť".to_string()
            }
            Self::Unreadable(Unreadable::Firewall(p)) => format!(
                "stav brány firewall ({} profil) sa nepodarilo zistiť",
                p.name_sk()
            ),
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
    /// The edition is an LTSC edition.
    #[serde(default)]
    pub ltsc: Option<bool>,
    /// The edition is allowed in production (LTSC or IoT Enterprise).
    #[serde(default)]
    pub edition_allowed: Option<bool>,
    /// Windows Defender's real-time protection is on.
    #[serde(default)]
    pub defender_realtime: Option<bool>,
    /// Each firewall profile.
    #[serde(default)]
    pub firewall: FirewallState,
    /// Every problem, in a fixed order: the edition, Defender, the firewall's
    /// domain, private and public profile.
    #[serde(default)]
    pub problems: Vec<ProblemView>,
    /// Why a fact could not be read (English, for the log and the box).
    #[serde(default)]
    pub read_errors: Vec<String>,
    /// When the last check ran (UTC unix ms).
    #[serde(default)]
    pub checked_at_ms: Option<i64>,
}

/// What the health bar's tooltip says before the problems.
pub const RULE_SK: &str = "Produkčný počítač musí bežať na Windows LTSC alebo Windows IoT \
     Enterprise, s vypnutou ochranou Windows Defender v reálnom čase a vypnutou bránou \
     firewall — nič nesmie kontrolovať ani filtrovať programy, ktoré SongPlayer spúšťa.";

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

#[cfg(test)]
#[path = "prod_env_tests.rs"]
mod tests;
