//! In-memory [`SecretStore`] used exclusively by tests.
//!
//! This store never touches the real platform keyring, so unit tests stay
//! hermetic. It can also simulate an unavailable or failing backend.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use super::{normalize_secret, provider_account, SecretStore, SecretStoreError};

/// Injectable, thread-safe secret store that keeps values in memory.
pub struct MemorySecretStore {
    entries: Mutex<HashMap<String, String>>,
    backend_available: bool,
    fail_backend: bool,
    fail_after_writes: Mutex<Option<usize>>,
}

impl MemorySecretStore {
    /// A healthy, empty store.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            backend_available: true,
            fail_backend: false,
            fail_after_writes: Mutex::new(None),
        }
    }

    /// A store whose backend reports as unavailable and rejects operations.
    pub fn unavailable() -> Self {
        Self {
            backend_available: false,
            ..Self::new()
        }
    }

    /// A store whose every read/write operation fails with a backend error.
    pub fn failing() -> Self {
        Self {
            fail_backend: true,
            ..Self::new()
        }
    }

    /// Arm a one-shot write failure after `successful_writes` further writes.
    pub fn set_fail_once_after_writes(&self, successful_writes: usize) {
        *self.fail_after_writes.lock().unwrap() = Some(successful_writes);
    }

    /// Insert a raw value bypassing normalization, to simulate a value that
    /// was persisted before `set_secret` started rejecting blanks (e.g. by an
    /// older build or a legacy migration path).
    #[cfg(test)]
    pub fn seed_raw(&self, provider_id: &str, secret: &str) {
        self.entries
            .lock()
            .unwrap()
            .insert(provider_account(provider_id).unwrap(), secret.to_string());
    }

    fn guard(&self) -> Result<(), SecretStoreError> {
        if !self.backend_available {
            Err(SecretStoreError::BackendUnavailable)
        } else if self.fail_backend {
            Err(SecretStoreError::BackendFailure)
        } else {
            Ok(())
        }
    }
}

impl Default for MemorySecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for MemorySecretStore {
    fn set_secret(&self, provider_id: &str, secret: &str) -> Result<(), SecretStoreError> {
        let secret = normalize_secret(secret)?;
        self.guard()?;
        let mut fail_after = self.fail_after_writes.lock().unwrap();
        if let Some(remaining) = fail_after.as_mut() {
            if *remaining == 0 {
                *fail_after = None;
                return Err(SecretStoreError::BackendFailure);
            }
            *remaining -= 1;
        }
        drop(fail_after);
        self.entries
            .lock()
            .unwrap()
            .insert(provider_account(provider_id)?, secret);
        Ok(())
    }

    fn get_secret(&self, provider_id: &str) -> Result<String, SecretStoreError> {
        self.guard()?;
        self.entries
            .lock()
            .unwrap()
            .get(&provider_account(provider_id)?)
            .cloned()
            .ok_or(SecretStoreError::NotFound)
    }

    fn delete_secret(&self, provider_id: &str) -> Result<(), SecretStoreError> {
        self.guard()?;
        self.entries
            .lock()
            .unwrap()
            .remove(&provider_account(provider_id)?);
        Ok(())
    }

    fn backend_available(&self) -> bool {
        self.backend_available
    }
}

impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render stored secret values: only their count.
        let count = self.entries.lock().map(|e| e.len()).unwrap_or(0);
        f.debug_struct("MemorySecretStore")
            .field("entries", &format_args!("<{count} redacted>"))
            .field("backend_available", &self.backend_available)
            .field("fail_backend", &self.fail_backend)
            .finish()
    }
}
