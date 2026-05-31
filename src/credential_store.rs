use anyhow::Result;

const SERVICE_NAME: &str = "adele-gtk";

pub struct CredentialStore;

impl CredentialStore {
    /// Register the system Secret Service as keyring-core's default credential
    /// store. Best-effort: if unavailable (headless / no session bus) we log and
    /// continue — credential calls then surface errors and callers fall back.
    pub fn init_store() {
        run_keyring_blocking(|| match zbus_secret_service_keyring_store::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(error) => tracing::warn!("Secret Service unavailable; keyring disabled: {error}"),
        });
    }

    fn entry(key: &str) -> Result<keyring_core::Entry> {
        keyring_core::Entry::new(SERVICE_NAME, key)
            .map_err(|e| anyhow::anyhow!("keyring error: {e}"))
    }

    pub fn store_password(profile_id: &str, password: &str) -> Result<()> {
        let key = format!("password:{profile_id}");
        run_keyring_blocking(|| {
            let entry = Self::entry(&key)?;
            entry
                .set_password(password)
                .map_err(|e| anyhow::anyhow!("failed to store password: {e}"))
        })
    }

    pub fn get_password(profile_id: &str) -> Result<Option<String>> {
        let key = format!("password:{profile_id}");
        run_keyring_blocking(|| {
            let entry = Self::entry(&key)?;
            match entry.get_password() {
                Ok(pw) => Ok(Some(pw)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(anyhow::anyhow!("failed to get password: {e}")),
            }
        })
    }

    pub fn store_refresh_token(profile_id: &str, token: &str) -> Result<()> {
        let key = format!("refresh-token:{profile_id}");
        run_keyring_blocking(|| {
            let entry = Self::entry(&key)?;
            entry
                .set_password(token)
                .map_err(|e| anyhow::anyhow!("failed to store refresh token: {e}"))
        })
    }

    pub fn get_refresh_token(profile_id: &str) -> Result<Option<String>> {
        let key = format!("refresh-token:{profile_id}");
        run_keyring_blocking(|| {
            let entry = Self::entry(&key)?;
            match entry.get_password() {
                Ok(token) => Ok(Some(token)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(anyhow::anyhow!("failed to get refresh token: {e}")),
            }
        })
    }

    pub fn delete_credentials(profile_id: &str) -> Result<()> {
        run_keyring_blocking(|| {
            for prefix in &["password", "refresh-token"] {
                let key = format!("{prefix}:{profile_id}");
                if let Ok(entry) = Self::entry(&key) {
                    // Ignore NoEntry errors on delete
                    let _ = entry.delete_credential();
                }
            }
        });
        Ok(())
    }
}

/// Run a blocking Secret Service operation without starving the async runtime.
///
/// keyring-core's Secret Service store drives D-Bus over zbus's *blocking*
/// API, which must not run directly on an async worker thread. On a
/// multi-thread runtime we hand the work to `block_in_place`; off a runtime
/// (the GTK main thread) we run inline.
fn run_keyring_blocking<T>(operation: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(operation)
        }
        _ => operation(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // keyring-core's default store is process-global, so register the in-memory
    // mock store once for the whole test binary. Each test uses a unique profile
    // id so the shared store can't cross-contaminate across parallel tests.
    fn with_mock_store() {
        use std::sync::Once;
        static MOCK_STORE: Once = Once::new();
        MOCK_STORE.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    #[test]
    fn password_round_trips() {
        with_mock_store();
        let profile_id = "test-password-roundtrip";
        CredentialStore::store_password(profile_id, "hunter2").unwrap();
        assert_eq!(
            CredentialStore::get_password(profile_id).unwrap(),
            Some("hunter2".to_string())
        );
    }

    #[test]
    fn get_password_returns_none_when_absent() {
        with_mock_store();
        assert_eq!(
            CredentialStore::get_password("test-password-absent").unwrap(),
            None
        );
    }

    #[test]
    fn refresh_token_round_trips() {
        with_mock_store();
        let profile_id = "test-refresh-roundtrip";
        CredentialStore::store_refresh_token(profile_id, "refresh-abc").unwrap();
        assert_eq!(
            CredentialStore::get_refresh_token(profile_id).unwrap(),
            Some("refresh-abc".to_string())
        );
    }
}
