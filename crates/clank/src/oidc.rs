//! `clank login`: OIDC device authorization grant (RFC 8628). The ID token is the bearer token
//! clankrd verifies; the refresh token renews it silently. Stored in
//! `~/.config/clankr/session.json` (0600).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub issuer: String,
    pub client_id: String,
    pub id_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

pub fn session_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(".config"))
        .join("clankr/session.json")
}

fn http() -> Result<Client> {
    // the IdP is public: system roots, not the cluster CA
    Ok(Client::builder().timeout(Duration::from_secs(30)).build()?)
}

fn discover(c: &Client, issuer: &str) -> Result<Value> {
    let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    Ok(c.get(&url).send()?.error_for_status().with_context(|| format!("OIDC discovery at {url}"))?.json()?)
}

fn endpoint(disc: &Value, name: &str) -> Result<String> {
    disc[name].as_str().map(str::to_owned).with_context(|| format!("issuer doesn't advertise {name}"))
}

/// Token endpoint POST. Ok(Err(code)) is an OAuth error response (e.g. authorization_pending).
fn token_request(c: &Client, url: &str, form: &[(&str, &str)]) -> Result<Result<Value, String>> {
    let r = c.post(url).form(form).send()?;
    let ok = r.status().is_success();
    let v: Value = r.json().context("token endpoint returned non-JSON")?;
    Ok(if ok { Ok(v) } else { Err(v["error"].as_str().unwrap_or("unknown_error").to_owned()) })
}

/// Seconds since epoch at which a JWT expires (payload read without verification: the server
/// verifies; we only decide when to refresh).
fn expiry(jwt: &str) -> Option<i64> {
    let payload = jwt.split('.').nth(1)?;
    let v: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()?;
    v["exp"].as_i64()
}

fn save(path: &Path, s: &Session) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?.write_all(serde_json::to_string_pretty(s)?.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Runs the device flow, printing the code for the user, and saves the session.
pub fn login(path: &Path, issuer: &str, client_id: &str, scope: &str) -> Result<Session> {
    let c = http()?;
    let disc = discover(&c, issuer)?;
    let device: Value = c
        .post(endpoint(&disc, "device_authorization_endpoint")?)
        .form(&[("client_id", client_id), ("scope", scope)])
        .send()?
        .error_for_status()
        .context("device authorization request")?
        .json()?;
    let code = device["device_code"].as_str().context("no device_code")?;
    match device["verification_uri_complete"].as_str() {
        Some(u) => eprintln!("Open {u} to sign in (code {}).", device["user_code"].as_str().unwrap_or("?")),
        None => eprintln!("Open {} and enter code {}", device["verification_uri"].as_str().unwrap_or("?"), device["user_code"].as_str().unwrap_or("?")),
    }
    let token_url = endpoint(&disc, "token_endpoint")?;
    let mut interval = device["interval"].as_u64().unwrap_or(5);
    let deadline = std::time::Instant::now() + Duration::from_secs(device["expires_in"].as_u64().unwrap_or(600));
    loop {
        if std::time::Instant::now() > deadline {
            bail!("sign-in timed out; run clank login again");
        }
        std::thread::sleep(Duration::from_secs(interval));
        let form = [("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", code), ("client_id", client_id)];
        match token_request(&c, &token_url, &form)? {
            Ok(t) => {
                let s = Session {
                    issuer: issuer.trim_end_matches('/').to_owned(),
                    client_id: client_id.to_owned(),
                    id_token: t["id_token"].as_str().context("token response has no id_token (is the openid scope allowed?)")?.to_owned(),
                    refresh_token: t["refresh_token"].as_str().map(str::to_owned),
                };
                save(path, &s)?;
                return Ok(s);
            }
            Err(e) if e == "authorization_pending" => {}
            Err(e) if e == "slow_down" => interval += 5,
            Err(e) => bail!("sign-in failed: {e}"),
        }
    }
}

/// The current ID token, refreshed first if it expires within a minute. None = not logged in.
pub fn bearer(path: &Path) -> Result<Option<String>> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Ok(None) };
    let mut s: Session = serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let now = chrono::Utc::now().timestamp();
    if expiry(&s.id_token).is_some_and(|exp| exp > now + 60) {
        return Ok(Some(s.id_token));
    }
    let Some(refresh) = s.refresh_token.clone() else { bail!("login expired; run clank login") };
    let c = http()?;
    let url = endpoint(&discover(&c, &s.issuer)?, "token_endpoint")?;
    let t = token_request(&c, &url, &[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &s.client_id)])?
        .map_err(|e| anyhow::anyhow!("login expired ({e}); run clank login"))?;
    s.id_token = t["id_token"].as_str().context("refresh returned no id_token; run clank login")?.to_owned();
    if let Some(r) = t["refresh_token"].as_str() {
        s.refresh_token = Some(r.to_owned()); // rotated
    }
    save(path, &s)?;
    Ok(Some(s.id_token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn jwt(exp: i64, n: u32) -> String {
        let b = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        format!("{}.{}.sig", b(json!({"alg": "none"})), b(json!({"exp": exp, "n": n})))
    }

    /// Device flow: pending once, then tokens; refresh hands out a new short-lived ID token.
    fn fake_idp() -> String {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let polls = Arc::new(AtomicU32::new(0));
        let disc = json!({"issuer": base, "device_authorization_endpoint": format!("{base}/device"), "token_endpoint": format!("{base}/token")});
        let app = axum::Router::new()
            .route("/.well-known/openid-configuration", get(move || async move { axum::Json(disc) }))
            .route("/device", post(|| async { axum::Json(json!({"device_code": "dc", "user_code": "WXYZ", "verification_uri": "http://idp/activate", "interval": 1, "expires_in": 30})) }))
            .route(
                "/token",
                post(move |axum::Form(f): axum::Form<std::collections::HashMap<String, String>>| {
                    let polls = polls.clone();
                    async move {
                        let n = polls.fetch_add(1, Ordering::SeqCst);
                        let now = chrono::Utc::now().timestamp();
                        let bad = |e: &str| (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": e})));
                        match f["grant_type"].as_str() {
                            "urn:ietf:params:oauth:grant-type:device_code" if n == 0 => bad("authorization_pending"),
                            // first ID token is already near expiry, so the next use refreshes
                            "urn:ietf:params:oauth:grant-type:device_code" => (axum::http::StatusCode::OK, axum::Json(json!({"id_token": jwt(now + 30, n), "refresh_token": "r1"}))),
                            "refresh_token" if f["refresh_token"] == "r1" => (axum::http::StatusCode::OK, axum::Json(json!({"id_token": jwt(now + 3600, n), "refresh_token": "r2"}))),
                            _ => bad("invalid_grant"),
                        }
                    }
                }),
            );
        std::thread::spawn(move || rt.block_on(async move { axum::serve(listener, app).await.unwrap() }));
        base
    }

    #[test]
    fn device_login_then_silent_refresh() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let issuer = fake_idp();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        assert!(bearer(&path).unwrap().is_none(), "not logged in");

        let s = login(&path, &issuer, "clank", "openid email groups offline_access").unwrap();
        assert_eq!(s.refresh_token.as_deref(), Some("r1"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        let t = bearer(&path).unwrap().unwrap();
        assert_ne!(t, s.id_token, "near-expiry token was refreshed");
        assert!(expiry(&t).unwrap() > chrono::Utc::now().timestamp() + 3000);
        let saved: Session = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.refresh_token.as_deref(), Some("r2"), "rotated refresh token persisted");
        assert_eq!(bearer(&path).unwrap().unwrap(), t, "fresh token reused without a round trip");

        // refresh rejected: clear message, no silent fallback
        let mut stale = saved;
        stale.id_token = jwt(0, 9);
        save(&path, &stale).unwrap();
        assert!(bearer(&path).unwrap_err().to_string().contains("run clank login"));
    }
}
