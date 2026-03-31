# Security Audit — gtk-client

**Date:** 2026-03-31
**Scope:** All source files in the `gtk-client/` project

---

## Medium Severity

### 1. Profile Files Written with Default Permissions (MEDIUM)

**File:** `src/profile.rs:48-58`

The profiles JSON file (containing WebSocket URLs) is written with default filesystem permissions, typically `0o644` (world-readable).

**Recommendation:** Use `OpenOptions` with `.mode(0o600)` and create directories with `0o700`.

---

### 2. Unchecked File Paths for Avatar Loading (MEDIUM)

**File:** `src/avatars.rs:21-54`

Avatar paths are constructed from the `USER` environment variable without character validation.

**Recommendation:** Validate that the username contains only `[a-zA-Z0-9_.-]`, or canonicalize the path and verify it remains under the expected directory.

---

### 3. JavaScript Evaluation Errors Silently Ignored (MEDIUM)

**File:** `src/webview.rs:55, 65, 75, 85, 91, 96`

All `evaluate_javascript()` calls use a no-op callback.

**Recommendation:** Log errors in the callback for diagnostics.

---

## Low Severity

### 4. No OAuth Rate Limiting (LOW-MEDIUM)

**File:** `src/oauth.rs:108-191`

The OAuth flow has a 120-second timeout but no rate limiting on attempts. CSRF state is properly validated and PKCE is implemented.

**Recommendation:** Add a cooldown between OAuth attempts.

---

### 5. Token Refresh Does Not Clear Old Tokens (LOW)

**File:** `src/widgets/login_screen.rs:349-376`

When a new refresh token is stored, the old one is not explicitly deleted first.

**Recommendation:** Explicitly delete the old token before storing the new one.

---

### 6. HTTP Client Panics on Build Failure (LOW)

**File:** `src/oauth.rs:67-70`

**Recommendation:** Replace `.expect()` with proper error propagation.

---

## Resolved (2026-03-31)

- Content Security Policy — strict CSP meta tag added to WebView template
- JavaScript string injection — replaced template-literal escaping with `serde_json::to_string()`
- Temp file TOCTOU — atomic `create_new(true)` + XDG cache dir
- CLI credential removal — `--ws-jwt`/`--ws-login-*` args and env vars removed
- Avatar URL escaping — HTML entity encoding for `src` and `alt` attributes

## Positive Findings

- Credential storage uses system keyring via the `keyring` crate
- OAuth implements PKCE with SHA-256 challenge
- CSRF state parameter is validated
- Markdown rendering uses `pulldown-cmark` which escapes HTML by default
- External link navigation is intercepted and opened in the default browser
- TLS uses `rustls` via reqwest
- No `unsafe` blocks in the codebase
- No hardcoded secrets in source
