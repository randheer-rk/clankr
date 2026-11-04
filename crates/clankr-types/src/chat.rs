//! `ChatConnector`: a bridge between a chat app (Telegram, Slack) and the agents in a namespace.
//! Inbound messages become service calls; replies go back to the chat; pending approvals are
//! posted as cards whose buttons decide them. Tokens stay in secrets; agents never see them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApiError, ErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatKind {
    Telegram,
    Slack,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatSecretRef {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatConnectorSpec {
    pub kind: ChatKind,
    /// The bot token: Telegram's `123:ABC…`, Slack's `xoxb-…`.
    pub token_ref: ChatSecretRef,
    /// Slack only: the app-level `xapp-…` token for Socket Mode (no public URL needed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_token_ref: Option<ChatSecretRef>,
    /// The API base, for tests and self-hosted proxies (default: the platform's own).
    #[serde(default)]
    pub endpoint: String,
    /// The AgentService that answers inbound messages. Unset: the connector only sends
    /// (outbound tool, approval cards) and inbound messages are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Chats (Telegram chat ids, Slack channel ids) allowed to talk to the service. Empty:
    /// nobody, which is the safe default for a bot anyone can find.
    #[serde(default)]
    pub allowed_chats: Vec<String>,
    /// How long to wait for the service's reply before telling the chat it's still working.
    #[serde(default = "default_reply_timeout")]
    pub reply_timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<ApprovalCards>,
}

/// Pending approvals in the namespace are posted to `chat` with Approve/Reject buttons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalCards {
    pub chat: String,
    /// The approver group the buttons decide for; only approvals waiting on it are posted.
    pub group: String,
    /// Chat user ids allowed to press the buttons. Decisions are recorded under
    /// `<platform>:<user id>`, so each one is traceable to a person.
    pub users: Vec<String>,
}

fn default_reply_timeout() -> u64 {
    120
}

fn invalid(field: &str, m: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::InvalidSpec, m).field(field)
}

pub fn admit_chat_connector(spec: &Value) -> Result<ChatConnectorSpec, ApiError> {
    let s: ChatConnectorSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("chat connector: {e}")))?;
    if s.kind == ChatKind::Slack && s.app_token_ref.is_none() {
        return Err(invalid("spec.appTokenRef", "Slack needs an app-level token (xapp-…) with connections:write for Socket Mode"));
    }
    if !s.endpoint.is_empty() {
        let u = url::Url::parse(&s.endpoint).map_err(|e| invalid("spec.endpoint", format!("bad url: {e}")))?;
        let loopback = matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if !(u.scheme() == "https" || (u.scheme() == "http" && loopback)) {
            return Err(invalid("spec.endpoint", "endpoint must be https (http only for localhost)"));
        }
    }
    if !(5..=600).contains(&s.reply_timeout_seconds) {
        return Err(invalid("spec.replyTimeoutSeconds", "5..600"));
    }
    if let Some(a) = &s.approvals {
        if a.chat.is_empty() || a.group.is_empty() {
            return Err(invalid("spec.approvals", "chat and group are required"));
        }
        if a.users.is_empty() {
            return Err(invalid("spec.approvals.users", "list who may decide; an empty list would let nobody press the buttons"));
        }
    }
    Ok(s)
}
