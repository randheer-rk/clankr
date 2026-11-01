//! Model gateway (PRD §16.1, §27). Agents name a ModelClaim; the gateway resolves it to a
//! ProviderConnection, checks the model allowlist and token budget, brokers the credential
//! (the agent never sees it), calls the provider, and records usage.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use clankr_store::Store;
use clankr_store::secrets::MasterKey;
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod providers;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    OpenaiCompatible,
    Anthropic,
    /// Deterministic, offline; for tests and `clank dev`.
    Mock,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretRef {
    pub name: String,
    pub key: String,
}

/// `kind: ProviderConnection` spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConnectionSpec {
    pub provider: Provider,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    pub allowed_models: Vec<String>,
    /// Per-model prices; needed for any cost budget. Applied price is recorded per call.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pricing: BTreeMap<String, Price>,
}

/// USD per million tokens (numerically: micro-USD per token).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    #[serde(rename = "inputUSDPerMTok")]
    pub input: f64,
    #[serde(rename = "outputUSDPerMTok")]
    pub output: f64,
}

impl Price {
    /// Cost in micro-USD, rounded up so reservations never under-count.
    pub fn micro_usd(&self, input_tokens: i64, output_tokens: i64) -> i64 {
        (input_tokens as f64 * self.input + output_tokens as f64 * self.output).ceil() as i64
    }
}

/// `kind: ModelClaim` spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelClaimSpec {
    pub provider_connection: String,
    pub model: String,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub budget: Budget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_max_output")]
    pub max_output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_output_tokens: default_max_output(), max_input_tokens: None }
    }
}

fn default_max_output() -> u32 {
    4096
}

/// Token and cost ceilings. Enforced before dispatch with reservations, so parallel calls can't
/// overshoot. Cost limits need `pricing` for the model on the ProviderConnection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_per_run: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_total: Option<i64>,
    #[serde(default, rename = "maxCostUSDPerRun", skip_serializing_if = "Option::is_none")]
    pub max_cost_usd_per_run: Option<f64>,
    #[serde(default, rename = "maxCostUSDTotal", skip_serializing_if = "Option::is_none")]
    pub max_cost_usd_total: Option<f64>,
}

/// One counter row in `model_usage`: (key, scope) with a hard max, in tokens or micro-USD.
#[derive(Debug, Clone)]
struct Bucket {
    key: String,
    scope: String,
    max: i64,
    cost: bool,
    /// Human name for errors.
    what: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateRequest {
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Attribution and per-run budget scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateResponse {
    pub text: String,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    /// Provenance of what actually served the request (PRV-04).
    pub provider: Provider,
    pub model: String,
    pub connection: String,
}

fn invalid(field: &str, msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::InvalidSpec, msg).field(field)
}

/// Admission for ProviderConnection specs (CL-27-01).
pub fn admit_connection(spec: &serde_json::Value) -> Result<ProviderConnectionSpec, ApiError> {
    let s: ProviderConnectionSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("provider connection: {e}")))?;
    if s.allowed_models.is_empty() {
        return Err(invalid("spec.allowedModels", "list at least one model"));
    }
    if s.provider != Provider::Mock {
        check_endpoint(&s.endpoint)?;
        if s.secret_ref.is_none() {
            return Err(invalid("spec.secretRef", "provider needs a secretRef {name, key}"));
        }
    }
    Ok(s)
}

/// https only, except plain http to loopback for local inference servers.
// ponytail: no DNS-level private-range blocking yet (SEC-13); the endpoint is operator-set, not agent-set
fn check_endpoint(endpoint: &str) -> Result<(), ApiError> {
    let u = url::Url::parse(endpoint).map_err(|e| invalid("spec.endpoint", format!("bad url: {e}")))?;
    let loopback = matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match u.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(invalid("spec.endpoint", "endpoint must be https (http only for localhost)")),
    }
}

pub fn admit_claim(spec: &serde_json::Value) -> Result<ModelClaimSpec, ApiError> {
    let s: ModelClaimSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("model claim: {e}")))?;
    if s.limits.max_output_tokens == 0 {
        return Err(invalid("spec.limits.maxOutputTokens", "must be > 0"));
    }
    Ok(s)
}

/// Rough pre-dispatch input estimate (~4 chars/token). Only used to reserve budget;
/// the provider's reported usage is what gets charged.
fn estimate_input(req: &GenerateRequest) -> i64 {
    let chars: usize = req.messages.iter().map(|m| m.content.len()).sum::<usize>() + req.system.as_ref().map_or(0, String::len);
    (chars as i64 / 4).max(1)
}

#[derive(Clone)]
pub struct Gateway {
    store: Store,
    master: MasterKey,
    http: reqwest::Client,
}

impl Gateway {
    pub fn new(store: Store, master: MasterKey) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none()) // a redirect could leak the credential elsewhere
            .timeout(Duration::from_secs(300))
            .build()
            .expect("http client");
        Self { store, master, http }
    }

    #[tracing::instrument(name = "model.generate", skip(self, ctx, req), fields(
        run_id = req.run_id.map(|r| r.to_string()), task = req.task.as_deref(), actor = %ctx.actor,
        provider = tracing::field::Empty, model = tracing::field::Empty,
        input_tokens = tracing::field::Empty, output_tokens = tracing::field::Empty, outcome = tracing::field::Empty,
    ))]
    pub async fn generate(&self, ctx: &RequestCtx, ns: &str, claim_name: &str, req: GenerateRequest) -> Result<GenerateResponse, ApiError> {
        if req.messages.is_empty() {
            return Err(invalid("messages", "at least one message"));
        }
        // loaded per call: deleting or editing either resource takes effect immediately (SEC-18)
        let claim = admit_claim(&self.store.get(ns, "ModelClaim", claim_name).await?.spec)?;
        let conn = admit_connection(&self.store.get(ns, "ProviderConnection", &claim.provider_connection).await?.spec)?;
        if !conn.allowed_models.contains(&claim.model) {
            return Err(ApiError::new(
                ErrorCode::Forbidden,
                format!("model {} is not in allowedModels of ProviderConnection {}", claim.model, claim.provider_connection),
            ));
        }
        let max_out = req.max_output_tokens.unwrap_or(claim.limits.max_output_tokens).min(claim.limits.max_output_tokens);
        let est_in = estimate_input(&req);
        if let Some(max_in) = claim.limits.max_input_tokens
            && est_in > i64::from(max_in)
        {
            return Err(invalid("messages", format!("~{est_in} input tokens exceeds the claim's maxInputTokens {max_in}")));
        }

        let reservation = est_in + i64::from(max_out);
        let buckets = self.buckets(ns, claim_name, &claim.budget, req.run_id).await?;
        let price = conn.pricing.get(&claim.model).copied();
        if price.is_none() && buckets.iter().any(|b| b.cost) {
            return Err(ApiError::new(
                ErrorCode::Forbidden,
                format!("a cost budget applies but ProviderConnection {} has no pricing for {}", claim.provider_connection, claim.model),
            ));
        }
        let cost_reservation = price.map_or(0, |p| p.micro_usd(est_in, i64::from(max_out)));
        self.reserve(&buckets, ns, reservation, cost_reservation).await?;

        let api_key = match &conn.secret_ref {
            Some(r) => Some(self.store.secret_read(ctx, &self.master, ns, &r.name, &r.key, &format!("model gateway: ModelClaim {claim_name}")).await?),
            None => None,
        };
        let started = Instant::now();
        let result = providers::call(&self.http, &conn, &claim.model, api_key.as_deref(), &req, max_out).await;
        let latency = started.elapsed().as_millis() as i64;

        let span = tracing::Span::current();
        span.record("provider", serde_json::to_value(conn.provider).unwrap().as_str().unwrap_or_default());
        span.record("model", claim.model.as_str());
        let (usage, outcome, stop) = match &result {
            Ok(r) => (r.usage.clone(), "ok".to_owned(), r.stop_reason.clone()),
            // ponytail: a failed call releases its reservation; providers that bill partial failures will under-count
            Err(e) => (Usage::default(), format!("error:{:?}", e.code), None),
        };
        span.record("input_tokens", usage.input_tokens);
        span.record("output_tokens", usage.output_tokens);
        span.record("outcome", outcome.as_str());
        let cost = price.map(|p| p.micro_usd(usage.input_tokens, usage.output_tokens));
        self.settle(&buckets, ns, (reservation, usage.input_tokens + usage.output_tokens), (cost_reservation, cost.unwrap_or(0))).await?;
        sqlx::query(
            "insert into model_calls (namespace, claim, connection, provider, model, run_id, task, actor, input_tokens, output_tokens, latency_ms, outcome, stop_reason, cost_micro_usd)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(ns)
        .bind(claim_name)
        .bind(&claim.provider_connection)
        .bind(serde_json::to_value(conn.provider).unwrap().as_str().unwrap_or_default())
        .bind(&claim.model)
        .bind(req.run_id)
        .bind(&req.task)
        .bind(&ctx.actor)
        .bind(usage.input_tokens)
        .bind(usage.output_tokens)
        .bind(latency)
        .bind(&outcome)
        .bind(stop)
        .bind(cost)
        .execute(self.store.pool())
        .await
        .map_err(clankr_store::db)?;

        let (text, stop_reason, usage) = result.map(|r| (r.text, r.stop_reason, r.usage))?;
        Ok(GenerateResponse { text, stop_reason, usage, provider: conn.provider, model: claim.model, connection: claim.provider_connection })
    }

    /// Every counter this call must fit: the claim's own limits and each AgentBudget in the
    /// namespace (current calendar window). Sorted, so concurrent reservations lock rows in the
    /// same order and can't deadlock.
    async fn buckets(&self, ns: &str, claim: &str, b: &Budget, run: Option<Uuid>) -> Result<Vec<Bucket>, ApiError> {
        let mut out = vec![];
        let mut add = |key: String, scope: String, max: Option<i64>, cost: bool, what: String| {
            if let Some(max) = max {
                out.push(Bucket { key, scope, max, cost, what });
            }
        };
        let usd = |v: Option<f64>| v.map(clankr_types::quota::micro_usd);
        add(claim.into(), "total".into(), b.max_tokens_total, false, format!("ModelClaim {claim}: total token budget"));
        add(format!("cost:{claim}"), "total".into(), usd(b.max_cost_usd_total), true, format!("ModelClaim {claim}: total cost budget"));
        if let Some(run) = run {
            add(claim.into(), run.to_string(), b.max_tokens_per_run, false, format!("ModelClaim {claim}: per-run token budget (run {run})"));
            add(format!("cost:{claim}"), run.to_string(), usd(b.max_cost_usd_per_run), true, format!("ModelClaim {claim}: per-run cost budget (run {run})"));
        }
        let now = chrono::Utc::now();
        for r in self.store.list("AgentBudget", Some(ns), &Default::default()).await?.0 {
            let spec = clankr_types::quota::admit_agent_budget(&r.spec)?;
            let (name, window) = (&r.metadata.name, spec.period.window(now));
            add(format!("budget:{name}"), window.clone(), spec.max_tokens, false, format!("AgentBudget {name}: token budget for {window}"));
            add(format!("budget-cost:{name}"), window.clone(), usd(spec.max_cost_usd), true, format!("AgentBudget {name}: cost budget for {window}"));
        }
        out.sort_by(|a, b| (&a.key, &a.scope).cmp(&(&b.key, &b.scope)));
        Ok(out)
    }

    /// Atomically reserves in every bucket (tokens or micro-USD), or in none.
    async fn reserve(&self, buckets: &[Bucket], ns: &str, tokens: i64, cost: i64) -> Result<(), ApiError> {
        let mut tx = self.store.pool().begin().await.map_err(clankr_store::db)?;
        for b in buckets {
            let amount = if b.cost { cost } else { tokens };
            sqlx::query("insert into model_usage (namespace, claim, scope) values ($1, $2, $3) on conflict do nothing")
                .bind(ns)
                .bind(&b.key)
                .bind(&b.scope)
                .execute(&mut *tx)
                .await
                .map_err(clankr_store::db)?;
            let ok = sqlx::query(
                "update model_usage set reserved = reserved + $4
                 where namespace = $1 and claim = $2 and scope = $3 and used + reserved + $4 <= $5",
            )
            .bind(ns)
            .bind(&b.key)
            .bind(&b.scope)
            .bind(amount)
            .bind(b.max)
            .execute(&mut *tx)
            .await
            .map_err(clankr_store::db)?
            .rows_affected();
            if ok == 0 {
                let (limit, need) = if b.cost {
                    (format!("${:.2}", b.max as f64 / 1e6), format!("~${:.4} reserved", amount as f64 / 1e6))
                } else {
                    (b.max.to_string(), format!("~{amount} tokens reserved"))
                };
                return Err(ApiError::new(ErrorCode::QuotaExceeded, format!("{} {limit} can't fit this request ({need})", b.what)));
            }
        }
        tx.commit().await.map_err(clankr_store::db)
    }

    /// Releases reservations and charges actual usage: `(reserved, used)` for tokens and cost.
    async fn settle(&self, buckets: &[Bucket], ns: &str, tokens: (i64, i64), cost: (i64, i64)) -> Result<(), ApiError> {
        let mut tx = self.store.pool().begin().await.map_err(clankr_store::db)?;
        for b in buckets {
            let (reserved, used) = if b.cost { cost } else { tokens };
            sqlx::query("update model_usage set reserved = reserved - $4, used = used + $5 where namespace = $1 and claim = $2 and scope = $3")
                .bind(ns)
                .bind(&b.key)
                .bind(&b.scope)
                .bind(reserved)
                .bind(used)
                .execute(&mut *tx)
                .await
                .map_err(clankr_store::db)?;
        }
        tx.commit().await.map_err(clankr_store::db)
    }
}
