//! #229 `peer::config`: validation, the masking of the peer secrets, the
//! redacting Debug and the checks a settings PATCH runs.
//! (Repo convention: names the parent only imports are imported explicitly.)

use std::collections::HashMap;

use sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEERS};
use sqlx::SqlitePool;

use super::*;

/// Exactly `MIN_KEY_LEN` (32) characters.
const KEY: &str = "example-peer-key-for-the-tests-1";
/// A Cloudflare Access client secret.
const CF_SECRET: &str = "cf-secret-example";

fn peer(name: &str) -> PeerConfig {
    PeerConfig {
        name: name.into(),
        base_url: "https://sp.newlevel.media".into(),
        key: KEY.into(),
        cf_client_id: Some("client-id.access".into()),
        cf_client_secret: Some(CF_SECRET.into()),
    }
}

fn list(peers: &[PeerConfig]) -> String {
    serde_json::to_string(peers).unwrap()
}

/// A migrated in-memory database (its own per test, so no test lock).
async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

async fn store(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value)
        .await
        .unwrap();
}

/// A PATCH body: the settings sent.
fn sent(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn no_settings_is_an_exchange_that_is_off() {
    let c = NodeConfig::from_settings(None, None, None).unwrap();
    assert_eq!(c, NodeConfig::default());
    assert!(!c.serving());
    assert!(!c.asking());
}

#[test]
fn a_named_node_with_its_key_serves_and_with_peers_asks() {
    let c =
        NodeConfig::from_settings(Some(" pp "), Some(KEY), Some(&list(&[peer("snv")]))).unwrap();
    assert_eq!(c.node_name.as_deref(), Some("pp"));
    assert!(
        c.serve_key.as_deref() == Some(KEY),
        "the serving key as stored"
    );
    assert!(c.serving());
    assert!(c.asking());
    assert_eq!(c.peers, vec![peer("snv")]);
    assert_eq!(c.peer("snv"), Some(&peer("snv")));
    assert_eq!(c.peer("xx"), None);
}

#[test]
fn without_a_node_name_nothing_serves_or_asks() {
    let c = NodeConfig::from_settings(Some(""), Some(KEY), Some(&list(&[peer("snv")]))).unwrap();
    assert!(!c.serving());
    assert!(!c.asking());
}

#[test]
fn a_node_with_a_name_and_no_peers_does_not_ask() {
    let c = NodeConfig::from_settings(Some("snv"), None, Some("[]")).unwrap();
    assert!(!c.asking());
    assert!(!c.serving());
}

#[test]
fn a_name_is_1_to_32_of_lowercase_digits_and_dashes() {
    assert!(valid_name("pp"));
    assert!(valid_name("snv-2"));
    assert!(valid_name(&"a".repeat(MAX_NAME_LEN)));
    assert!(!valid_name(""));
    assert!(!valid_name(&"a".repeat(MAX_NAME_LEN + 1)));
    assert!(!valid_name("PP"));
    assert!(!valid_name("p p"));
    assert!(!valid_name("p_p"));
    assert!(!valid_name("../x"));
    assert!(NodeConfig::from_settings(Some("PP"), None, None).is_err());
}

#[test]
fn a_serving_key_has_at_least_32_characters() {
    let short = &KEY[..MIN_KEY_LEN - 1];
    let err = NodeConfig::from_settings(Some("snv"), Some(short), None).unwrap_err();
    assert!(!err.contains(short), "the key never appears in an error");
    assert!(
        NodeConfig::from_settings(Some("snv"), Some(KEY), None)
            .unwrap()
            .serving()
    );
    let long = format!("{KEY}-and-longer");
    assert!(
        NodeConfig::from_settings(Some("snv"), Some(&long), None)
            .unwrap()
            .serving()
    );
}

#[test]
fn a_base_url_is_http_or_https_with_a_host() {
    assert!(valid_base_url("https://sp.newlevel.media"));
    assert!(valid_base_url("http://10.77.9.201:8920"));
    assert!(valid_base_url("https://host/sub/"));
    assert!(!valid_base_url("ftp://host"));
    assert!(!valid_base_url("https://"));
    assert!(!valid_base_url("https:///path"));
    assert!(!valid_base_url("https://host?x=1"));
    assert!(!valid_base_url("https://host#f"));
    assert!(!valid_base_url("https://ho st"));
}

/// A `user:password@` in a peer's base_url would show in clear in the GET's
/// `peers` and in `Debug` (only the key and the Cloudflare secret are masked).
#[test]
fn a_base_url_never_carries_a_password() {
    assert!(!valid_base_url("https://u:p@host"));
    assert!(!valid_base_url("http://user@10.77.9.201:8920"));
    assert!(valid_base_url("https://sp.newlevel.media"));
}

#[test]
fn every_peer_list_rule_is_enforced() {
    let own = Some("pp");
    let bad_name = PeerConfig {
        name: "SNV".into(),
        ..peer("snv")
    };
    let bad_url = PeerConfig {
        base_url: "sp.newlevel.media".into(),
        ..peer("snv")
    };
    let short_key = PeerConfig {
        key: KEY[..MIN_KEY_LEN - 1].to_string(),
        ..peer("snv")
    };
    let half_cf = PeerConfig {
        cf_client_secret: None,
        ..peer("snv")
    };
    let other_half_cf = PeerConfig {
        cf_client_id: None,
        ..peer("snv")
    };
    assert!(validate_peers(&[bad_name], own).is_err());
    assert!(
        validate_peers(&[peer("pp")], own).is_err(),
        "a peer named like this node"
    );
    assert!(
        validate_peers(&[peer("snv"), peer("snv")], own).is_err(),
        "a peer twice"
    );
    assert!(validate_peers(&[bad_url], own).is_err());
    assert!(validate_peers(&[short_key], own).is_err());
    assert!(validate_peers(&[half_cf], own).is_err());
    assert!(validate_peers(&[other_half_cf], own).is_err());
    let no_cf = PeerConfig {
        cf_client_id: None,
        cf_client_secret: None,
        ..peer("snv")
    };
    assert!(
        validate_peers(&[peer("snv"), no_cf], own).is_err(),
        "still twice"
    );
    let rig = PeerConfig {
        cf_client_id: None,
        cf_client_secret: None,
        ..peer("rig")
    };
    assert!(
        validate_peers(&[peer("snv"), rig], own).is_ok(),
        "no Cloudflare token at all"
    );
    assert!(validate_peers(&[peer("snv")], own).is_ok());
    assert!(validate_peers(&[peer("snv")], None).is_ok());
    assert!(validate_peers(&[], own).is_ok());
    assert!(
        NodeConfig::from_settings(Some("snv"), None, Some(&list(&[peer("snv")]))).is_err(),
        "the stored list is checked against the node's own name"
    );
}

#[test]
fn a_peer_list_error_never_echoes_the_setting() {
    let raw = "\"secret-value-that-must-not-leak\"";
    let err = NodeConfig::from_settings(Some("pp"), None, Some(raw)).unwrap_err();
    assert!(
        !err.contains("secret-value"),
        "the error echoes the setting"
    );
    assert!(err.contains("line 1"), "the error names no position");
}

#[test]
fn debug_never_prints_a_secret() {
    let d = format!("{:?}", peer("snv"));
    assert!(!d.contains(KEY), "PeerConfig Debug shows the key");
    assert!(
        !d.contains(CF_SECRET),
        "PeerConfig Debug shows the cf secret"
    );
    assert!(
        d.contains("client-id.access"),
        "the client id is not a secret"
    );
    assert!(d.contains(MASK), "PeerConfig Debug shows no mask");
    let c = NodeConfig::from_settings(Some("snv"), Some(KEY), None).unwrap();
    let d = format!("{c:?}");
    assert!(!d.contains(KEY), "NodeConfig Debug shows the key");
    assert!(
        d.contains("snv") && d.contains(MASK),
        "the name shown, the key masked"
    );
}

#[test]
fn shown_peers_masks_each_peers_secrets() {
    let rig = PeerConfig {
        cf_client_id: None,
        cf_client_secret: None,
        ..peer("rig")
    };
    let keyless = PeerConfig {
        key: String::new(),
        ..peer("old")
    };
    let shown = shown_peers(&list(&[peer("snv"), rig.clone(), keyless]));
    assert!(!shown.contains(KEY), "shown_peers shows a key");
    assert!(!shown.contains(CF_SECRET), "shown_peers shows a cf secret");
    let back: Vec<PeerConfig> = serde_json::from_str(&shown).unwrap();
    assert_eq!(back.len(), 3);
    assert_eq!(back[0].name, "snv");
    assert!(back[0].key == MASK, "snv's key reads as the mask");
    assert!(
        back[0].cf_client_secret.as_deref() == Some(MASK),
        "snv's cf secret reads as the mask"
    );
    assert_eq!(back[0].cf_client_id.as_deref(), Some("client-id.access"));
    assert_eq!(back[0].base_url, "https://sp.newlevel.media");
    assert!(back[1].key == MASK, "rig's key reads as the mask");
    assert_eq!(back[1].cf_client_secret, None);
    assert_eq!(back[2].key, "", "an empty key stays empty");
    let rig_alone = shown_peers(&list(&[rig]));
    assert!(
        !rig_alone.contains("cf_client"),
        "no Cloudflare token stays absent"
    );
    assert_eq!(
        shown_peers("not json"),
        MASK,
        "a list that does not parse is never echoed"
    );
}

#[test]
fn unmask_takes_the_stored_secret_of_the_same_peer() {
    let stored = vec![peer("snv")];
    let kept = unmask_peers(vec![peer("snv").masked()], &stored).unwrap();
    assert_eq!(
        kept,
        vec![peer("snv")],
        "the same base_url and cf_client_id: the stored key and cf secret"
    );

    // A masked secret stays with the base_url it was stored for. The
    // PATCH has no login, so a peer re-pointed at another host must not take
    // the stored key and Cloudflare token there.
    let moved = PeerConfig {
        base_url: "https://other.example".into(),
        ..peer("snv").masked()
    };
    let err = unmask_peers(vec![moved], &stored).unwrap_err();
    assert!(
        err.contains("peer snv") && err.contains("send its key again"),
        "{err}"
    );
    assert!(!err.contains("other.example"), "never the URL: {err}");
    let moved_with_its_key = PeerConfig {
        base_url: "https://other.example".into(),
        key: KEY.into(),
        ..peer("snv").masked()
    };
    let err = unmask_peers(vec![moved_with_its_key], &stored).unwrap_err();
    assert!(
        err.contains("peer snv") && err.contains("send cf_client_secret again"),
        "the masked cf secret stays with the stored base_url too: {err}"
    );
    let moved_in_clear = PeerConfig {
        base_url: "https://other.example".into(),
        ..peer("snv")
    };
    assert_eq!(
        unmask_peers(vec![moved_in_clear.clone()], &stored),
        Ok(vec![moved_in_clear]),
        "secrets sent in clear are taken as sent"
    );

    assert!(
        unmask_peers(vec![peer("pp").masked()], &stored).is_err(),
        "no stored peer of that name"
    );
    let no_cf = vec![PeerConfig {
        cf_client_id: None,
        cf_client_secret: None,
        ..peer("snv")
    }];
    assert!(
        unmask_peers(vec![peer("snv").masked()], &no_cf).is_err(),
        "no stored cf secret"
    );
    let fresh = unmask_peers(vec![peer("snv")], &[]).unwrap();
    assert!(fresh[0].key == KEY, "a key sent in clear is taken as sent");
}

/// A masked Cloudflare secret also stays with its `cf_client_id`; a new
/// service token is sent whole.
#[test]
fn a_masked_cf_secret_stays_with_its_client_id() {
    let stored = vec![peer("snv")];
    let new_id = PeerConfig {
        cf_client_id: Some("other-id.access".into()),
        ..peer("snv").masked()
    };
    let err = unmask_peers(vec![new_id], &stored).unwrap_err();
    assert!(
        err.contains("peer snv") && err.contains("send cf_client_secret again"),
        "{err}"
    );
    assert!(!err.contains("other-id"), "never the client id: {err}");
    let new_token = PeerConfig {
        cf_client_id: Some("other-id.access".into()),
        cf_client_secret: Some("cf-other-secret-example".into()),
        ..peer("snv").masked()
    };
    let merged = unmask_peers(vec![new_token], &stored).unwrap();
    let want = PeerConfig {
        cf_client_id: Some("other-id.access".into()),
        cf_client_secret: Some("cf-other-secret-example".into()),
        ..peer("snv")
    };
    assert_eq!(
        merged,
        vec![want],
        "the stored key (the same base_url), the new token as sent"
    );
}

#[tokio::test]
async fn load_reads_the_node_config_from_the_settings_table() {
    let pool = pool().await;
    assert_eq!(
        NodeConfig::load(&pool).await.unwrap(),
        NodeConfig::default(),
        "no row: off"
    );
    store(&pool, SETTING_NODE_NAME, "pp").await;
    store(&pool, SETTING_PEER_API_KEY, KEY).await;
    store(&pool, SETTING_PEERS, &list(&[peer("snv")])).await;
    let want = NodeConfig {
        node_name: Some("pp".into()),
        serve_key: Some(KEY.into()),
        peers: vec![peer("snv")],
    };
    assert_eq!(NodeConfig::load(&pool).await.unwrap(), want);
}

#[tokio::test]
async fn checked_peer_api_key_is_32_or_more_characters_or_empty() {
    let pool = pool().await;
    let none: HashMap<String, String> = HashMap::new();
    let short = &KEY[..MIN_KEY_LEN - 1];
    let err = match checked(&pool, SETTING_PEER_API_KEY, short, &none).await {
        Ok(_) => panic!("a 31-character peer_api_key was accepted"),
        Err(e) => e,
    };
    assert!(!err.contains(short), "the key never appears in an error");
    let exact = checked(&pool, SETTING_PEER_API_KEY, KEY, &none).await;
    assert!(
        exact == Ok(KEY.to_string()),
        "a 32-character key is taken as sent"
    );
    let padded = format!(" {KEY} ");
    let trimmed = checked(&pool, SETTING_PEER_API_KEY, &padded, &none).await;
    assert!(trimmed == Ok(KEY.to_string()), "the key is trimmed");
    let cleared = checked(&pool, SETTING_PEER_API_KEY, "", &none).await;
    assert_eq!(cleared, Ok(String::new()));
}

#[tokio::test]
async fn checked_node_name_is_valid_and_not_a_stored_peers_name() {
    let pool = pool().await;
    let none: HashMap<String, String> = HashMap::new();
    assert!(
        checked(&pool, SETTING_NODE_NAME, "PP", &none)
            .await
            .is_err()
    );
    let trimmed = checked(&pool, SETTING_NODE_NAME, " pp ", &none).await;
    assert_eq!(trimmed, Ok("pp".to_string()));
    store(&pool, SETTING_PEERS, &list(&[peer("snv")])).await;
    let err = checked(&pool, SETTING_NODE_NAME, "snv", &none)
        .await
        .unwrap_err();
    assert!(err.contains("snv"), "{err}");
    let other = checked(&pool, SETTING_NODE_NAME, "pp", &none).await;
    assert_eq!(other, Ok("pp".to_string()), "a name no stored peer has");
    let with_peers = sent(&[(SETTING_NODE_NAME, "snv"), (SETTING_PEERS, "[]")]);
    let renamed = checked(&pool, SETTING_NODE_NAME, "snv", &with_peers).await;
    assert_eq!(
        renamed,
        Ok("snv".to_string()),
        "the peers sent along are checked on their own key"
    );
    let cleared = checked(&pool, SETTING_NODE_NAME, "", &none).await;
    assert_eq!(cleared, Ok(String::new()));
}

#[tokio::test]
async fn checked_peers_unmasks_from_the_stored_list_and_validates() {
    let pool = pool().await;
    let none: HashMap<String, String> = HashMap::new();
    store(&pool, SETTING_PEERS, &list(&[peer("snv")])).await;
    let same = list(&[peer("snv").masked()]);
    let written = checked(&pool, SETTING_PEERS, &same, &none).await.unwrap();
    let back: Vec<PeerConfig> = serde_json::from_str(&written).unwrap();
    assert_eq!(
        back,
        vec![peer("snv")],
        "the same base_url and cf_client_id: the stored key + cf secret"
    );
    let moved = PeerConfig {
        base_url: "https://snv.example".into(),
        ..peer("snv").masked()
    };
    let err = match checked(&pool, SETTING_PEERS, &list(&[moved]), &none).await {
        Ok(_) => panic!("a masked key was taken to a changed base_url"),
        Err(e) => e,
    };
    assert!(
        err.contains("send its key again"),
        "a masked key stays with its stored base_url: {err}"
    );
    assert!(!err.contains("snv.example"), "never the URL: {err}");

    let stranger = list(&[peer("rig").masked()]);
    let err = checked(&pool, SETTING_PEERS, &stranger, &none).await;
    assert!(err.is_err(), "a masked peer no stored peer has");

    let snv = list(&[peer("snv")]);
    let named_snv = sent(&[(SETTING_NODE_NAME, "snv"), (SETTING_PEERS, &snv)]);
    let err = checked(&pool, SETTING_PEERS, &snv, &named_snv).await;
    assert!(err.is_err(), "a peer named like the node name sent");
    store(&pool, SETTING_NODE_NAME, "snv").await;
    let err = checked(&pool, SETTING_PEERS, &snv, &none).await;
    assert!(err.is_err(), "a peer named like the stored node name");
    let renamed = sent(&[(SETTING_NODE_NAME, " pp ")]);
    let ok = checked(&pool, SETTING_PEERS, &snv, &renamed).await;
    assert!(
        ok.is_ok(),
        "the node name sent wins over the stored one: {ok:?}"
    );

    let raw = "[{\"name\":\"SNV\"}]";
    let err = checked(&pool, SETTING_PEERS, raw, &none).await.unwrap_err();
    assert!(!err.contains("SNV"), "{err}");
    assert!(err.contains(SETTING_PEERS), "{err}");

    store(&pool, SETTING_PEERS, "not json").await;
    let rig = list(&[peer("rig")]);
    let taken = checked(&pool, SETTING_PEERS, &rig, &none).await;
    assert!(
        taken == Ok(rig),
        "a stored list that does not parse counts as empty"
    );
}

#[tokio::test]
async fn checked_passes_every_other_key_as_sent() {
    let pool = pool().await;
    let none: HashMap<String, String> = HashMap::new();
    let plain = checked(&pool, "gemini_model", " m ", &none).await;
    assert_eq!(plain, Ok(" m ".to_string()));
    let masked = checked(&pool, "gemini_api_key", MASK, &none).await;
    assert_eq!(masked, Ok(MASK.to_string()), "masking is the caller's job");
}
