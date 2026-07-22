//! One-time migration of legacy plaintext credentials into the OS keyring.
//!
//! Older builds persisted `gemini_api_key` and `post_process_api_keys` directly
//! in `settings_store.json`. This module copies every non-empty legacy secret
//! into the [`SecretStore`] under a stable, provider-scoped account so the new
//! provider abstraction reads credentials only from the keyring.
//!
//! **Non-destructive by design in this milestone:** the copy step does not yet
//! remove the values from settings. Destructive removal of the legacy fields is
//! deferred to a later transition version, once every live read path (including
//! the local Gemini transcription branch) sources its key from the keyring.
//! Until then the copy makes the keyring authoritative for the new architecture
//! without breaking the existing pipeline. No secret value is ever logged.

use log::warn;

use super::{provider_account, SecretStore, SecretStoreError};
use crate::settings::AppSettings;

/// Keyring account holding the migrated Gemini key.
pub const GEMINI_ACCOUNT: &str = "gemini";
/// Prefix distinguishing post-processing credentials from ASR provider
/// credentials in the shared keyring service (roadmap invariant #10).
pub const POST_PROCESS_ACCOUNT_PREFIX: &str = "postprocess-";

/// Highest migration step implemented. Persisted as
/// `secret_store_migration_version` once the step completes; bumping it re-runs
/// the (idempotent) copy.
pub const CURRENT_MIGRATION_VERSION: u32 = 1;

/// Result of a migration pass. Contains only account names, never values.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub migrated_accounts: Vec<String>,
}

/// Copy every non-empty legacy secret in `settings` into `store`.
///
/// Idempotent: re-running overwrites with the same value. On the first backend
/// error the pass stops and returns it, leaving already-written accounts in
/// place (the caller must not mark the migration complete). Accounts with an
/// invalid provider id are skipped with a warning rather than aborting.
pub fn copy_legacy_secrets(
    store: &dyn SecretStore,
    settings: &AppSettings,
) -> Result<MigrationReport, SecretStoreError> {
    let mut report = MigrationReport::default();

    if let Some(key) = settings.gemini_api_key.as_deref() {
        if !key.is_empty() {
            store.set_secret(GEMINI_ACCOUNT, key)?;
            report.migrated_accounts.push(GEMINI_ACCOUNT.to_string());
        }
    }

    for (provider_id, key) in &settings.post_process_api_keys {
        if key.is_empty() {
            continue;
        }
        let account = format!("{POST_PROCESS_ACCOUNT_PREFIX}{provider_id}");
        if provider_account(&account).is_err() {
            warn!("Skipping migration of post-process key with unsupported provider id");
            continue;
        }
        store.set_secret(&account, key)?;
        report.migrated_accounts.push(account);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecretStore;
    use crate::settings::get_default_settings;

    fn settings_with_legacy_secrets() -> AppSettings {
        let mut settings = get_default_settings();
        settings.gemini_api_key = Some("gemini-legacy-key".into());
        settings.post_process_api_keys.clear();
        settings
            .post_process_api_keys
            .insert("openai".into(), "openai-legacy-key".into());
        settings
            .post_process_api_keys
            .insert("anthropic".into(), String::new()); // empty -> skipped
        settings
    }

    #[test]
    fn copies_non_empty_secrets_into_keyring() {
        let store = MemorySecretStore::new();
        let settings = settings_with_legacy_secrets();

        let report = copy_legacy_secrets(&store, &settings).unwrap();

        assert_eq!(
            store.get_secret(GEMINI_ACCOUNT).unwrap(),
            "gemini-legacy-key"
        );
        assert_eq!(
            store.get_secret("postprocess-openai").unwrap(),
            "openai-legacy-key"
        );
        assert!(report
            .migrated_accounts
            .contains(&GEMINI_ACCOUNT.to_string()));
        assert!(report
            .migrated_accounts
            .contains(&"postprocess-openai".to_string()));
    }

    #[test]
    fn empty_values_are_not_migrated() {
        let store = MemorySecretStore::new();
        let settings = settings_with_legacy_secrets();
        copy_legacy_secrets(&store, &settings).unwrap();
        // The empty anthropic key must not have been written.
        assert_eq!(
            store.get_secret("postprocess-anthropic"),
            Err(SecretStoreError::NotFound)
        );
    }

    #[test]
    fn no_secrets_migrates_nothing() {
        let store = MemorySecretStore::new();
        let mut settings = get_default_settings();
        settings.gemini_api_key = None;
        settings.post_process_api_keys.clear();
        let report = copy_legacy_secrets(&store, &settings).unwrap();
        assert!(report.migrated_accounts.is_empty());
    }

    #[test]
    fn is_idempotent() {
        let store = MemorySecretStore::new();
        let settings = settings_with_legacy_secrets();
        copy_legacy_secrets(&store, &settings).unwrap();
        let second = copy_legacy_secrets(&store, &settings).unwrap();
        assert_eq!(
            store.get_secret(GEMINI_ACCOUNT).unwrap(),
            "gemini-legacy-key"
        );
        assert!(second
            .migrated_accounts
            .contains(&GEMINI_ACCOUNT.to_string()));
    }

    #[test]
    fn backend_failure_is_propagated_without_leaking_value() {
        let store = MemorySecretStore::failing();
        let settings = settings_with_legacy_secrets();
        let err = copy_legacy_secrets(&store, &settings).unwrap_err();
        assert!(!format!("{err}").contains("gemini-legacy-key"));
    }
}
