//! In-memory [`SecretStore`] used exclusively by tests.
//!
//! This store never touches the real platform keyring, so unit tests stay
//! hermetic. It can also simulate an unavailable or failing backend.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use super::{provider_account, SecretStore, SecretStoreError};

/// Injectable, thread-safe secret store that keeps values in memory.
pub struct MemorySecretStore {
    entries: Mutex<HashMap<String, String>>,
    backend_available: bool,
    fail_backend: bool,
}

impl MemorySecretStore {
    /// A healthy, empty store.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            backend_available: true,
            fail_backend: false,
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
        self.guard()?;
        self.entries
            .lock()
            .unwrap()
            .insert(provider_account(provider_id)?, secret.to_string());
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
