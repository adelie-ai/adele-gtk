//! Embedded-asset cache helpers.
//!
//! Several widgets embed a PNG via `include_bytes!` and need it on disk as
//! a real file (GTK's `Image::from_file` / icon-theme search path can't read
//! an in-memory `&[u8]`). They all wrote the bytes to a path under the user
//! cache directory with the same idempotent create-new dance; this module
//! centralizes that so the write semantics live in one place.

use std::io::Write;
use std::path::PathBuf;

/// Write `bytes` to `<cache-dir>/<name>`, returning the resolved path.
///
/// `name` may contain sub-directories (e.g. `hicolor/512x512/apps/x.png`);
/// any missing parent directories are created. The cache directory falls
/// back to the system temp directory when no user cache dir is available,
/// matching the previous call-site behavior.
///
/// The write uses `create_new` so it is idempotent and TOCTOU-free: the file
/// is either atomically created and filled, or already exists (treated as
/// success). Any other I/O error is returned to the caller.
pub fn extract_to_cache(bytes: &[u8], name: &str) -> std::io::Result<PathBuf> {
    let path = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(name);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // `create_new` avoids TOCTOU: the write either atomically creates the
    // file or harmlessly fails because it already exists.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => file.write_all(bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }

    Ok(path)
}
