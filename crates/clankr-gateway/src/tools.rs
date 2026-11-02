//! Tool gateway (PRD §28, CL-28-01/02/06/07/08/09/10). Agents call tools through a ToolClaim;
//! the gateway checks the grant, validates input against the method's schema, demands an
//! approval for high-risk methods, deduplicates by idempotency key, brokers the credential,
//! bounds and sanitizes the output, and records every call for audit and replay.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use clankr_store::{AuditTarget, audit_in_tx, db};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::sandbox::{Container, admit_container, mcp_http};
use crate::{Gateway, SecretRef};

/// `kind: Tool`: a versioned capability with typed methods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolSpec {
    pub version: String,
    pub executor: Executor,
    pub methods: BTreeMap<String, MethodSpec>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
}

fn default_timeout() -> u64 {
    30
}
fn default_max_output() -> usize {
    64 * 1024
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Executor {
    /// POSTs `{method, input, idempotencyKey, runId, task}` to `url`; the credential (if any)
    /// is sent as `header: <secret value>` and never reaches the agent.
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_ref: Option<SecretRef>,
        #[serde(default = "default_header")]
        header: String,
    },
    /// Fixed outputs per method (tests, evaluations, dry runs). `{"$error": "..."}` fails.
    Mock { responses: BTreeMap<String, Value> },
    /// A fresh container per call (no network): `{method, input, idempotencyKey, runId, task}`
    /// as JSON on stdin, the result on stdout; exit non-zero to fail. Shell and code runners.
    Sandbox { container: Container },
    /// An MCP server over streamable HTTP; each method is the MCP tool of the same name.
    McpHttp {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_ref: Option<SecretRef>,
        #[serde(default = "default_header")]
        header: String,
    },
    /// A stdio MCP server, started in a fresh sandbox container for each call.
    McpStdio { container: Container },
}

fn default_header() -> String {
    "authorization".into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Effects {
    ReadOnly,
    Idempotent,
    /// Default: a lost answer can't be retried blindly (the effect may have happened).
    #[default]
    Unsafe,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Risk {
    #[default]
    Low,
    /// Publishing, payments, deletions, credential changes: needs an approval (CL-28-07).
    High,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MethodSpec {
    /// JSON Schema subset: type, properties, required, additionalProperties, items, enum.
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub side_effects: Effects,
    #[serde(default)]
    pub risk: Risk,
}

/// `kind: ToolClaim`: what an agent bound to it may call. Discovery is not permission (CL-28-02).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolClaimSpec {
    #[serde(default)]
    pub allow: Vec<Grant>,
    /// Wins over allow.
    #[serde(default)]
    pub deny: Vec<Grant>,
    /// `tool.method` entries that need an approval even when the method isn't high-risk.
    #[serde(default)]
    pub approval_required: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls_per_run: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    pub tool: String,
    /// Empty: every method.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Empty: any version.
    #[serde(default)]
    pub versions: Vec<String>,
}

impl Grant {
    fn covers(&self, tool: &str, method: &str) -> bool {
        self.tool == tool && (self.methods.is_empty() || self.methods.iter().any(|m| m == method))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCall {
    pub tool: String,
    pub method: String,
    #[serde(default)]
    pub input: Value,
    /// Same key, same call: the recorded outcome comes back instead of a second execution.
    pub idempotency_key: String,
    #[serde(default)]
    pub run_id: Option<Uuid>,
    #[serde(default)]
    pub task: Option<String>,
    /// A specific approval to spend; by default the run's newest unused granted approval.
    #[serde(default)]
    pub approval: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub output: Value,
    pub output_sha256: String,
    pub truncated: bool,
    pub duration_ms: i64,
    pub tool_version: String,
    /// True when this is the recorded outcome of an earlier call with the same key.
    pub replayed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Uuid>,
}

fn invalid(field: &str, msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::InvalidSpec, msg).field(field)
}

pub fn admit_tool(spec: &Value) -> Result<ToolSpec, ApiError> {
    let s: ToolSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("tool: {e}")))?;
    if s.version.is_empty() {
        return Err(invalid("spec.version", "set a version (tools are immutable per version)"));
    }
    if s.methods.is_empty() {
        return Err(invalid("spec.methods", "declare at least one method"));
    }
    match &s.executor {
        Executor::Http { url, .. } | Executor::McpHttp { url, .. } => crate::check_endpoint(url).map_err(|e| invalid("spec.executor.url", e.message))?,
        Executor::Sandbox { container } | Executor::McpStdio { container } => admit_container(container).map_err(|e| invalid("spec.executor.container", e))?,
        Executor::Mock { .. } => {}
    }
    if !(1..=600).contains(&s.timeout_seconds) || !(1..=16 << 20).contains(&s.max_output_bytes) {
        return Err(invalid("spec", "timeoutSeconds 1..600, maxOutputBytes 1..16MiB"));
    }
    Ok(s)
}

pub fn admit_tool_claim(spec: &Value) -> Result<ToolClaimSpec, ApiError> {
    let s: ToolClaimSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("tool claim: {e}")))?;
    if s.allow.is_empty() {
        return Err(invalid("spec.allow", "grant at least one tool"));
    }
    if let Some(bad) = s.approval_required.iter().find(|x| !x.contains('.')) {
        return Err(invalid("spec.approvalRequired", format!("{bad:?}: want tool.method")));
    }
    Ok(s)
}

/// Validates `v` against the schema subset; returns the first problem as `path: message`.
pub fn validate(schema: &Value, v: &Value, path: &str) -> Result<(), String> {
    if schema.is_null() || schema.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(());
    }
    if let Some(t) = schema["type"].as_str() {
        let ok = match t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            _ => return Err(format!("{path}: schema type {t:?} not supported")),
        };
        if !ok {
            return Err(format!("{path}: expected {t}"));
        }
    }
    if let Some(choices) = schema["enum"].as_array()
        && !choices.contains(v)
    {
        return Err(format!("{path}: not one of {}", Value::Array(choices.clone())));
    }
    if let Some(obj) = v.as_object() {
        for r in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !obj.contains_key(r) {
                return Err(format!("{path}.{r}: required"));
            }
        }
        let props = schema["properties"].as_object();
        for (k, x) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(sub) => validate(sub, x, &format!("{path}.{k}"))?,
                None if schema["additionalProperties"] == false => return Err(format!("{path}.{k}: not allowed (undeclared input)")),
                None => {}
            }
        }
    }
    if let (Some(items), Some(arr)) = (schema.get("items"), v.as_array()) {
        for (i, x) in arr.iter().enumerate() {
            validate(items, x, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

/// Strips terminal control sequences from every string (keeps \n and \t) so tool output can't
/// drive an operator's terminal or the UI (CL-28-08).
pub fn sanitize(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(sanitize).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, x)| (k, sanitize(x))).collect()),
        x => x,
    }
}

fn sha(v: &Value) -> String {
    hex::encode(Sha256::digest(v.to_string().as_bytes()))
}

/// Bounds output to `max` bytes of JSON; over that, keeps a sanitized preview and flags it.
fn bound(v: Value, max: usize) -> (Value, bool) {
    let v = sanitize(v);
    let text = v.to_string();
    if text.len() <= max {
        return (v, false);
    }
    let cut = (0..=max.min(text.len())).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
    (json!({"truncated": true, "bytes": text.len(), "preview": &text[..cut]}), true)
}

impl Gateway {
    /// One tool call, end to end. See the module doc for the checks, in order.
    #[tracing::instrument(name = "tool.call", skip(self, ctx, call), fields(claim, tool = %call.tool, method = %call.method, actor = %ctx.actor, outcome = tracing::field::Empty))]
    pub async fn call_tool(&self, ctx: &RequestCtx, ns: &str, claim_name: &str, call: ToolCall) -> Result<ToolResult, ApiError> {
        let claim = admit_tool_claim(&self.store.get(ns, "ToolClaim", claim_name).await?.spec)?;
        let forbid = |m: String| ApiError::new(ErrorCode::Forbidden, m);
        if claim.deny.iter().any(|g| g.covers(&call.tool, &call.method)) {
            return Err(forbid(format!("ToolClaim {claim_name} denies {}.{}", call.tool, call.method)));
        }
        let grant = claim.allow.iter().find(|g| g.covers(&call.tool, &call.method)).ok_or_else(|| forbid(format!("ToolClaim {claim_name} doesn't grant {}.{}", call.tool, call.method)))?;
        let tool = admit_tool(&self.store.get(ns, "Tool", &call.tool).await?.spec)?;
        if !grant.versions.is_empty() && !grant.versions.contains(&tool.version) {
            return Err(forbid(format!("ToolClaim {claim_name} grants {} versions {}, not {}", call.tool, grant.versions.join(", "), tool.version)));
        }
        let method = tool.methods.get(&call.method).ok_or_else(|| invalid("method", format!("{} has no method {:?}", call.tool, call.method)))?;
        validate(&method.input, &call.input, "$.input").map_err(|e| invalid("input", e))?;
        if call.idempotency_key.is_empty() || call.idempotency_key.len() > 200 {
            return Err(invalid("idempotencyKey", "required, at most 200 characters"));
        }
        let input_sha = sha(&json!({"tool": call.tool, "method": call.method, "input": call.input}));

        // a key we've seen: the recorded outcome, never a second execution
        if let Some(prev) = self.recorded(ns, claim_name, &call.idempotency_key).await? {
            return prev.into_result(&input_sha);
        }
        if let (Some(max), Some(run)) = (claim.max_calls_per_run, call.run_id) {
            let n: i64 = sqlx::query_scalar("select count(*) from tool_calls where namespace = $1 and claim = $2 and run_id = $3")
                .bind(ns)
                .bind(claim_name)
                .bind(run)
                .fetch_one(self.store.pool())
                .await
                .map_err(db)?;
            if n >= max {
                return Err(ApiError::new(ErrorCode::QuotaExceeded, format!("ToolClaim {claim_name}: run {run} already made {n} calls (maxCallsPerRun {max})")));
            }
        }
        let needs_approval = method.risk == Risk::High || claim.approval_required.iter().any(|x| *x == format!("{}.{}", call.tool, call.method));
        let approval = if needs_approval { Some(self.approval_for(ns, &call).await?) } else { None };

        // claim the key (and the approval) before executing; a racing duplicate loses here
        let id: Option<i64> = sqlx::query_scalar(
            "insert into tool_calls (namespace, claim, tool, tool_version, method, idempotency_key, actor, run_id, task, approval_id, input_sha256, state)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'started')
             on conflict do nothing returning id",
        )
        .bind(ns)
        .bind(claim_name)
        .bind(&call.tool)
        .bind(&tool.version)
        .bind(&call.method)
        .bind(&call.idempotency_key)
        .bind(&ctx.actor)
        .bind(call.run_id)
        .bind(&call.task)
        .bind(approval)
        .bind(&input_sha)
        .fetch_optional(self.store.pool())
        .await
        .map_err(db)?;
        let Some(id) = id else {
            return match self.recorded(ns, claim_name, &call.idempotency_key).await? {
                Some(prev) => prev.into_result(&input_sha),
                None => Err(forbid("that approval was already used for another call".into())),
            };
        };

        let started = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(tool.timeout_seconds), self.execute(ctx, ns, &tool, &call)).await;
        let ms = started.elapsed().as_millis() as i64;
        let (state, output, err) = match outcome {
            Ok(Ok(v)) => ("done", Some(v), None),
            // timed out: did the effect happen? for unsafe methods we can't know (OutcomeUnknown)
            Err(_) if method.side_effects == Effects::Unsafe => ("unknown", None, Some(ApiError::new(ErrorCode::OutcomeUnknown, format!("{}.{} timed out after {}s; it may or may not have taken effect", call.tool, call.method, tool.timeout_seconds)))),
            Err(_) => ("failed", None, Some(ApiError::new(ErrorCode::ProviderUnavailable, format!("{}.{} timed out", call.tool, call.method)))),
            Ok(Err(e)) => ("failed", None, Some(e)),
        };
        tracing::Span::current().record("outcome", state);
        let (output, truncated) = output.map_or((Value::Null, false), |v| bound(v, tool.max_output_bytes));
        let output_sha = (state == "done").then(|| sha(&output));

        let mut tx = self.store.pool().begin().await.map_err(db)?;
        // a failed call frees its key (and approval) so a retry can run; done/unknown stay recorded
        if state == "failed" {
            sqlx::query("delete from tool_calls where id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
        } else {
            sqlx::query("update tool_calls set state = $2, output = $3, output_sha256 = $4, truncated = $5, duration_ms = $6, error = $7 where id = $1")
                .bind(id)
                .bind(state)
                .bind(&output)
                .bind(&output_sha)
                .bind(truncated)
                .bind(ms)
                .bind(err.as_ref().map(|e| e.message.clone()))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        if needs_approval || method.side_effects == Effects::Unsafe {
            let target = AuditTarget { kind: "Tool", namespace: ns, name: &call.tool, uid: None };
            let reason = approval.map(|a| format!("approval {a}"));
            audit_in_tx(&mut tx, ctx, &format!("tool {}.{} v{} {state}", call.tool, call.method, tool.version), Some(target), state != "failed", reason.as_deref(), None).await?;
        }
        tx.commit().await.map_err(db)?;
        if let Some(e) = err {
            return Err(e);
        }
        Ok(ToolResult { output, output_sha256: output_sha.unwrap_or_default(), truncated, duration_ms: ms, tool_version: tool.version, replayed: false, approval })
    }

    /// The approval a high-risk call spends: the named one, or the run's newest granted one
    /// not yet used by another call. Approvals belong to runs, so calls outside a run can't spend one.
    async fn approval_for(&self, ns: &str, call: &ToolCall) -> Result<Uuid, ApiError> {
        let need = || ApiError::new(ErrorCode::Forbidden, format!("{}.{} needs an approval from an editor in this run first", call.tool, call.method));
        let run = call.run_id.ok_or_else(need)?;
        let id: Option<Uuid> = sqlx::query_scalar(
            "select a.id from approvals a where a.namespace = $1 and a.run_id = $2 and a.state = 'approved' and ($3::uuid is null or a.id = $3)
               and not exists (select 1 from tool_calls t where t.approval_id = a.id)
             order by a.decided_at desc limit 1",
        )
        .bind(ns)
        .bind(run)
        .bind(call.approval)
        .fetch_optional(self.store.pool())
        .await
        .map_err(db)?;
        id.ok_or_else(need)
    }

    fn sandbox(&self) -> Result<&crate::sandbox::Sandbox, ApiError> {
        self.sandbox.as_ref().ok_or_else(|| ApiError::new(ErrorCode::ProviderUnavailable, "sandboxed tools aren't enabled on this control plane (start clankrd with --tool-kennel)"))
    }

    /// Re-read per call: a policy change applies to the very next tool run.
    async fn image_policies(&self, ns: &str) -> Result<Vec<(String, clankr_types::agent::ImagePolicySpec)>, ApiError> {
        self.store
            .list("ImagePolicy", Some(ns), &Default::default())
            .await?
            .0
            .into_iter()
            .map(|p| Ok((p.metadata.name.clone(), clankr_types::agent::admit_image_policy(&p.spec)?)))
            .collect()
    }

    async fn recorded(&self, ns: &str, claim: &str, key: &str) -> Result<Option<Recorded>, ApiError> {
        sqlx::query_as("select state, input_sha256, output, output_sha256, truncated, duration_ms, tool_version, approval_id from tool_calls where namespace = $1 and claim = $2 and idempotency_key = $3")
            .bind(ns)
            .bind(claim)
            .bind(key)
            .fetch_optional(self.store.pool())
            .await
            .map_err(db)
    }

    async fn execute(&self, ctx: &RequestCtx, ns: &str, tool: &ToolSpec, call: &ToolCall) -> Result<Value, ApiError> {
        match &tool.executor {
            Executor::Mock { responses } => match responses.get(&call.method) {
                Some(v) if v.get("$error").is_some() => Err(ApiError::new(ErrorCode::ProviderUnavailable, v["$error"].as_str().unwrap_or("tool error").to_owned())),
                Some(v) => Ok(v.clone()),
                None => Err(ApiError::new(ErrorCode::InvalidSpec, format!("mock tool has no response for {}", call.method))),
            },
            Executor::Sandbox { container } => {
                let request = json!({"method": call.method, "input": call.input, "idempotencyKey": call.idempotency_key, "runId": call.run_id, "task": call.task});
                self.sandbox()?.run(container, &self.image_policies(ns).await?, &request).await
            }
            Executor::McpStdio { container } => self.sandbox()?.mcp(container, &self.image_policies(ns).await?, &call.method, &call.input).await,
            Executor::McpHttp { url, secret_ref, header } => {
                let secret = match secret_ref {
                    Some(r) => Some(self.store.secret_read(ctx, &self.master, ns, &r.name, &r.key, &format!("tool gateway: {}", call.tool)).await?),
                    None => None,
                };
                mcp_http(&self.http, url, secret.as_deref().map(|v| (header.as_str(), v)), &call.method, &call.input).await
            }
            Executor::Http { url, secret_ref, header } => {
                let mut rb = self.http.post(url).json(&json!({"method": call.method, "input": call.input, "idempotencyKey": call.idempotency_key, "runId": call.run_id, "task": call.task}));
                if let Some(r) = secret_ref {
                    let secret = self.store.secret_read(ctx, &self.master, ns, &r.name, &r.key, &format!("tool gateway: {}", call.tool)).await?;
                    rb = rb.header(header.as_str(), secret);
                }
                let resp = rb.send().await.map_err(|e| ApiError::new(ErrorCode::ProviderUnavailable, format!("{}: {}", call.tool, e.without_url())))?;
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                let v = serde_json::from_str(&body).unwrap_or(Value::String(body));
                if status.is_success() {
                    Ok(v)
                } else {
                    let msg: String = v["error"].as_str().or(v.as_str()).unwrap_or("error").chars().take(300).collect();
                    let code = if status.is_client_error() { ErrorCode::InvalidSpec } else { ErrorCode::ProviderUnavailable };
                    Err(ApiError::new(code, format!("{} returned {status}: {msg}", call.tool)))
                }
            }
        }
    }
}

#[derive(sqlx::FromRow)]
struct Recorded {
    state: String,
    input_sha256: String,
    output: Option<Value>,
    output_sha256: Option<String>,
    truncated: bool,
    duration_ms: Option<i64>,
    tool_version: String,
    approval_id: Option<Uuid>,
}

impl Recorded {
    fn into_result(self, input_sha: &str) -> Result<ToolResult, ApiError> {
        if self.input_sha256 != input_sha {
            return Err(ApiError::new(ErrorCode::Conflict, "idempotency key already used for a different call"));
        }
        match self.state.as_str() {
            "done" => Ok(ToolResult {
                output: self.output.unwrap_or(Value::Null),
                output_sha256: self.output_sha256.unwrap_or_default(),
                truncated: self.truncated,
                duration_ms: self.duration_ms.unwrap_or(0),
                tool_version: self.tool_version,
                replayed: true,
                approval: self.approval_id,
            }),
            "unknown" => Err(ApiError::new(ErrorCode::OutcomeUnknown, "an earlier call with this key timed out; whether it took effect is unknown")),
            _ => Err(ApiError::new(ErrorCode::Conflict, "a call with this key is still in progress")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_subset_and_output_bounds() {
        let schema = json!({"type": "object", "required": ["url"], "additionalProperties": false,
            "properties": {"url": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}, "mode": {"enum": ["draft", "live"]}}});
        assert!(validate(&schema, &json!({"url": "x", "tags": ["a"], "mode": "draft"}), "$").is_ok());
        assert_eq!(validate(&schema, &json!({}), "$").unwrap_err(), "$.url: required");
        assert_eq!(validate(&schema, &json!({"url": "x", "evil": 1}), "$").unwrap_err(), "$.evil: not allowed (undeclared input)");
        assert_eq!(validate(&schema, &json!({"url": "x", "tags": [1]}), "$").unwrap_err(), "$.tags[0]: expected string");
        assert!(validate(&schema, &json!({"url": "x", "mode": "loud"}), "$").unwrap_err().contains("not one of"));
        assert_eq!(sanitize(json!({"t": "ok\u{1b}[31mred\u{7}\nnext"})), json!({"t": "ok[31mred\nnext"}));
        let (v, cut) = bound(json!({"text": "é".repeat(100)}), 50);
        assert!(cut && v["truncated"] == true && v["preview"].as_str().unwrap().len() <= 50);
        assert!(!bound(json!({"a": 1}), 50).1);
    }
}
