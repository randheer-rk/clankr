//! Agent runtime ABI v1 (PRD §15.3, §25): frames between the node daemon and an agent process
//! over a per-instance Unix socket. Each frame is a 4-byte big-endian length then that many
//! bytes of JSON. The agent never holds a cluster credential: model and context calls go
//! through the node, which checks them against the Agent spec before forwarding (CL-25-08).
//!
//! Handshake: agent `Hello` → node `Welcome` (or `Reject`) → agent `Ready` → `Deliver`s.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ApiError;

pub const ABI_MAJOR: u16 = 1;
/// 1.1: hibernation (`Hibernate` / `Checkpoint`, checkpoint in `Welcome`).
pub const ABI_MINOR: u16 = 2;
/// Bigger payloads go through the artifact store by reference (CL-25-06).
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// Agent → node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AgentFrame {
    Hello { abi_major: u16, abi_minor: u16, sdk: String },
    Ready,
    Complete { delivery_id: u64, result: Value },
    Fail { delivery_id: u64, error: String, retryable: bool },
    Call { call_id: u64, op: CallOp },
    Log { level: String, message: String },
    /// Answer to `Hibernate`: the app's portable state (None: nothing to keep). The agent exits after.
    Checkpoint { state: Option<Value> },
}

/// Privileged operations an agent may request; the node authorizes each one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum CallOp {
    /// Body is the gateway's GenerateRequest (messages, system, maxOutputTokens).
    Generate { claim: String, request: Value },
    /// A tool call through a ToolClaim; body is `{tool, method, input, idempotencyKey, approval?}`.
    ToolCall { claim: String, call: Value },
    ContextGet { claim: String, key: String },
    ContextPut {
        claim: String,
        key: String,
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<i64>,
    },
    ContextList {
        claim: String,
        #[serde(default)]
        prefix: String,
    },
    MailboxSend {
        mailbox: String,
        payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dedup_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_seconds: Option<i64>,
    },
    MailboxReceive {
        mailbox: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visibility_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_seconds: Option<u64>,
    },
    MailboxAck { mailbox: String, id: String, epoch: i64 },
    /// Request/reply to an AgentService (ABI 1.2); answers `{reply, requestId, from}`.
    ServiceCall {
        service: String,
        payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dedup_key: Option<String>,
    },
    /// The serving agent answers a request it received from `svc:<service>`.
    ServiceReply { service: String, request_id: String, payload: Value },
    /// Publish to a topic: a durable copy into each subscribed mailbox.
    Publish {
        topic: String,
        payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dedup_key: Option<String>,
    },
    MailboxNack {
        mailbox: String,
        id: String,
        epoch: i64,
        #[serde(default)]
        delay_seconds: f64,
        #[serde(default)]
        reason: String,
    },
}

/// Node → agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NodeFrame {
    Welcome {
        abi_major: u16,
        abi_minor: u16,
        namespace: String,
        agent: String,
        instance: String,
        /// ModelClaims and ContextClaims this agent may use.
        models: Vec<String>,
        contexts: Vec<String>,
        /// Resuming from hibernation: the state this slot checkpointed, and its version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint_version: Option<i64>,
    },
    Reject { reason: String },
    Deliver {
        delivery_id: u64,
        run_id: String,
        task: String,
        node: String,
        attempt: i32,
        /// Stable across attempts; pass to external systems to dedupe side effects.
        idempotency_key: String,
        input: Value,
    },
    Reply {
        call_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ApiError>,
    },
    Shutdown { grace_ms: u64 },
    /// Finish (no new work is delivered), then reply `Checkpoint` within `grace_ms` and exit.
    Hibernate { grace_ms: u64 },
}
