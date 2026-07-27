//! One-time migration of legacy plaintext credentials into the OS keyring.
//!
//! Older builds persisted `gemini_api_key` and `post_process_api_keys` directly
//! in `settings_store.json`. This module copies every non-empty legacy secret
//! into the [`SecretStore`] under a stable, provider-scoped account so the new
//! provider abstraction reads credentials only from the keyring.
//!
//! The caller clears the legacy fields only after **all** keyring writes succeed.
//! Every live read path uses the keyring, so there is no plaintext fallback.
//! No secret value is ever logged.

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
pub const CURRENT_MIGRATION_VERSION: u32 = 2;

/// Result of a migration pass. Contains only account names, never values.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub migrated_accounts: Vec<String>,
}

/// Copy every non-empty legacy secret in `settings` into `store`.
///
/// All account names and current keyring values are validated/read before the
/// first write. If a later write fails, every account already changed by this
/// pass is restored to its original value (or deleted when it did not exist).
/// The caller must not clear plaintext or mark the migration complete on any
/// error, including rollback failure.
pub fn copy_legacy_secrets(
    store: &dyn SecretStore,
    settings: &AppSettings,
) -> Result<MigrationReport, SecretStoreError> {
    let mut pending = Vec::<(String, String)>::new();

    if let Some(key) = settings
        .gemini_api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        pending.push((GEMINI_ACCOUNT.to_string(), key.to_string()));
    }

    for (provider_id, key) in &settings.post_process_api_keys {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let account = format!("{POST_PROCESS_ACCOUNT_PREFIX}{provider_id}");
        // Validate every account before any keyring mutation. Unsupported
        // legacy identifiers abort rather than silently destroying data.
        provider_account(&account)?;
        pending.push((account, key.to_string()));
    }

    let mut originals = Vec::with_capacity(pending.len());
    for (account, _) in &pending {
        let original = match store.get_secret(account) {
            Ok(value) => Some(value),
            Err(SecretStoreError::NotFound) => None,
            Err(error) => return Err(error),
        };
        originals.push(original);
    }

    let mut written = 0usize;
    for (account, value) in &pending {
        if let Err(error) = store.set_secret(account, value) {
            let mut rollback_failed = false;
            for index in (0..written).rev() {
                let account = &pending[index].0;
                // `set_secret` now rejects blank/whitespace-only values
                // (`normalize_secret`), but a pre-existing keyring entry
                // written before that guard existed may itself be blank.
                // Restoring such an original via `set_secret` would fail and
                // strand the newly migrated value in place despite the
                // overall rollback being reported as failed. A blank
                // original carries no real secret (see `is_configured`'s
                // same treatment), so deleting it reproduces its practical
                // pre-migration state without going through the rejected
                // write path.
                let rollback = match &originals[index] {
                    Some(original) if !original.trim().is_empty() => {
                        store.set_secret(account, original)
                    }
                    Some(_) | None => store.delete_secret(account),
                };
                rollback_failed |= rollback.is_err();
            }
            return Err(if rollback_failed {
                SecretStoreError::BackendFailure
            } else {
                error
            });
        }
        written += 1;
    }

    Ok(MigrationReport {
        migrated_accounts: pending.into_iter().map(|(account, _)| account).collect(),
    })
}

/// Remove all credential values from settings after a successful copy pass.
/// Provider ids remain so existing settings UI layout is preserved.
pub fn clear_legacy_plaintext(settings: &mut AppSettings) {
    settings.gemini_api_key = None;
    for value in settings.post_process_api_keys.values_mut() {
        value.clear();
    }
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

    #[test]
    fn partial_failure_restores_preexisting_values_and_deletes_new_entries() {
        let store = MemorySecretStore::new();
        store
            .set_secret(GEMINI_ACCOUNT, "preexisting-gemini")
            .unwrap();
        let mut settings = settings_with_legacy_secrets();
        settings
            .post_process_api_keys
            .insert("mistral".into(), "mistral-legacy-key".into());
        store.set_fail_once_after_writes(2);

        assert!(copy_legacy_secrets(&store, &settings).is_err());
        assert_eq!(
            store.get_secret(GEMINI_ACCOUNT).unwrap(),
            "preexisting-gemini"
        );
        assert_eq!(
            store.get_secret("postprocess-openai"),
            Err(SecretStoreError::NotFound)
        );
        assert_eq!(
            store.get_secret("postprocess-mistral"),
            Err(SecretStoreError::NotFound)
        );
    }

    #[test]
    fn partial_failure_restores_a_blank_legacy_original_by_deleting_it() {
        // A pre-existing keyring entry written before `set_secret` started
        // rejecting blanks (e.g. an older build). The rollback must not call
        // `set_secret` with this raw value -- that would itself fail now --
        // and must not report success while leaving the newly migrated value
        // in place.
        //
        // Note: the injected write failure below is itself reported as
        // `SecretStoreError::BackendFailure` (see `MemorySecretStore`), the
        // same variant `copy_legacy_secrets` also returns when a *rollback*
        // fails -- so the top-level `Err` variant can't distinguish "the
        // primary write failed as intended" from "rollback also failed" in
        // this harness. The real, unambiguous signal is the store's final
        // state, which is what this test actually asserts on.
        let store = MemorySecretStore::new();
        store.seed_raw(GEMINI_ACCOUNT, "   ");
        let mut settings = settings_with_legacy_secrets();
        settings
            .post_process_api_keys
            .insert("mistral".into(), "mistral-legacy-key".into());
        // Fail after the Gemini write (index 0) succeeds, so its rollback
        // must restore the blank original.
        store.set_fail_once_after_writes(1);

        assert!(copy_legacy_secrets(&store, &settings).is_err());
        // The blank original is restored by deletion: no secret configured,
        // and critically not the newly migrated legacy key left stranded.
        assert_eq!(
            store.get_secret(GEMINI_ACCOUNT),
            Err(SecretStoreError::NotFound)
        );
    }

    #[test]
    fn invalid_account_aborts_before_any_write() {
        let store = MemorySecretStore::new();
        let mut settings = settings_with_legacy_secrets();
        settings
            .post_process_api_keys
            .insert("unsupported/provider".into(), "must-not-be-lost".into());

        assert_eq!(
            copy_legacy_secrets(&store, &settings),
            Err(SecretStoreError::InvalidProviderId)
        );
        assert_eq!(
            store.get_secret(GEMINI_ACCOUNT),
            Err(SecretStoreError::NotFound)
        );
    }

    #[test]
    fn plaintext_is_cleared_only_by_explicit_success_step() {
        let mut settings = settings_with_legacy_secrets();
        clear_legacy_plaintext(&mut settings);
        assert!(settings.gemini_api_key.is_none());
        assert!(settings
            .post_process_api_keys
            .values()
            .all(String::is_empty));
    }
}
