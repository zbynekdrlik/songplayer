use super::ASSEMBLYAI_API_KEY_SETTING;

#[test]
fn settings_key_is_stable() {
    // Locks the exact string. Renaming would silently break operator
    // configs already written into the production SQLite. If a rename
    // is genuinely needed, a DB migration MUST move existing values.
    assert_eq!(ASSEMBLYAI_API_KEY_SETTING, "assemblyai_api_key");
}
