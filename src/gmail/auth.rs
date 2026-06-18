//! Google OAuth (authorization-code + PKCE, with **offline** access so we get a
//! refresh token) and on-disk token storage for the Gmail tools.
//!
//! Unlike threepio — which discovers OAuth endpoints from an MCP server's `401`
//! challenge — Gmail speaks the standard Gmail REST API, so the endpoints and
//! scope are known constants and the flow is much simpler. The one-time browser
//! login runs via `yoda gmail-login`; the daemon (artoo) then runs headless off
//! the cached refresh token (`bearer()` refreshes silently when the access token
//! is stale).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Full Gmail access — read, send, modify, and permanent delete. The gmail_*
/// tools span all of these, and this single scope subsumes the finer-grained
/// ones (`gmail.readonly`, `gmail.compose`, `gmail.modify`).
const SCOPE: &str = "https://mail.google.com/";

/// Localhost callback port. Must match the redirect URI registered on the Google
/// OAuth client (`http://localhost:33418/callback`); override with
/// `YODA_GMAIL_PORT` only if you registered a different one.
const DEFAULT_PORT: u16 = 33418;

fn callback_port() -> u16 {
    std::env::var("YODA_GMAIL_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

fn redirect_uri() -> String {
    format!("http://localhost:{}/callback", callback_port())
}

/// The OAuth client credentials. We reuse the same "Web application" client
/// created for Gmail: `YODA_GMAIL_CLIENT_ID/SECRET` take precedence, falling back
/// to `THREEPIO_CLIENT_ID/SECRET` (already set in artoo's launch environment) so
/// no extra configuration is needed.
fn configured_client() -> Option<(String, Option<String>)> {
    let id = env_first(&["YODA_GMAIL_CLIENT_ID", "THREEPIO_CLIENT_ID"])?;
    let secret = env_first(&["YODA_GMAIL_CLIENT_SECRET", "THREEPIO_CLIENT_SECRET"]);
    Some((id, secret))
}

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
}

/// True when Gmail is set up enough to be worth registering the tools: either a
/// cached token already exists, or client credentials are in the environment
/// (so a login could succeed).
pub fn is_configured() -> bool {
    token_path().map(|p| p.exists()).unwrap_or(false) || configured_client().is_some()
}

// --- token store --------------------------------------------------------------

/// Persisted OAuth state, in `~/.yoda/gmail.json` (`0600`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// Unix seconds at which `access_token` expires (if known).
    pub expires_at: Option<u64>,
    pub scope: Option<String>,
}

impl Store {
    fn load() -> Store {
        let Some(path) = token_path() else {
            return Store::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        let path = token_path().context("cannot determine home directory for token store")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
            restrict_dir(parent);
        }
        let json = serde_json::to_string_pretty(self)?;
        write_private(&path, json.as_bytes())
            .with_context(|| format!("could not write {}", path.display()))?;
        Ok(())
    }

    /// True when we hold an access token that is not (about to be) expired. A 60s
    /// skew guards against a token dying mid-request.
    fn token_is_fresh(&self) -> bool {
        if self.access_token.is_none() {
            return false;
        }
        match self.expires_at {
            Some(exp) => now() + 60 < exp,
            None => true,
        }
    }

    fn apply(&mut self, tokens: TokenResponse) {
        self.access_token = Some(tokens.access_token);
        // A refresh response usually omits the refresh token — keep the old one.
        if tokens.refresh_token.is_some() {
            self.refresh_token = tokens.refresh_token;
        }
        self.expires_at = tokens.expires_in.map(|secs| now() + secs);
        if tokens.scope.is_some() {
            self.scope = tokens.scope;
        }
    }
}

/// `~/.yoda/gmail.json`
fn token_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut path = PathBuf::from(home);
    path.push(".yoda");
    path.push("gmail.json");
    Some(path)
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// --- the public entry points --------------------------------------------------

/// Return a usable bearer access token, refreshing it silently if it has gone
/// stale. Errors (telling the user to run `yoda gmail-login`) when there is no
/// token and no refresh token to revive it — the daemon can't open a browser.
pub async fn bearer() -> Result<String> {
    let mut store = Store::load();
    if store.token_is_fresh() {
        return Ok(store.access_token.clone().expect("fresh implies present"));
    }
    if store.refresh_token.is_some() && refresh(&mut store).await? {
        store.save()?;
        return Ok(store.access_token.clone().expect("refresh sets the token"));
    }
    bail!("Gmail is not authorized (no usable token). Run `yoda gmail-login` once to sign in.")
}

/// Run the interactive browser login, leaving fresh tokens on disk. Requires a
/// GUI session (it opens a browser and listens on a localhost callback).
pub async fn login() -> Result<()> {
    let (client_id, client_secret) = configured_client().context(
        "no OAuth client configured — set YODA_GMAIL_CLIENT_ID/YODA_GMAIL_CLIENT_SECRET \
         (or THREEPIO_CLIENT_ID/THREEPIO_CLIENT_SECRET)",
    )?;

    let pkce = pkce()?;
    let state = random_token()?;
    let auth_url = build_auth_url(&client_id, &pkce.challenge, &state)?;

    eprintln!("yoda: opening browser to authorize Gmail…");
    eprintln!("  If it doesn't open, visit:\n  {auth_url}");
    open_browser(&auth_url);

    // The callback listener is blocking std I/O — keep it off the async runtime.
    let want_state = state.clone();
    let code = tokio::task::spawn_blocking(move || await_callback(&want_state))
        .await
        .context("callback task panicked")??;

    let tokens = exchange_code(&client_id, client_secret.as_deref(), &code, &pkce.verifier).await?;
    let mut store = Store::load();
    store.apply(tokens);
    if store.refresh_token.is_none() {
        eprintln!(
            "yoda: warning — Google returned no refresh token; the daemon won't be able to \
             refresh. Revoke the app's access at myaccount.google.com and log in again."
        );
    }
    store.save()?;
    eprintln!("yoda: Gmail authorized \u{2705}");
    Ok(())
}

// --- OAuth plumbing -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

fn build_auth_url(client_id: &str, challenge: &str, state: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(AUTH_ENDPOINT).context("bad authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri())
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        // access_type=offline + prompt=consent are what make Google mint (and
        // re-mint) a refresh token, so the headless daemon can renew on its own.
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.to_string())
}

async fn exchange_code(
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri()),
        ("client_id", client_id.to_string()),
        ("code_verifier", verifier.to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.to_string()));
    }
    post_token(form).await.context("token exchange failed")
}

/// Refresh the access token. Returns `false` when refresh isn't possible or the
/// server rejects it (refresh token expired/revoked → caller must re-login).
async fn refresh(store: &mut Store) -> Result<bool> {
    let Some((client_id, client_secret)) = configured_client() else {
        return Ok(false);
    };
    let Some(refresh_token) = store.refresh_token.clone() else {
        return Ok(false);
    };
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    match post_token(form).await {
        Ok(tokens) => {
            store.apply(tokens);
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

async fn post_token(form: Vec<(&str, String)>) -> Result<TokenResponse> {
    let resp = reqwest::Client::new()
        .post(TOKEN_ENDPOINT)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .context("token request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token endpoint returned {status}: {body}");
    }
    serde_json::from_str(&body).context("could not parse token response")
}

// --- PKCE + state -------------------------------------------------------------

struct Pkce {
    verifier: String,
    challenge: String,
}

fn pkce() -> Result<Pkce> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("rng failure: {e}"))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok(Pkce {
        verifier,
        challenge,
    })
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("rng failure: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

// --- localhost callback (blocking) --------------------------------------------

/// Block on the localhost callback, returning the authorization `code` once the
/// browser is redirected back. Validates `state` to defeat CSRF.
fn await_callback(expected_state: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", callback_port())).with_context(|| {
        format!(
            "could not bind callback port {} (in use? set YODA_GMAIL_PORT)",
            callback_port()
        )
    })?;

    let respond = |stream: &mut std::net::TcpStream, msg: &str| {
        let body = format!(
            "<html><body style='font-family:sans-serif'><h2>yoda · Gmail</h2><p>{msg}</p>\
             <p>You can close this tab and return to the terminal.</p></body></html>"
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
    };

    // Browsers open stray connections (favicon, preconnects) around the redirect;
    // loop until we see the real /callback so a stray hit doesn't consume it.
    loop {
        let (mut stream, _) = listener.accept().context("callback connection failed")?;
        let request = read_request_head(&mut stream);
        let target = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));

        if path != "/callback" {
            let _ = write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            continue;
        }

        let params = parse_query(query);
        if let Some(err) = params.iter().find(|(k, _)| k == "error") {
            respond(&mut stream, "Authorization failed.");
            bail!("authorization server returned error: {}", err.1);
        }
        if params.iter().find(|(k, _)| k == "state").map(|(_, v)| v.as_str())
            != Some(expected_state)
        {
            respond(&mut stream, "Authorization failed (state mismatch).");
            bail!("state mismatch — possible CSRF; aborting");
        }
        let code = params
            .into_iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v)
            .context("callback carried no authorization code")?;
        respond(&mut stream, "Authorized! \u{2705}");
        return Ok(code);
    }
}

/// Parse a URL query string into key/value pairs (percent-decoded) using
/// reqwest's bundled `url` parser, avoiding a direct dependency.
fn parse_query(query: &str) -> Vec<(String, String)> {
    match reqwest::Url::parse(&format!("http://localhost/?{query}")) {
        Ok(u) => u
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn read_request_head(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

// --- private file helpers (mirrors threepio's store) --------------------------

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(unix)]
fn restrict_dir(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let p = pkce().unwrap();
        let expect = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        assert_eq!(p.verifier.len(), 43); // 32 bytes base64url-no-pad
    }

    #[test]
    fn auth_url_requests_offline_consent_and_scope() {
        let url = build_auth_url("cid", "chal", "st").unwrap();
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("mail.google.com"));
    }

    #[test]
    fn parse_query_decodes_pairs() {
        let p = parse_query("code=abc%20123&state=xyz");
        assert!(p.contains(&("code".to_string(), "abc 123".to_string())));
        assert!(p.contains(&("state".to_string(), "xyz".to_string())));
    }

    #[test]
    fn fresh_requires_a_token() {
        assert!(!Store::default().token_is_fresh());
    }

    #[test]
    fn unexpired_token_is_fresh() {
        let s = Store {
            access_token: Some("t".into()),
            expires_at: Some(now() + 3600),
            ..Default::default()
        };
        assert!(s.token_is_fresh());
    }

    #[test]
    fn expired_token_is_not_fresh() {
        let s = Store {
            access_token: Some("t".into()),
            expires_at: Some(now()),
            ..Default::default()
        };
        assert!(!s.token_is_fresh());
    }
}
