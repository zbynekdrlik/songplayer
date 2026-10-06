//! #229: this node's place in the node exchange, read live from its settings.
//!
//! - `node_name` (`snv`, `pp`): empty = the exchange is off (no serving, no asking).
//! - `peer_api_key`: the key this node's peer API accepts (`X-SP-Peer-Key`);
//!   empty = the peer API answers 404.
//! - `peers`: a JSON list of [`PeerConfig`] — the nodes asked first.
//!
//! The secrets (keys, a Cloudflare Access client secret) never leave the node
//! in clear: `GET /api/v1/settings` (`api::settings`) shows each peer's secrets
//! as [`MASK`] ([`shown_peers`]); a PATCH that sends a masked peer back keeps
//! the stored secret ([`unmask_peers`]); [`checked`] refuses an exchange
//! setting that does not hold; `Debug` prints the mask; no error text echoes
//! a secret or the `peers` text.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEERS};
use sqlx::SqlitePool;

/// What a secret reads as outside the node — `sp_core::config::SECRET_MASK`.
pub const MASK: &str = sp_core::config::SECRET_MASK;
/// The shortest key a node serves with or sends (`openssl rand -hex 32` gives 64).
pub const MIN_KEY_LEN: usize = 32;
/// The longest node or peer name.
pub const MAX_NAME_LEN: usize = 32;

/// One peer this node asks first.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerConfig {
    /// The peer's `node_name` (`snv`).
    pub name: String,
    /// The peer's SongPlayer, e.g. `https://sp.newlevel.media`.
    pub base_url: String,
    /// The peer's `peer_api_key`, sent as `X-SP-Peer-Key`.
    #[serde(default)]
    pub key: String,
    /// The client id of a Cloudflare Access service token, for a peer behind
    /// Access (with `cf_client_secret`: both or neither).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_id: Option<String>,
    /// That service token's client secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_secret: Option<String>,
}

impl PeerConfig {
    /// This peer with its secrets shown as [`MASK`] (an empty one stays empty).
    pub fn masked(&self) -> Self {
        Self {
            key: mask(&self.key).to_string(),
            cf_client_secret: self
                .cf_client_secret
                .as_deref()
                .map(|s| mask(s).to_string()),
            ..self.clone()
        }
    }
}

impl fmt::Debug for PeerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.masked();
        f.debug_struct("PeerConfig")
            .field("name", &m.name)
            .field("base_url", &m.base_url)
            .field("key", &m.key)
            .field("cf_client_id", &m.cf_client_id)
            .field("cf_client_secret", &m.cf_client_secret)
            .finish()
    }
}

/// `""` stays `""`; any other secret reads as [`MASK`].
fn mask(secret: &str) -> &'static str {
    if secret.is_empty() { "" } else { MASK }
}

/// This node's exchange settings, validated.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct NodeConfig {
    pub node_name: Option<String>,
    /// The key this node's peer API accepts; `None` = not serving.
    pub serve_key: Option<String>,
    pub peers: Vec<PeerConfig>,
}

impl fmt::Debug for NodeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeConfig")
            .field("node_name", &self.node_name)
            .field("serve_key", &self.serve_key.as_deref().map(mask))
            .field("peers", &self.peers)
            .finish()
    }
}

impl NodeConfig {
    /// The config the three settings hold, or why they do not hold.
    pub fn from_settings(
        node_name: Option<&str>,
        serve_key: Option<&str>,
        peers: Option<&str>,
    ) -> Result<Self, String> {
        let node_name = match node_name.map(str::trim).filter(|n| !n.is_empty()) {
            None => None,
            Some(n) if valid_name(n) => Some(n.to_string()),
            Some(n) => {
                return Err(format!(
                    "node_name {n:?} must be 1-{MAX_NAME_LEN} of a-z, 0-9 and -"
                ));
            }
        };
        let serve_key = match serve_key.map(str::trim).filter(|k| !k.is_empty()) {
            None => None,
            Some(k) if k.len() >= MIN_KEY_LEN => Some(k.to_string()),
            Some(_) => {
                return Err(format!(
                    "peer_api_key must have at least {MIN_KEY_LEN} characters"
                ));
            }
        };
        let peers = parse_peers(peers)?;
        validate_peers(&peers, node_name.as_deref())?;
        Ok(Self {
            node_name,
            serve_key,
            peers,
        })
    }

    /// The config as the settings table holds it now.
    pub async fn load(pool: &SqlitePool) -> Result<Self, String> {
        let node_name = setting(pool, SETTING_NODE_NAME).await?;
        let serve_key = setting(pool, SETTING_PEER_API_KEY).await?;
        let peers = setting(pool, SETTING_PEERS).await?;
        Self::from_settings(node_name.as_deref(), serve_key.as_deref(), peers.as_deref())
    }

    /// The peer API answers (a name and a key).
    pub fn serving(&self) -> bool {
        self.node_name.is_some() && self.serve_key.is_some()
    }

    /// A heavy job asks peers first (a name and at least one peer).
    pub fn asking(&self) -> bool {
        self.node_name.is_some() && !self.peers.is_empty()
    }

    /// The listed peer named `name`.
    pub fn peer(&self, name: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| p.name == name)
    }
}

async fn setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, String> {
    crate::db::models::get_setting(pool, key)
        .await
        .map_err(|e| format!("reading {key} failed: {e}"))
}

/// The stored peer list; a stored value that does not parse counts as no peer
/// (the PATCH being checked replaces it).
async fn stored_peers(pool: &SqlitePool) -> Result<Vec<PeerConfig>, String> {
    let raw = setting(pool, SETTING_PEERS).await?;
    Ok(parse_peers(raw.as_deref()).unwrap_or_default())
}

/// A node or peer name: 1-32 of `a-z`, `0-9`, `-`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `http(s)://host[/path]`, no query, no fragment, no whitespace, and no
/// `user:password@` before the host (it would show in clear: only the key and
/// the Cloudflare secret are masked).
fn valid_base_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    !host.is_empty()
        && !rest.contains('?')
        && !rest.contains('#')
        && !url.contains(char::is_whitespace)
        && !host.contains('@')
}

/// The peer list's rules; an error names the peer, never a secret.
fn validate_peers(peers: &[PeerConfig], own: Option<&str>) -> Result<(), String> {
    let mut seen = HashSet::new();
    for p in peers {
        if !valid_name(&p.name) {
            return Err(format!(
                "peer name {:?} must be 1-{MAX_NAME_LEN} of a-z, 0-9 and -",
                p.name
            ));
        }
        if own == Some(p.name.as_str()) {
            return Err(format!("peer {} has this node's own name", p.name));
        }
        if !seen.insert(p.name.as_str()) {
            return Err(format!("peer {} is listed twice", p.name));
        }
        if !valid_base_url(&p.base_url) {
            return Err(format!(
                "peer {}: base_url must be http(s)://host[/path]",
                p.name
            ));
        }
        if p.key.len() < MIN_KEY_LEN {
            return Err(format!(
                "peer {}: key must have at least {MIN_KEY_LEN} characters",
                p.name
            ));
        }
        if p.cf_client_id.is_some() != p.cf_client_secret.is_some() {
            return Err(format!(
                "peer {}: cf_client_id and cf_client_secret go together",
                p.name
            ));
        }
    }
    Ok(())
}

/// The `peers` setting as a list; an empty setting is no peer. A parse error
/// names only the position (serde's text can quote the input).
fn parse_peers(raw: Option<&str>) -> Result<Vec<PeerConfig>, String> {
    match raw.map(str::trim).filter(|r| !r.is_empty()) {
        None => Ok(Vec::new()),
        Some(r) => serde_json::from_str(r).map_err(|e| {
            format!(
                "peers is not a peer list (line {}, column {})",
                e.line(),
                e.column()
            )
        }),
    }
}

/// A stored, non-empty `peers` value as `GET /api/v1/settings` shows it: each
/// peer's `key` and `cf_client_secret` as [`MASK`] ([`PeerConfig::masked`]),
/// the rest as stored. A value that does not parse as a peer list reads as
/// [`MASK`] whole: it is never echoed. (The caller shows an empty value as it is.)
pub fn shown_peers(value: &str) -> String {
    match parse_peers(Some(value)) {
        Ok(peers) => {
            let masked: Vec<PeerConfig> = peers.iter().map(PeerConfig::masked).collect();
            serde_json::to_string(&masked).unwrap_or_else(|_| MASK.to_string())
        }
        Err(_) => MASK.to_string(),
    }
}

/// `incoming` with every masked secret taken from the stored peer of the same
/// name, but only where the secret was stored for: a masked `key` needs
/// the stored `base_url`, a masked `cf_client_secret` the stored `base_url`
/// and `cf_client_id`. The settings PATCH has no login, so a peer re-pointed
/// at another host must send its secrets again, in clear; else the node would
/// send the stored ones there. A secret sent in clear is taken as sent; an
/// error names the peer, never a secret or the URL.
pub fn unmask_peers(
    incoming: Vec<PeerConfig>,
    stored: &[PeerConfig],
) -> Result<Vec<PeerConfig>, String> {
    incoming
        .into_iter()
        .map(|mut p| -> Result<PeerConfig, String> {
            let old = stored.iter().find(|s| s.name == p.name);
            if p.key == MASK {
                let o = old.ok_or_else(|| {
                    format!(
                        "peer {}: a masked key, but no stored peer of that name",
                        p.name
                    )
                })?;
                if o.base_url != p.base_url {
                    return Err(format!(
                        "peer {}: base_url changed — send its key again (a masked key stays with its stored base_url)",
                        p.name
                    ));
                }
                p.key = o.key.clone();
            }
            if p.cf_client_secret.as_deref() == Some(MASK) {
                let Some((o, secret)) =
                    old.and_then(|o| o.cf_client_secret.as_ref().map(|s| (o, s)))
                else {
                    return Err(format!(
                        "peer {}: a masked cf_client_secret, but none stored",
                        p.name
                    ));
                };
                let same_token = o.base_url == p.base_url && o.cf_client_id == p.cf_client_id;
                if !same_token {
                    return Err(format!(
                        "peer {}: base_url or cf_client_id changed — send cf_client_secret again",
                        p.name
                    ));
                }
                p.cf_client_secret = Some(secret.clone());
            }
            Ok(p)
        })
        .collect()
}

/// The value a settings PATCH writes for `key`, or why the PATCH is refused
/// (then it writes nothing). `incoming` is the whole PATCH body, so a
/// `node_name` and a `peers` sent together are checked against each other.
///
/// - `peer_api_key`: trimmed; `""` clears it, else at least [`MIN_KEY_LEN`]
///   characters (the error never names the key).
/// - `node_name`: trimmed; `""` clears it, else a valid name, and (when no
///   `peers` is sent along) no stored peer's name.
/// - `peers`: a peer list (an error names only the position); a masked secret
///   takes the stored peer's ([`unmask_peers`]; a stored list that does not
///   parse counts as empty); the list's rules hold against the node name
///   sent, else the stored one.
/// - any other key: as sent.
///
/// A value equal to [`MASK`] for a masked setting keeps the stored one: the
/// caller skips it before asking; this fn does not treat the mask specially.
pub async fn checked(
    pool: &SqlitePool,
    key: &str,
    value: &str,
    incoming: &HashMap<String, String>,
) -> Result<String, String> {
    match key {
        SETTING_PEER_API_KEY => {
            let k = value.trim();
            if !k.is_empty() && k.len() < MIN_KEY_LEN {
                return Err(format!(
                    "peer_api_key must have at least {MIN_KEY_LEN} characters"
                ));
            }
            Ok(k.to_string())
        }
        SETTING_NODE_NAME => {
            let n = value.trim();
            if !n.is_empty() && !valid_name(n) {
                return Err(format!(
                    "node_name {n:?} must be 1-{MAX_NAME_LEN} of a-z, 0-9 and -"
                ));
            }
            if !incoming.contains_key(SETTING_PEERS)
                && stored_peers(pool).await?.iter().any(|p| p.name == n)
            {
                return Err(format!("node_name {n} is a peer's name"));
            }
            Ok(n.to_string())
        }
        SETTING_PEERS => {
            let sent = parse_peers(Some(value))?;
            let merged = unmask_peers(sent, &stored_peers(pool).await?)?;
            let own = match incoming.get(SETTING_NODE_NAME) {
                Some(n) => Some(n.clone()),
                None => setting(pool, SETTING_NODE_NAME).await?,
            };
            let own = own.as_deref().map(str::trim).filter(|n| !n.is_empty());
            validate_peers(&merged, own)?;
            serde_json::to_string(&merged).map_err(|_| "peers could not be written".to_string())
        }
        _ => Ok(value.to_string()),
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
