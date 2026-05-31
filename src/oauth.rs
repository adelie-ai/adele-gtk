use anyhow::{Context, Result};
use serde::Deserialize;

/// Auth discovery response from the server's GET /auth/config endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthDiscovery {
    pub methods: Vec<String>,
    pub oidc: Option<OidcDiscovery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OidcDiscovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub scopes: String,
}

/// Tokens returned from an OAuth2 flow.
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Discover auth configuration from the server.
///
/// Derives the HTTP base URL from the WebSocket URL (ws:// -> http://, wss:// -> https://).
/// When the server uses a self-signed certificate (e.g. local daemon), pass the
/// CA certificate path so the HTTPS request can verify it.
pub async fn discover_auth_config(
    ws_url: &str,
    tls_ca_cert: Option<&std::path::Path>,
) -> Result<AuthDiscovery> {
    let base_url = ws_url_to_http_base(ws_url);
    let url = format!("{base_url}/auth/config");

    let client = build_http_client(tls_ca_cert)?;

    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching auth config from {url}"))?;

    if !response.status().is_success() {
        // Server doesn't support discovery -- assume password-only (backward compat)
        return Ok(AuthDiscovery {
            methods: vec!["password".to_string()],
            oidc: None,
        });
    }

    response
        .json::<AuthDiscovery>()
        .await
        .with_context(|| "parsing auth config response")
}

/// Build the HTTP client used for auth discovery, optionally trusting a
/// caller-supplied CA certificate (for a self-signed local daemon).
///
/// SECURITY: when a CA certificate *is present* but cannot be honored, the
/// error is propagated as `Err` rather than swallowed. Previously the cert was
/// loaded behind `if let Ok(pem_bytes) = std::fs::read(ca_path)` and the client
/// built with `builder.build().unwrap_or_else(|_| Client::new())`, so a present
/// but unreadable/unparseable CA cert — or a builder failure — silently fell
/// back to a *default-trust* client, downgrading TLS trust without telling the
/// caller.
///
/// A *missing* CA file (`NotFound`) is treated as "no cert configured" and we
/// proceed without one: callers always pass the daemon's default CA path even
/// when it hasn't been generated, and that case is not a trust downgrade
/// (there was no configured cert to honor). Any other read failure, a parse
/// failure, or a builder failure is a hard error.
fn build_http_client(tls_ca_cert: Option<&std::path::Path>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10));
    if let Some(ca_path) = tls_ca_cert {
        match std::fs::read(ca_path) {
            Ok(pem_bytes) => {
                let cert = reqwest::tls::Certificate::from_pem(&pem_bytes).with_context(|| {
                    format!("parsing CA certificate from {}", ca_path.display())
                })?;
                builder = builder.add_root_certificate(cert);
            }
            // No cert at the configured path — proceed with default trust.
            // This is the daemon's optional auto-generated CA that may not
            // exist yet; not a downgrade of an actually-configured cert.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // The cert exists but we couldn't read it (permissions, etc.):
            // do NOT silently fall back to default trust.
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading CA certificate from {}", ca_path.display()));
            }
        }
    }
    builder.build().context("building HTTP client")
}

/// Run the full OAuth2 Authorization Code + PKCE flow.
///
/// 1. Generate PKCE verifier + challenge
/// 2. Start a local HTTP server on a random port
/// 3. Open the browser to the authorization URL
/// 4. Wait for the redirect with the auth code
/// 5. Exchange the code for tokens
pub async fn run_oauth_flow(oidc: &OidcDiscovery) -> Result<TokenResponse> {
    use oauth2::{
        AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope,
        TokenResponse as _, TokenUrl, basic::BasicClient,
    };

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client should build");

    let client = BasicClient::new(ClientId::new(oidc.client_id.clone()))
        .set_auth_uri(AuthUrl::new(oidc.authorization_endpoint.clone())?)
        .set_token_uri(TokenUrl::new(oidc.token_endpoint.clone())?);

    // Bind a local listener for the redirect
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    let redirect_uri = format!("http://127.0.0.1:{}", local_addr.port());

    let client = client.set_redirect_uri(RedirectUrl::new(redirect_uri)?);

    // Generate PKCE challenge
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Build authorization URL
    let scopes: Vec<Scope> = oidc
        .scopes
        .split_whitespace()
        .map(|s| Scope::new(s.to_string()))
        .collect();

    let (auth_url, csrf_state) = {
        let mut req = client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(pkce_challenge);
        for scope in &scopes {
            req = req.add_scope(scope.clone());
        }
        req.url()
    };

    // Open browser
    tracing::info!("opening browser for OAuth login");
    open::that(auth_url.to_string()).with_context(|| "failed to open browser")?;

    // Wait for the redirect (with timeout)
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        accept_redirect(listener, &csrf_state),
    )
    .await
    .map_err(|_| anyhow::anyhow!("OAuth redirect timed out after 120s"))??;

    // Exchange code for tokens
    let token_result = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;

    Ok(TokenResponse {
        access_token: token_result.access_token().secret().clone(),
        refresh_token: token_result.refresh_token().map(|t| t.secret().clone()),
    })
}

/// Refresh an access token using a refresh token.
pub async fn refresh_access_token(
    oidc: &OidcDiscovery,
    refresh_token: &str,
) -> Result<TokenResponse> {
    use oauth2::{
        AuthUrl, ClientId, RefreshToken, TokenResponse as _, TokenUrl, basic::BasicClient,
    };

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client should build");

    let client = BasicClient::new(ClientId::new(oidc.client_id.clone()))
        .set_auth_uri(AuthUrl::new(oidc.authorization_endpoint.clone())?)
        .set_token_uri(TokenUrl::new(oidc.token_endpoint.clone())?);

    let token_result = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("refresh token exchange failed: {e}"))?;

    Ok(TokenResponse {
        access_token: token_result.access_token().secret().clone(),
        refresh_token: token_result.refresh_token().map(|t| t.secret().clone()),
    })
}

/// Accept the OAuth redirect and extract the authorization code.
async fn accept_redirect(
    listener: tokio::net::TcpListener,
    expected_state: &oauth2::CsrfToken,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut stream, _) = listener.accept().await?;

    // Read until the end of the HTTP request head (`\r\n\r\n`) rather than a
    // fixed buffer: a long claim set / large query string can push the request
    // line past 4096 bytes, and a fixed read would truncate it before the
    // `code`/`state` params were fully captured. A generous cap guards against
    // a peer that never sends the header terminator.
    const MAX_REQUEST_BYTES: usize = 64 * 1024;
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break; // peer closed before sending the terminator
        }
        data.extend_from_slice(&chunk[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
            break; // end of request head reached
        }
        if data.len() >= MAX_REQUEST_BYTES {
            anyhow::bail!("OAuth redirect request exceeded {MAX_REQUEST_BYTES} bytes");
        }
    }
    let request = String::from_utf8_lossy(&data);

    // Parse the GET request line to extract query params
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP request from redirect"))?;

    let url = url::Url::parse(&format!("http://localhost{path}"))?;
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    // Check CSRF state
    let state = params
        .get("state")
        .ok_or_else(|| anyhow::anyhow!("missing state parameter"))?;
    if state != expected_state.secret() {
        anyhow::bail!("CSRF state mismatch");
    }

    // Check for error
    if let Some(error) = params.get("error") {
        let desc = params.get("error_description").cloned().unwrap_or_default();
        anyhow::bail!("OAuth error: {error} - {desc}");
    }

    let code = params
        .get("code")
        .ok_or_else(|| anyhow::anyhow!("missing authorization code"))?
        .clone();

    // Send a response to the browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h2>Login successful!</h2>\
        <p>You can close this window and return to Adelie.</p></body></html>";
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;

    Ok(code)
}

/// Convert a WebSocket URL to an HTTP base URL.
///
/// `ws://host:port/ws` -> `http://host:port`
/// `wss://host:port/ws` -> `https://host:port`
fn ws_url_to_http_base(ws_url: &str) -> String {
    let http_url = ws_url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    // Strip the path (e.g., /ws)
    if let Ok(parsed) = url::Url::parse(&http_url) {
        format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or("localhost"),
            parsed.port().map(|p| format!(":{p}")).unwrap_or_default()
        )
    } else {
        http_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oauth2::CsrfToken;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_ws_url_to_http_base() {
        assert_eq!(
            ws_url_to_http_base("ws://127.0.0.1:11339/ws"),
            "http://127.0.0.1:11339"
        );
        assert_eq!(
            ws_url_to_http_base("wss://example.com/ws"),
            "https://example.com"
        );
        assert_eq!(
            ws_url_to_http_base("wss://example.com:8443/ws"),
            "https://example.com:8443"
        );
    }

    // --- Item 4: CA cert must not be silently dropped --------------------
    //
    // The historical bug: a configured CA cert was loaded behind
    // `if let Ok(pem_bytes) = std::fs::read(ca_path)` and the client built with
    // `builder.build().unwrap_or_else(|_| Client::new())`. So a present but
    // unreadable CA file (or a builder failure) was silently swallowed and the
    // HTTPS request proceeded with *default* trust — a TLS-trust downgrade the
    // caller never learned about. The fix propagates the error: when a CA cert
    // is present but cannot be loaded, `build_http_client` must return `Err`,
    // never a silent default-trust client.
    #[test]
    fn build_http_client_errors_when_present_ca_cert_cannot_be_loaded() {
        // A directory at the cert path is a present-but-unreadable-as-a-file
        // entry: `std::fs::read` returns a non-NotFound error, which stands in
        // for any "configured CA cert we were asked to trust but can't honor"
        // — the exact case the old `if let Ok(..)` guard swallowed.
        let dir =
            std::env::temp_dir().join(format!("adele-gtk-oauth-ca-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = build_http_client(Some(dir.as_path()));
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            result.is_err(),
            "a present-but-unloadable CA cert must produce Err, not a silent default-trust client"
        );
    }

    #[test]
    fn build_http_client_tolerates_missing_ca_cert_path() {
        // A *missing* CA file is "no cert configured" — callers always pass the
        // daemon's default path even when it hasn't been generated. This must
        // NOT error (preserves the success path); there is no configured cert
        // to downgrade away from.
        let missing = std::path::Path::new("/nonexistent/adele-gtk/ca-does-not-exist.pem");
        assert!(build_http_client(Some(missing)).is_ok());
    }

    #[test]
    fn build_http_client_succeeds_with_no_ca_cert() {
        // The success path (no CA configured) must remain unchanged: a usable
        // client is returned.
        assert!(build_http_client(None).is_ok());
    }

    // --- Item 3: accept_redirect branch coverage -------------------------

    /// Connect to `addr`, send `request`, and read the HTTP response body
    /// back so the server side can finish its write/shutdown.
    async fn send_redirect_request(addr: std::net::SocketAddr, request: &str) {
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        client.flush().await.unwrap();
        // Drain whatever the server sends so its `write_all`/`shutdown` can
        // complete; ignore content.
        let mut sink = Vec::new();
        let _ = client.read_to_end(&mut sink).await;
    }

    #[tokio::test]
    async fn accept_redirect_rejects_csrf_state_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = CsrfToken::new("the-real-state".to_string());

        let server = tokio::spawn(async move { accept_redirect(listener, &expected).await });
        send_redirect_request(
            addr,
            "GET /?code=abc&state=WRONG-STATE HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;

        let result = server.await.unwrap();
        let err = result.expect_err("CSRF state mismatch must be rejected");
        assert!(
            err.to_string().contains("CSRF state mismatch"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn accept_redirect_propagates_oauth_error_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = CsrfToken::new("state-123".to_string());

        let server = tokio::spawn(async move { accept_redirect(listener, &expected).await });
        send_redirect_request(
            addr,
            "GET /?state=state-123&error=access_denied&error_description=nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;

        let result = server.await.unwrap();
        let err = result.expect_err("an error= response must be surfaced as Err");
        assert!(
            err.to_string().contains("OAuth error") && err.to_string().contains("access_denied"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn accept_redirect_rejects_missing_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = CsrfToken::new("state-xyz".to_string());

        let server = tokio::spawn(async move { accept_redirect(listener, &expected).await });
        // Valid state, no error, but no `code` param.
        send_redirect_request(
            addr,
            "GET /?state=state-xyz HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;

        let result = server.await.unwrap();
        let err = result.expect_err("a missing code param must be rejected");
        assert!(
            err.to_string().contains("missing authorization code"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn accept_redirect_extracts_code_on_happy_path() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = CsrfToken::new("state-ok".to_string());

        let server = tokio::spawn(async move { accept_redirect(listener, &expected).await });
        send_redirect_request(
            addr,
            "GET /?code=happy-code&state=state-ok HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;

        let code = server
            .await
            .unwrap()
            .expect("happy path must yield the code");
        assert_eq!(code, "happy-code");
    }

    // --- Item 5: long redirect must not be truncated at 4096 bytes -------
    //
    // Historically the handler read exactly 4096 bytes; a long claim set /
    // large query string would be truncated and the request line never fully
    // parsed. The fix reads until the end of the HTTP headers (`\r\n\r\n`).
    #[tokio::test]
    async fn accept_redirect_parses_request_longer_than_4096_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = CsrfToken::new("state-long".to_string());

        // Pad the query string so the full request comfortably exceeds 4096
        // bytes. `code`/`state` live near the front of the line but the line
        // (and headers) push well past the old fixed buffer.
        let padding = "x".repeat(8192);
        let request = format!(
            "GET /?code=long-code&state=state-long&blob={padding} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert!(request.len() > 4096);

        let server = tokio::spawn(async move { accept_redirect(listener, &expected).await });
        send_redirect_request(addr, &request).await;

        let code = server
            .await
            .unwrap()
            .expect("a >4096-byte redirect must still parse");
        assert_eq!(code, "long-code");
    }
}
