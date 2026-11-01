//! Access control (SEC-02, CL-31-03, E2E-12): ServiceAccount tokens and OIDC users get exactly
//! what their RoleBindings grant; everything else is denied and audited.

use std::time::Duration;

use base64::Engine as _;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};

const OWNER: &str = "owner-token";

struct Server {
    _pg: DevPostgres,
    base: String,
    http: reqwest::Client,
}

async fn start(oidc: Option<clankrd::authz::Oidc>) -> Server {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let mut state = clankrd::AppState::new(store, OWNER.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    state.oidc = oidc.map(std::sync::Arc::new);
    let app = clankrd::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { _pg: pg, base, http: reqwest::Client::new() }
}

impl Server {
    async fn call(&self, token: &str, m: M, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = self.http.request(m, format!("{}{path}", self.base)).bearer_auth(token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        let status = r.status();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn owner(&self, m: M, path: &str, body: Value) -> Value {
        let (st, v) = self.call(OWNER, m, path, Some(body)).await;
        assert!(st.is_success(), "{path}: {st} {v}");
        v
    }

    async fn bind(&self, ns: &str, name: &str, role: &str, subjects: &[&str]) {
        self.owner(M::POST, &format!("/v1/namespaces/{ns}/rolebindings"), res(ns, "RoleBinding", name, json!({"role": role, "subjects": subjects}))).await;
    }
}

fn res(ns: &str, kind: &str, name: &str, spec: Value) -> Value {
    json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": ns}, "spec": spec})
}

fn agent(ns: &str, name: &str) -> Value {
    res(ns, "Agent", name, json!({"runtime": {"type": "oci", "image": format!("r.example/{name}@sha256:{}", "a".repeat(64))}}))
}

#[tokio::test]
async fn service_account_tokens_and_role_bindings() {
    let s = start(None).await;
    s.owner(M::POST, "/v1/namespaces/team-a/serviceaccounts", res("team-a", "ServiceAccount", "ci", json!({"groups": ["builders"]}))).await;
    let issued = s.owner(M::POST, "/v1/namespaces/team-a/serviceaccounts/ci/tokens", json!({"description": "pipeline"})).await;
    let tok = issued["token"].as_str().unwrap().to_owned();
    assert!(tok.starts_with("clk_"));
    let listed = s.owner(M::GET, "/v1/namespaces/team-a/serviceaccounts/ci/tokens", Value::Null).await;
    assert!(!listed.to_string().contains(&tok), "plaintext is shown once only");

    let (st, who) = s.call(&tok, M::GET, "/v1/whoami", None).await;
    assert_eq!((st, who["kind"].as_str(), who["name"].as_str()), (StatusCode::OK, Some("serviceAccount"), Some("ci")));

    // nothing bound yet: deny by default
    let a_agents = "/v1/namespaces/team-a/agents";
    assert_eq!(s.call(&tok, M::GET, a_agents, None).await.0, StatusCode::FORBIDDEN);

    // viewer via a group binding: read, not write
    s.bind("team-a", "builders-view", "viewer", &["group:builders"]).await;
    assert_eq!(s.call(&tok, M::GET, a_agents, None).await.0, StatusCode::OK);
    assert_eq!(s.call(&tok, M::POST, a_agents, Some(agent("team-a", "x"))).await.0, StatusCode::FORBIDDEN);

    // editor: writes in team-a only; no secrets, no RBAC changes, nothing in team-b, no cluster scope
    s.bind("team-a", "ci-edit", "editor", &["serviceaccount:team-a/ci"]).await;
    assert_eq!(s.call(&tok, M::POST, a_agents, Some(agent("team-a", "x"))).await.0, StatusCode::CREATED);
    assert_eq!(s.call(&tok, M::GET, "/v1/namespaces/team-b/agents", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(s.call(&tok, M::GET, "/v1/namespaces/team-a/secrets", None).await.0, StatusCode::FORBIDDEN);
    let escalate = res("team-a", "RoleBinding", "me-admin", json!({"role": "admin", "subjects": ["serviceaccount:team-a/ci"]}));
    assert_eq!(s.call(&tok, M::POST, "/v1/namespaces/team-a/rolebindings", Some(escalate.clone())).await.0, StatusCode::FORBIDDEN);
    assert_eq!(s.call(&tok, M::GET, "/v1/agents", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(s.call(&tok, M::GET, "/v1/audit", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(s.call(&tok, M::GET, "/v1/nodes", None).await.0, StatusCode::FORBIDDEN);

    // admin in team-a: secrets and RBAC there, still nothing cluster-only
    s.bind("team-a", "ci-admin", "admin", &["serviceaccount:team-a/ci"]).await;
    assert_eq!(s.call(&tok, M::POST, "/v1/namespaces/team-a/rolebindings", Some(escalate)).await.0, StatusCode::CREATED);
    assert_eq!(s.call(&tok, M::GET, "/v1/namespaces/team-a/secrets", None).await.0, StatusCode::OK);
    assert_eq!(s.call(&tok, M::GET, "/v1/audit", None).await.0, StatusCode::FORBIDDEN);
    // a namespace admin can't grant itself cluster rights: clankr-system is a different namespace
    let cluster = res("clankr-system", "RoleBinding", "grab", json!({"role": "cluster-admin", "subjects": ["serviceaccount:team-a/ci"]}));
    assert_eq!(s.call(&tok, M::POST, "/v1/namespaces/clankr-system/rolebindings", Some(cluster)).await.0, StatusCode::FORBIDDEN);

    // a binding in clankr-system applies everywhere; cluster-admin adds the cluster-only routes
    s.bind("clankr-system", "ci-view-all", "viewer", &["serviceaccount:team-a/ci"]).await;
    assert_eq!(s.call(&tok, M::GET, "/v1/namespaces/team-b/agents", None).await.0, StatusCode::OK);
    assert_eq!(s.call(&tok, M::GET, "/v1/agents", None).await.0, StatusCode::OK);
    s.bind("clankr-system", "ci-root", "cluster-admin", &["serviceaccount:team-a/ci"]).await;
    let (st, audit) = s.call(&tok, M::GET, "/v1/audit", None).await;
    assert_eq!(st, StatusCode::OK);
    let denial = audit.as_array().unwrap().iter().find(|e| e["decision"] == "deny" && e["actor"] == "serviceaccount:team-a/ci").expect("denials audited");
    assert!(denial["reason"].as_str().unwrap().contains("lacks"), "{denial}");
    // writes record the principal, not "owner"
    let created = audit.as_array().unwrap().iter().find(|e| e["action"] == "create" && e["name"] == "x").unwrap();
    assert_eq!(created["actor"], "serviceaccount:team-a/ci");

    // invalid bindings are refused at admission
    let (st, _) = s.call(OWNER, M::POST, "/v1/namespaces/team-a/rolebindings", Some(res("team-a", "RoleBinding", "bad", json!({"role": "editor", "subjects": ["bob"]})))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // revocation, expiry and SA deletion each end access
    let id = issued["id"].as_str().unwrap();
    s.owner(M::DELETE, &format!("/v1/namespaces/team-a/serviceaccounts/ci/tokens/{id}"), Value::Null).await;
    assert_eq!(s.call(&tok, M::GET, "/v1/whoami", None).await.0, StatusCode::UNAUTHORIZED);
    let short = s.owner(M::POST, "/v1/namespaces/team-a/serviceaccounts/ci/tokens", json!({"ttlSeconds": 1})).await["token"].as_str().unwrap().to_owned();
    assert_eq!(s.call(&short, M::GET, "/v1/whoami", None).await.0, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(s.call(&short, M::GET, "/v1/whoami", None).await.0, StatusCode::UNAUTHORIZED);
    let live = s.owner(M::POST, "/v1/namespaces/team-a/serviceaccounts/ci/tokens", json!({})).await["token"].as_str().unwrap().to_owned();
    s.owner(M::DELETE, "/v1/namespaces/team-a/serviceaccounts/ci", Value::Null).await;
    assert_eq!(s.call(&live, M::GET, "/v1/whoami", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(s.call("clk_made-up", M::GET, "/v1/whoami", None).await.0, StatusCode::UNAUTHORIZED);
}

/// Minimal IdP: discovery + JWKS for one P-256 key.
struct Idp {
    issuer: String,
    key: jsonwebtoken::EncodingKey,
}

async fn idp() -> Idp {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let raw = kp.public_key_raw(); // 0x04 || x || y
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let jwks = json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig", "alg": "ES256", "x": b64(&raw[1..33]), "y": b64(&raw[33..])}]});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let disc = json!({"issuer": issuer, "jwks_uri": format!("{issuer}/jwks")});
    let app = axum::Router::new()
        .route("/.well-known/openid-configuration", axum::routing::get(move || async move { axum::Json(disc) }))
        .route("/jwks", axum::routing::get(move || async move { axum::Json(jwks) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Idp { issuer, key: jsonwebtoken::EncodingKey::from_ec_pem(kp.serialize_pem().as_bytes()).unwrap() }
}

impl Idp {
    fn token(&self, kid: &str, claims: Value) -> String {
        let now = chrono::Utc::now().timestamp();
        let mut c = json!({"iss": self.issuer, "aud": "clankr", "iat": now, "exp": now + 300, "email": "ana@example.com", "groups": ["sre"]});
        c.as_object_mut().unwrap().extend(claims.as_object().unwrap().clone());
        let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        h.kid = Some(kid.into());
        jsonwebtoken::encode(&h, &c, &self.key).unwrap()
    }
}

#[tokio::test]
async fn oidc_users_are_verified_and_bound_by_group() {
    let idp = idp().await;
    let s = start(Some(clankrd::authz::Oidc::new(idp.issuer.clone(), "clankr".into(), "email".into(), "groups".into()))).await;
    let good = idp.token("k1", json!({}));

    let (st, who) = s.call(&good, M::GET, "/v1/whoami", None).await;
    assert_eq!((st, who), (StatusCode::OK, json!({"kind": "user", "id": "ana@example.com", "groups": ["sre"]})));
    assert_eq!(s.call(&good, M::GET, "/v1/namespaces/ops/agents", None).await.0, StatusCode::FORBIDDEN);
    s.bind("ops", "sre-approve", "approver", &["group:sre"]).await;
    assert_eq!(s.call(&good, M::GET, "/v1/namespaces/ops/agents", None).await.0, StatusCode::OK);
    assert_eq!(s.call(&good, M::POST, "/v1/namespaces/ops/agents", Some(agent("ops", "y"))).await.0, StatusCode::FORBIDDEN);

    let now = chrono::Utc::now().timestamp();
    let hs = {
        let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        h.kid = Some("k1".into());
        let c = json!({"iss": idp.issuer, "aud": "clankr", "exp": now + 300, "email": "ana@example.com"});
        jsonwebtoken::encode(&h, &c, &jsonwebtoken::EncodingKey::from_secret(b"guess")).unwrap()
    };
    for (what, bad) in [
        ("wrong audience", idp.token("k1", json!({"aud": "other"}))),
        ("wrong issuer", idp.token("k1", json!({"iss": "https://evil.example"}))),
        ("expired", idp.token("k1", json!({"exp": now - 3600}))),
        ("not yet valid", idp.token("k1", json!({"nbf": now + 3600}))),
        ("unknown key", idp.token("k2", json!({}))),
        ("unverified email", idp.token("k1", json!({"email_verified": false}))),
        ("symmetric alg", hs),
        ("tampered", format!("{}x", &good[..good.len() - 1])),
    ] {
        assert_eq!(s.call(&bad, M::GET, "/v1/whoami", None).await.0, StatusCode::UNAUTHORIZED, "{what}");
    }
}
