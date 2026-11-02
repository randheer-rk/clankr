//! Identity and access control (SEC-02, CL-31-03): who a bearer token belongs to (owner,
//! ServiceAccount API token, or OIDC user) and whether their RoleBindings allow the request.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use clankr_store::tokens::ApiToken;
use clankr_types::rbac::{Principal, RoleBindingSpec, SYSTEM_NAMESPACE, required};
use clankr_types::RequestCtx;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/namespaces/{ns}/serviceaccounts/{sa}/tokens", post(issue).get(list))
        .route("/v1/namespaces/{ns}/serviceaccounts/{sa}/tokens/{id}", delete(revoke))
}

/// Resolves a non-owner bearer token. Err = presented but invalid (reason for the audit log).
pub(crate) async fn principal_for(s: &AppState, bearer: &str) -> Result<Principal, String> {
    if bearer.starts_with(clankr_store::tokens::TOKEN_PREFIX) {
        let t = s.store.token_lookup(bearer).await.map_err(|e| e.message)?.ok_or("unknown, expired or revoked API token")?;
        // a deleted ServiceAccount disables its tokens
        let sa = s.store.get(&t.namespace, "ServiceAccount", &t.service_account).await.map_err(|_| format!("service account {}/{} no longer exists", t.namespace, t.service_account))?;
        let spec = clankr_types::rbac::admit_service_account(&sa.spec).map_err(|e| e.message)?;
        return Ok(Principal::ServiceAccount { namespace: t.namespace, name: t.service_account, groups: spec.groups });
    }
    match &s.oidc {
        Some(o) => o.verify(bearer).await,
        None => Err("invalid bearer token".into()),
    }
}

/// Ok when one of the principal's RoleBindings grants what the route needs.
pub(crate) async fn authorize(s: &AppState, p: &Principal, method: &str, path: &str) -> Result<(), String> {
    if *p == Principal::Owner {
        return Ok(());
    }
    let Some((ns, perm)) = required(method, path) else { return Ok(()) };
    let subjects = p.subjects();
    // a binding in ns X applies to X; one in clankr-system applies everywhere
    let applies = |bns: &str| bns == SYSTEM_NAMESPACE || ns.as_deref() == Some(bns);
    for (bns, spec) in s.bindings.get(&s.store).await.map_err(|e| e.message)?.iter() {
        if applies(bns) && spec.role.grants(perm) && spec.subjects.iter().any(|x| subjects.contains(x)) {
            return Ok(());
        }
    }
    let at = ns.map_or_else(|| "cluster scope".to_owned(), |n| format!("namespace {n}"));
    Err(format!("{} lacks {perm:?} in {at}", p.actor()))
}

/// All RoleBindings as of a store revision. Each request costs one primary-key read of the
/// revision; bindings are re-listed only after some resource changed, so a revoked binding
/// stops working on the very next request.
#[derive(Clone, Default)]
pub struct BindingCache(Arc<Mutex<Option<(i64, Bindings)>>>);

/// (namespace, binding) pairs.
type Bindings = Arc<Vec<(String, RoleBindingSpec)>>;

impl BindingCache {
    async fn get(&self, store: &clankr_store::Store) -> Result<Bindings, clankr_types::ApiError> {
        let rev = store.revision().await?;
        let mut c = self.0.lock().await;
        if let Some((at, b)) = c.as_ref()
            && *at == rev
        {
            return Ok(b.clone());
        }
        let (items, at) = store.list("RoleBinding", None, &Default::default()).await?;
        let b: Bindings = Arc::new(items.into_iter().filter_map(|r| Some((r.metadata.namespace, serde_json::from_value(r.spec).ok()?))).collect());
        *c = Some((at, b.clone()));
        Ok(b)
    }
}

/// OIDC ID-token verification against the issuer's published keys.
pub struct Oidc {
    pub issuer: String,
    pub audience: String,
    /// Public client the web UI signs in with (authorization code + PKCE); defaults to `audience`.
    pub client_id: Option<String>,
    pub user_claim: String,
    pub groups_claim: String,
    /// Minimum time between JWKS refetches triggered by unknown key ids.
    pub min_refresh: Duration,
    http: reqwest::Client,
    keys: Mutex<Option<(JwkSet, Instant)>>,
}

const ALGS: &[Algorithm] = &[Algorithm::RS256, Algorithm::RS384, Algorithm::RS512, Algorithm::PS256, Algorithm::ES256, Algorithm::ES384, Algorithm::EdDSA];

impl Oidc {
    pub fn new(issuer: String, audience: String, user_claim: String, groups_claim: String) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("http client");
        Self { issuer: issuer.trim_end_matches('/').to_owned(), client_id: None, audience, user_claim, groups_claim, min_refresh: Duration::from_secs(30), http, keys: Mutex::new(None) }
    }

    async fn fetch_keys(&self) -> Result<JwkSet, String> {
        let get = |url: String| async move {
            let r = self.http.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("{url}: {e}"))?;
            r.json::<Value>().await.map_err(|e| format!("{url}: {e}"))
        };
        let disc = get(format!("{}/.well-known/openid-configuration", self.issuer)).await?;
        if disc["issuer"].as_str().map(|i| i.trim_end_matches('/')) != Some(self.issuer.as_str()) {
            return Err("discovery document names a different issuer".into());
        }
        let uri = disc["jwks_uri"].as_str().ok_or("discovery document has no jwks_uri")?;
        serde_json::from_value(get(uri.to_owned()).await?).map_err(|e| format!("jwks: {e}"))
    }

    /// The key for `kid`, refetching the set (at most every 30s) when it's unknown, so IdP key
    /// rotation is picked up without a restart.
    async fn key(&self, kid: &str) -> Result<DecodingKey, String> {
        let mut keys = self.keys.lock().await;
        let stale = keys.as_ref().is_none_or(|(set, at)| set.find(kid).is_none() && at.elapsed() > self.min_refresh);
        if stale {
            match self.fetch_keys().await {
                Ok(set) => *keys = Some((set, Instant::now())),
                Err(e) if keys.is_none() => return Err(format!("OIDC keys unavailable: {e}")),
                Err(e) => tracing::warn!(error = %e, "OIDC key refresh failed; using cached keys"),
            }
        }
        let jwk = keys.as_ref().and_then(|(set, _)| set.find(kid)).ok_or_else(|| format!("unknown signing key {kid:?}"))?;
        DecodingKey::from_jwk(jwk).map_err(|e| format!("signing key {kid:?}: {e}"))
    }

    pub async fn verify(&self, token: &str) -> Result<Principal, String> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| "invalid bearer token".to_owned())?;
        // asymmetric only: an HS256 token "signed" with a public key must never verify
        if !ALGS.contains(&header.alg) {
            return Err(format!("token algorithm {:?} not accepted", header.alg));
        }
        let key = self.key(header.kid.as_deref().ok_or("token has no kid")?).await?;
        let mut v = Validation::new(header.alg);
        v.set_issuer(&[&self.issuer]);
        v.set_audience(&[&self.audience]);
        v.validate_nbf = true;
        let claims = jsonwebtoken::decode::<Value>(token, &key, &v).map_err(|e| format!("token rejected: {e}"))?.claims;
        if self.user_claim == "email" && claims["email_verified"] == Value::Bool(false) {
            return Err("email not verified".into());
        }
        let id = claims[&self.user_claim].as_str().filter(|s| !s.is_empty()).ok_or_else(|| format!("token has no {:?} claim", self.user_claim))?;
        let groups = match &claims[&self.groups_claim] {
            Value::Array(a) => a.iter().filter_map(|g| g.as_str().map(str::to_owned)).collect(),
            _ => vec![],
        };
        Ok(Principal::User { id: id.to_owned(), groups })
    }
}

/// Unauthenticated: how the web UI can sign people in (no secrets here).
pub(crate) async fn auth_config(State(s): State<AppState>) -> Json<Value> {
    Json(match &s.oidc {
        Some(o) => serde_json::json!({"oidc": {"issuer": o.issuer, "clientId": o.client_id.clone().unwrap_or_else(|| o.audience.clone()), "scopes": format!("openid email {}", o.groups_claim)}}),
        None => serde_json::json!({}),
    })
}

async fn whoami(Extension(p): Extension<Principal>) -> Json<Principal> {
    Json(p)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IssueBody {
    #[serde(default)]
    description: String,
    ttl_seconds: Option<i64>,
}

async fn issue(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, sa)): Path<(String, String)>,
    Json(b): Json<IssueBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (t, plain) = s.store.token_issue(&ctx, &ns, &sa, &b.description, b.ttl_seconds).await?;
    let mut v = serde_json::to_value(t).expect("serializable");
    v["token"] = Value::String(plain);
    Ok((StatusCode::CREATED, Json(v)))
}

async fn list(State(s): State<AppState>, Path((ns, sa)): Path<(String, String)>) -> ApiResult<Json<Vec<ApiToken>>> {
    Ok(Json(s.store.tokens(&ns, &sa).await?))
}

async fn revoke(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, sa, id)): Path<(String, String, String)>) -> ApiResult<StatusCode> {
    s.store.token_revoke(&ctx, &ns, &sa, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
