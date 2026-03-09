use anyhow::Result;

const SERVICE_NAME: &str = "adelie-gtk";

pub struct CredentialStore;

impl CredentialStore {
    fn entry(key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE_NAME, key).map_err(|e| anyhow::anyhow!("keyring error: {e}"))
    }

    pub fn store_password(profile_id: &str, password: &str) -> Result<()> {
        let key = format!("password:{profile_id}");
        let entry = Self::entry(&key)?;
        entry
            .set_password(password)
            .map_err(|e| anyhow::anyhow!("failed to store password: {e}"))
    }

    pub fn get_password(profile_id: &str) -> Result<Option<String>> {
        let key = format!("password:{profile_id}");
        let entry = Self::entry(&key)?;
        match entry.get_password() {
            Ok(pw) => Ok(Some(pw)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("failed to get password: {e}")),
        }
    }

    pub fn store_refresh_token(profile_id: &str, token: &str) -> Result<()> {
        let key = format!("refresh-token:{profile_id}");
        let entry = Self::entry(&key)?;
        entry
            .set_password(token)
            .map_err(|e| anyhow::anyhow!("failed to store refresh token: {e}"))
    }

    pub fn get_refresh_token(profile_id: &str) -> Result<Option<String>> {
        let key = format!("refresh-token:{profile_id}");
        let entry = Self::entry(&key)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("failed to get refresh token: {e}")),
        }
    }

    pub fn delete_credentials(profile_id: &str) -> Result<()> {
        for prefix in &["password", "refresh-token"] {
            let key = format!("{prefix}:{profile_id}");
            if let Ok(entry) = Self::entry(&key) {
                // Ignore NoEntry errors on delete
                let _ = entry.delete_credential();
            }
        }
        Ok(())
    }
}
