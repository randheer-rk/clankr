//! The node's connection to clankrd. Holds the node credential; agents never see it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use clankr_types::{ApiError, ErrorCode, Resource};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

/// One HTTP client shared by a node's instances, swapped in place when the node cert rotates.
pub type SharedHttp = Arc<RwLock<reqwest::Client>>;

/// The control plane's replicas (`--server a,b,c`). Calls go to the last one that worked; a
/// replica that can't be reached is skipped in favour of the next (HA failover).
#[derive(Clone, Debug)]
pub struct Endpoints(Arc<(Vec<String>, AtomicUsize)>);

impl Endpoints {
    pub fn parse(list: &str) -> Self {
        let urls: Vec<String> = list.split(',').map(|u| u.trim().trim_end_matches('/').to_owned()).filter(|u| !u.is_empty()).collect();
        assert!(!urls.is_empty(), "no control plane address given");
        Self(Arc::new((urls, AtomicUsize::new(0))))
    }

    /// Every replica, starting with the one that last worked.
    fn order(&self) -> Vec<(usize, &str)> {
        let (urls, cur) = &*self.0;
        let start = cur.load(Ordering::Relaxed);
        (0..urls.len()).map(|i| (start + i) % urls.len()).map(|i| (i, urls[i].as_str())).collect()
    }

    fn worked(&self, i: usize) {
        self.0.1.store(i, Ordering::Relaxed);
    }

    pub fn current(&self) -> String {
        let (urls, cur) = &*self.0;
        urls[cur.load(Ordering::Relaxed) % urls.len()].clone()
    }

    /// Runs `f` against replicas in order until one answers; only "unreachable" moves on.
    async fn first_reachable<T, F, Fut>(&self, mut f: F) -> Result<T, ApiError>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<T, Attempt>>,
    {
        let mut last = None;
        for (i, url) in self.order() {
            match f(url.to_owned()).await {
                Ok(v) => {
                    self.worked(i);
                    return Ok(v);
                }
                Err(Attempt::Refused(e)) => {
                    self.worked(i);
                    return Err(e);
                }
                Err(Attempt::Unreachable(e)) => {
                    tracing::warn!(replica = %url, error = %e.message, "control plane replica unreachable; trying the next");
                    last = Some(e);
                }
            }
        }
        Err(last.expect("at least one replica"))
    }
}

impl From<&str> for Endpoints {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

/// Outcome of one replica attempt: a real answer (even an error) stops the failover.
enum Attempt {
    Unreachable(ApiError),
    Refused(ApiError),
}

fn unreachable(e: reqwest::Error) -> Attempt {
    Attempt::Unreachable(ApiError::new(ErrorCode::StorageUnavailable, format!("control plane unreachable: {}", e.without_url())))
}

/// A response's JSON, or the server's ApiError. 503 (that replica lost its database) also fails over.
async fn answer(resp: reqwest::Response, what: &str) -> Result<Value, Attempt> {
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(v);
    }
    let e = serde_json::from_value(v).unwrap_or_else(|_| ApiError::new(ErrorCode::StorageUnavailable, format!("{what} failed: HTTP {status}")));
    Err(if status == StatusCode::SERVICE_UNAVAILABLE { Attempt::Unreachable(e) } else { Attempt::Refused(e) })
}

#[derive(Clone)]
pub struct Client {
    http: SharedHttp,
    servers: Endpoints,
    /// Owner bearer token (single-agent dev mode); empty for fleet nodes, which use mTLS.
    token: String,
    ns: String,
    /// Fleet mode: this instance's workload identity token, refreshed by the heartbeat loop.
    workload: Option<Arc<RwLock<String>>>,
}

/// mTLS material for a fleet node: the cluster CA to trust and this node's client identity.
#[derive(Clone)]
pub struct NodeTls {
    pub ca: reqwest::Certificate,
    pub identity: Option<reqwest::Identity>,
}

impl NodeTls {
    pub fn client(&self, timeout: Duration) -> reqwest::Client {
        // trust exactly the cluster CA, not the system roots
        let mut b = reqwest::Client::builder().timeout(timeout).connect_timeout(Duration::from_secs(3)).tls_certs_only([self.ca.clone()]);
        if let Some(id) = &self.identity {
            b = b.identity(id.clone());
        }
        b.build().expect("http client")
    }
}

/// An instance the scheduler placed on this node.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    pub id: String,
    pub namespace: String,
    pub agent: String,
    pub epoch: i64,
    /// Workload identity token for this instance (short-lived; refreshed every heartbeat).
    pub identity: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub run_id: String,
    pub task: String,
    pub node: String,
    pub attempt: i32,
    pub epoch: i64,
    pub idempotency_key: String,
    pub input: Value,
}

impl Client {
    pub fn new(base: &str, token: &str, ns: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(330)).build().expect("http client");
        Self { http: Arc::new(RwLock::new(http)), servers: Endpoints::parse(base), token: token.to_owned(), ns: ns.to_owned(), workload: None }
    }

    /// A fleet node's client for one instance: mTLS as the node, acting under `workload`.
    pub fn fleet(servers: &Endpoints, http: SharedHttp, ns: &str, workload: Arc<RwLock<String>>) -> Self {
        Self { http, servers: servers.clone(), token: String::new(), ns: ns.to_owned(), workload: Some(workload) }
    }

    /// Sends a request; returns the JSON body (Null for 204) or the server's ApiError. Fails over
    /// to another replica only when the connection was never made, so nothing is sent twice.
    async fn call(&self, m: Method, path: &str, body: Option<&Value>, on_behalf_of: Option<&str>) -> Result<Value, ApiError> {
        let http = self.http.read().expect("lock").clone();
        let mut last = None;
        for (i, base) in self.servers.order() {
            let mut rb = http.request(m.clone(), format!("{base}/v1/namespaces/{}{path}", self.ns));
            if !self.token.is_empty() {
                rb = rb.bearer_auth(&self.token);
            }
            if let Some(w) = &self.workload {
                rb = rb.header("x-clankr-workload", w.read().expect("lock").as_str());
            }
            if let Some(b) = body {
                rb = rb.json(b);
            }
            if let Some(who) = on_behalf_of {
                rb = rb.header("x-clankr-on-behalf-of", who);
            }
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => {
                    let connect = e.is_connect();
                    let err = ApiError::new(ErrorCode::StorageUnavailable, format!("control plane unreachable: {}", e.without_url()));
                    if connect {
                        last = Some(err);
                        continue;
                    }
                    // the request may have arrived: don't resend, but use another replica next time
                    self.servers.worked((i + 1) % self.servers.0.0.len());
                    return Err(err);
                }
            };
            self.servers.worked(i);
            let status = resp.status();
            if status == StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
            }
            return Err(serde_json::from_str(&text).unwrap_or_else(|_| ApiError::new(ErrorCode::StorageUnavailable, format!("HTTP {status}: {text}"))));
        }
        Err(last.expect("at least one replica"))
    }

    pub async fn agent(&self, name: &str) -> Result<Resource, ApiError> {
        let v = self.call(Method::GET, &format!("/agents/{name}"), None, None).await?;
        serde_json::from_value(v).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, e.to_string()))
    }

    pub async fn lease(&self, queue: &str, owner: &str, ttl: u64, wait: u64) -> Result<Option<Lease>, ApiError> {
        let v = self.call(Method::POST, "/tasks/lease", Some(&json!({"queue": queue, "owner": owner, "ttlSeconds": ttl, "waitSeconds": wait})), None).await?;
        if v.is_null() {
            return Ok(None);
        }
        serde_json::from_value(v).map(Some).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, e.to_string()))
    }

    pub async fn heartbeat(&self, l: &Lease, ttl: u64) -> Result<(), ApiError> {
        self.call(Method::POST, "/tasks/heartbeat", Some(&json!({"runId": l.run_id, "task": l.task, "epoch": l.epoch, "ttlSeconds": ttl})), None).await.map(drop)
    }

    pub async fn complete(&self, l: &Lease, result: Value) -> Result<(), ApiError> {
        self.call(Method::POST, "/tasks/complete", Some(&json!({"runId": l.run_id, "task": l.task, "epoch": l.epoch, "result": result})), None).await.map(drop)
    }

    pub async fn fail(&self, l: &Lease, error: &str, retryable: bool) -> Result<(), ApiError> {
        let body = json!({"runId": l.run_id, "task": l.task, "epoch": l.epoch, "error": error, "retryable": retryable});
        self.call(Method::POST, "/tasks/fail", Some(&body), None).await.map(drop)
    }

    pub async fn generate(&self, claim: &str, request: &Value, who: &str) -> Result<Value, ApiError> {
        self.call(Method::POST, &format!("/models/{claim}/generate"), Some(request), Some(who)).await
    }

    pub async fn context_get(&self, claim: &str, key: &str, who: &str) -> Result<Value, ApiError> {
        self.call(Method::GET, &format!("/contexts/{claim}/keys/{}", encode_key(key)), None, Some(who)).await
    }

    pub async fn context_put(&self, claim: &str, key: &str, value: &Value, if_version: Option<i64>, who: &str) -> Result<Value, ApiError> {
        let body = json!({"value": value, "ifVersion": if_version});
        self.call(Method::PUT, &format!("/contexts/{claim}/keys/{}", encode_key(key)), Some(&body), Some(who)).await
    }

    pub async fn context_list(&self, claim: &str, prefix: &str, who: &str) -> Result<Value, ApiError> {
        self.call(Method::GET, &format!("/contexts/{claim}/keys?prefix={}", encode_key(prefix)), None, Some(who)).await
    }

    /// Mailbox operations (`op` = messages | receive | ack | nack), brokered for an agent.
    pub async fn mailbox(&self, mailbox: &str, op: &str, body: &Value, who: &str) -> Result<Value, ApiError> {
        self.call(Method::POST, &format!("/mailboxes/{}/{op}", encode_key(mailbox)), Some(body), Some(who)).await
    }

    /// Joins the cluster: sends a CSR for a key generated here, returns the node's client
    /// identity (certificate + key) and its certificate PEM. The key never leaves this process.
    pub async fn join(servers: &Endpoints, ca: &reqwest::Certificate, name: &str, join_token: &str, capacity: &Value, labels: &Value) -> Result<(reqwest::Identity, String), ApiError> {
        let (key, csr) = clankr_tls::node_key_and_csr(name).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, e.to_string()))?;
        let http = NodeTls { ca: ca.clone(), identity: None }.client(Duration::from_secs(30));
        let body = json!({"name": name, "joinToken": join_token, "csr": csr, "capacity": capacity, "labels": labels});
        let v = servers
            .first_reachable(|base| {
                let rb = http.post(format!("{base}/v1/nodes/join")).json(&body);
                async move { answer(rb.send().await.map_err(unreachable)?, "join").await }
            })
            .await?;
        identity(&v, &key)
    }

    /// Rotates the node certificate: new key here, CSR to clankrd over the current mTLS identity.
    pub async fn renew(servers: &Endpoints, tls: &NodeTls, name: &str) -> Result<(reqwest::Identity, String), ApiError> {
        let (key, csr) = clankr_tls::node_key_and_csr(name).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, e.to_string()))?;
        let http = tls.client(Duration::from_secs(30));
        let body = json!({"csr": csr});
        let v = servers
            .first_reachable(|base| {
                let rb = http.post(format!("{base}/v1/nodes/{name}/renew")).json(&body);
                async move { answer(rb.send().await.map_err(unreachable)?, "renew").await }
            })
            .await?;
        identity(&v, &key)
    }

    /// Heartbeat over mTLS; returns this node's current assignments with fresh identity tokens.
    pub async fn node_heartbeat(servers: &Endpoints, tls: &NodeTls, name: &str) -> Result<Vec<Assignment>, ApiError> {
        let http = tls.client(Duration::from_secs(10));
        let v = servers
            .first_reachable(|base| {
                let rb = http.post(format!("{base}/v1/nodes/{name}/heartbeat"));
                async move { answer(rb.send().await.map_err(unreachable)?, "heartbeat").await }
            })
            .await?;
        serde_json::from_value(v["assignments"].clone()).map_err(|e| ApiError::new(ErrorCode::StorageUnavailable, e.to_string()))
    }
}

fn identity(v: &Value, key: &str) -> Result<(reqwest::Identity, String), ApiError> {
    let cert = v["certificate"].as_str().ok_or_else(|| ApiError::new(ErrorCode::StorageUnavailable, "reply without certificate"))?;
    let id = reqwest::Identity::from_pem(format!("{cert}\n{key}").as_bytes()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, e.to_string()))?;
    Ok((id, cert.to_owned()))
}

/// Percent-encodes everything except unreserved characters and `/` (keys are path-like).
fn encode_key(k: &str) -> String {
    k.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
