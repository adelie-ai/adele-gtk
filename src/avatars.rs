use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

const ADELE_AVATAR_BYTES: &[u8] = include_bytes!("../assets/adele_avatar_64.png");

/// Return a base64 data URI for the Adele avatar (pre-sized 64px).
pub fn adele_avatar_data_uri() -> String {
    let encoded = BASE64.encode(ADELE_AVATAR_BYTES);
    format!("data:image/png;base64,{encoded}")
}

/// Resolve the current user's profile picture using the same fallback chain
/// as the KDE widgets:
///   1. AccountsService icon: `/var/lib/AccountsService/icons/{username}`
///   2. `~/.face.icon`
///   3. `~/.face`
///
/// Returns a base64 data URI if found, or an empty string.
pub fn user_avatar_data_uri() -> String {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();

    let mut candidates: Vec<PathBuf> = Vec::new();

    if !username.is_empty() {
        candidates.push(PathBuf::from(format!(
            "/var/lib/AccountsService/icons/{username}"
        )));
    }

    if !home.is_empty() {
        candidates.push(PathBuf::from(format!("{home}/.face.icon")));
        candidates.push(PathBuf::from(format!("{home}/.face")));
    }

    for path in &candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let encoded = BASE64.encode(&bytes);
            // Guess mime from extension or default to png
            let mime = if path.extension().is_some_and(|e| e == "jpg" || e == "jpeg") {
                "image/jpeg"
            } else {
                "image/png"
            };
            return format!("data:{mime};base64,{encoded}");
        }
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adele_avatar_returns_data_uri() {
        let url = adele_avatar_data_uri();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn user_avatar_returns_string() {
        // May or may not find a user avatar depending on the system,
        // but should never panic.
        let url = user_avatar_data_uri();
        assert!(url.is_empty() || url.starts_with("data:image/"));
    }
}
