//! #223 S2: SongPlayer.exe links the vendored Spout2 SDK (BSD 2-Clause), and
//! BSD-2 asks a binary distribution to reproduce the notice in its
//! documentation. So the installer ships `THIRD-PARTY-NOTICES.txt`
//! (`src-tauri/resources`, listed in `tauri.conf.json`'s `bundle.resources`),
//! which must hold `vendor/spout2/LICENSE` verbatim: re-copy it when Spout2
//! is bumped. Runs on every platform (it only reads the repository's files).

const LICENSE: &str = include_str!("../vendor/spout2/LICENSE");
const README: &str = include_str!("../vendor/spout2/README.md");
const NOTICE: &str = include_str!("../../../src-tauri/resources/THIRD-PARTY-NOTICES.txt");
const TAURI_CONF: &str = include_str!("../../../src-tauri/tauri.conf.json");

/// The text with LF line ends (a Windows checkout may give CRLF).
fn lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[test]
fn the_installer_notice_holds_spout2s_license_verbatim() {
    let (license, notice) = (lf(LICENSE), lf(NOTICE));
    assert!(
        license.starts_with("BSD 2-Clause License"),
        "the vendored LICENSE"
    );
    assert!(
        notice.contains(&license),
        "THIRD-PARTY-NOTICES.txt must hold vendor/spout2/LICENSE verbatim"
    );
    let version = "2.007.017";
    assert!(
        README.contains(version) && notice.contains(&format!("Spout2 SDK {version}")),
        "the notice names the vendored version"
    );
}

#[test]
fn the_installer_bundles_the_notice() {
    assert!(
        TAURI_CONF.contains("\"resources/THIRD-PARTY-NOTICES.txt\""),
        "tauri.conf.json bundle.resources lists the notice"
    );
}
