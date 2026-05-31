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

#[cfg(test)]
mod integration {
    //! Real-Secret-Service integration test. Ignored by default — needs a live
    //! session bus and `secret-tool`; mutates the keyring (namespaced, cleaned
    //! up). Run via `just test-integration`.
    //!
    //! Covers what the mock store cannot: that credentials written under the old
    //! `keyring` v3 attribute scheme stay readable by keyring-core. v3's
    //! secret-service store keyed items by `service`+`username`+`target`+
    //! `application`; keyring-core searches the `service`+`username` subset, and
    //! Secret Service subset-matches — so v3 items must still be found.
    use super::*;
    use std::process::Command;
    use std::sync::Once;

    static REAL_STORE: Once = Once::new();

    fn real_store_ready() -> bool {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            eprintln!("SKIP: no DBUS_SESSION_BUS_ADDRESS");
            return false;
        }
        if !command_present("secret-tool") {
            eprintln!("SKIP: secret-tool not installed");
            return false;
        }
        REAL_STORE.call_once(|| match zbus_secret_service_keyring_store::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(error) => eprintln!("WARN: could not connect to Secret Service: {error}"),
        });
        match keyring_core::Entry::new("adele-gtk-it-probe", "probe").and_then(|e| e.get_password())
        {
            Ok(_) | Err(keyring_core::Error::NoEntry) => true,
            Err(error) => {
                eprintln!("SKIP: Secret Service unavailable: {error}");
                false
            }
        }
    }

    fn command_present(bin: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {bin}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn secret_tool_store(attrs: &[(&str, &str)], value: &str) {
        use std::io::Write as _;
        use std::process::Stdio;
        let mut cmd = Command::new("secret-tool");
        cmd.arg("store")
            .arg("--label")
            .arg("adele-gtk integration test");
        for (key, val) in attrs {
            cmd.arg(key).arg(val);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn secret-tool");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(value.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "secret-tool store failed");
    }

    fn secret_tool_clear(attrs: &[(&str, &str)]) {
        let mut cmd = Command::new("secret-tool");
        cmd.arg("clear");
        for (key, val) in attrs {
            cmd.arg(key).arg(val);
        }
        let _ = cmd.output();
    }

    #[test]
    #[ignore = "needs a real Secret Service; run via `just test-integration`"]
    fn reads_credential_written_by_keyring_v3() {
        if !real_store_ready() {
            return;
        }
        let profile = "it-v3-profile";
        // keyring v3 stored a password under username "password:<profile>" with
        // the full v3 attribute set.
        let username = format!("password:{profile}");
        let attrs = [
            ("service", SERVICE_NAME),
            ("username", username.as_str()),
            ("target", "default"),
            ("application", "rust-keyring"),
        ];
        secret_tool_clear(&attrs);
        secret_tool_store(&attrs, "v3-secret-value");

        // The migrated code must read it via the service+username subset match.
        let got = CredentialStore::get_password(profile);
        assert_eq!(got.unwrap(), Some("v3-secret-value".to_string()));

        // Cleanup.
        let _ = CredentialStore::delete_credentials(profile);
        secret_tool_clear(&attrs);
    }
}
