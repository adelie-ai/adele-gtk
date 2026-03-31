# Security Audit — gtk-client

**Date:** 2026-03-31
**Scope:** All source files in the `gtk-client/` project

---

## Critical / High Severity

### 1. No Content Security Policy on WebView (HIGH)

**File:** `src/markdown.rs:73-292`

The HTML template loaded into the WebKit WebView contains no Content Security Policy. This means there is no browser-level defense against inline script injection or external resource loading if any XSS vector is found.

**Recommendation:** Add a strict CSP meta tag to the HTML template:

```html
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; img-src data: file:;">
```

Alternatively, disable JavaScript entirely if not required: `settings.set_enable_javascript(false)`.

---

### 2. JavaScript String Injection via evaluate_javascript (HIGH)

**File:** `src/webview.rs:49-76`

User-generated content (rendered markdown) is passed into `evaluate_javascript()` by escaping backslashes, backticks, and `${` sequences, then interpolating into a template literal:

```rust
let escaped = messages_html
    .replace('\\', "\\\\")
    .replace('`', "\\`")
    .replace("${", "\\${");
let js = format!("updateMessages(`{escaped}`);");
webview.evaluate_javascript(&js, ...);
```

This escaping approach is brittle. Any missed escape sequence could allow code injection.

**Recommendation:** Replace string injection with WebKit's message passing API (`webkit::UserContentManager` with registered script message handlers), which avoids constructing JavaScript from untrusted strings entirely.

---

### 3. Insecure Temporary File Handling — TOCTOU (HIGH)

**Files:**
- `src/window.rs:786-795`
- `src/widgets/login_screen.rs:54-57`
- `src/widgets/sidebar.rs:39-42`

Icon files are written to `/tmp/adelie-gtk-icons/` with a check-then-write pattern:

```rust
if !icon_path.exists() {
    std::fs::create_dir_all(&icon_dir)?;
    std::fs::write(&icon_path, ICON_BYTES)?;
}
```

An attacker could place a symlink between the existence check and the write, causing the application to overwrite arbitrary files. Permissions are not explicitly set.

**Recommendation:**
- Use `OpenOptions::new().create_new(true).mode(0o600)` to atomically create files
- Use an XDG cache directory instead of `/tmp`

---

### 4. Credentials in CLI Arguments and Environment Variables (FIXED)

**File:** `src/main.rs`

**Status:** Fixed (2026-03-31)
**Resolution:** Removed `--ws-jwt`, `--ws-login-username`, `--ws-login-password` CLI args and their `ADELIE_GTK_*` env var equivalents from both the gtk-client and TUI. The gtk-client now always shows the login screen. Authentication uses the `/login` endpoint with credentials stored in the system keyring.

---

## Medium Severity

### 5. Unescaped Avatar URLs in HTML (MEDIUM)

**File:** `src/markdown.rs:26, 54-56`

Avatar URLs are interpolated directly into HTML `<img>` tags without HTML entity encoding:

```rust
format!(r#"<img class="avatar" src="{url}" alt="{alt}">"#)
```

Currently the URLs are from controlled sources (data URIs, local files), but if external URLs are ever accepted, this becomes an XSS vector.

**Recommendation:** HTML-encode the URL and alt text, or validate URLs against a `data:`/`file:` scheme allowlist.

---

### 6. Profile Files Written with Default Permissions (MEDIUM)

**File:** `src/profile.rs:48-58`

The profiles JSON file (containing WebSocket URLs) is written with default filesystem permissions, typically `0o644` (world-readable).

```rust
std::fs::create_dir_all(parent)?;  // 0o755
std::fs::write(&self.path, data)?; // 0o644
```

**Recommendation:** Use `OpenOptions` with `.mode(0o600)` and create directories with `0o700`.

---

### 7. Unchecked File Paths for Avatar Loading (MEDIUM)

**File:** `src/avatars.rs:21-54`

Avatar paths are constructed from the `USER` environment variable without character validation:

```rust
let username = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
candidates.push(PathBuf::from(format!("/var/lib/AccountsService/icons/{username}")));
```

A crafted `USER` value (e.g. `../../etc/passwd`) could cause unintended file reads.

**Recommendation:** Validate that the username contains only `[a-zA-Z0-9_.-]`, or canonicalize the path and verify it remains under the expected directory.

---

### 8. JavaScript Evaluation Errors Silently Ignored (MEDIUM)

**File:** `src/webview.rs:55, 65, 75, 85, 91, 96`

All `evaluate_javascript()` calls use a no-op callback:

```rust
webview.evaluate_javascript(&js, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
```

Failures in rendering updates go unnoticed.

**Recommendation:** Log errors in the callback for diagnostics.

---

## Low Severity

### 9. No OAuth Rate Limiting (LOW-MEDIUM)

**File:** `src/oauth.rs:108-191`

The OAuth flow has a 120-second timeout but no rate limiting on attempts. CSRF state is properly validated and PKCE is implemented.

**Recommendation:** Add a cooldown between OAuth attempts.

---

### 10. Token Refresh Does Not Clear Old Tokens (LOW)

**File:** `src/widgets/login_screen.rs:349-376`

When a new refresh token is stored, the old one is not explicitly deleted first:

```rust
if let Some(ref new_refresh) = tokens.refresh_token {
    let _ = CredentialStore::store_refresh_token(&profile.id, new_refresh);
}
```

**Recommendation:** Explicitly delete the old token before storing the new one, or verify that the keyring backend overwrites atomically.

---

### 11. HTTP Client Panics on Build Failure (LOW)

**File:** `src/oauth.rs:67-70`

```rust
let http_client = reqwest::ClientBuilder::new()
    .redirect(reqwest::redirect::Policy::none())
    .build()
    .expect("HTTP client should build");
```

**Recommendation:** Replace `.expect()` with proper error propagation.

---

## Positive Findings

- Credential storage uses system keyring via the `keyring` crate
- OAuth implements PKCE with SHA-256 challenge
- CSRF state parameter is validated
- Markdown rendering uses `pulldown-cmark` which escapes HTML by default
- External link navigation is intercepted and opened in the default browser
- TLS uses `rustls` via reqwest
- No `unsafe` blocks in the codebase
- No hardcoded secrets in source
