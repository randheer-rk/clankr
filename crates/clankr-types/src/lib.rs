//! Resource envelope and stable API error codes (PRD §4.1, §19, CL-35-06).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod agent;
pub mod chat;
pub mod federation;
pub mod protocol;
pub mod quota;
pub mod rbac;
pub mod schedule;

pub const API_VERSION: &str = "clankr.io/v1alpha1";

/// Every stored object: Agent, Workflow, ModelClaim, ... Spec stays untyped JSON here;
/// each controller deserializes the spec of the kinds it owns.
// ponytail: one generic envelope; typed per-kind specs arrive with the controller that needs them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub spec: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Value>,
}

/// Fields marked server-set are ignored on create and filled by the store.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub name: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// server-set
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// server-set; bumps only when spec changes
    #[serde(default)]
    pub generation: i64,
    /// server-set; cluster-wide monotonic revision of the last write (watch cursor)
    #[serde(default)]
    pub resource_version: i64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finalizers: Vec<String>,
    /// server-set
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_timestamp: Option<DateTime<Utc>>,
    /// server-set; present while finalizers block deletion
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_timestamp: Option<DateTime<Utc>>,
}

fn default_namespace() -> String {
    "default".into()
}

/// Stable machine-readable error codes (CL-35-06). Clients branch on these, never on messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    InvalidSpec,
    NotFound,
    AlreadyExists,
    Conflict,
    Forbidden,
    Unauthenticated,
    StaleLease,
    QuotaExceeded,
    UnsupportedCapability,
    OutcomeUnknown,
    StorageUnavailable,
    /// An upstream model/tool provider failed or is unreachable.
    ProviderUnavailable,
    /// Watch cursor is older than retained history; client must relist.
    HistoryExpired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// JSON path of the offending field, for InvalidSpec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), field: None }
    }
    pub fn field(mut self, path: impl Into<String>) -> Self {
        self.field = Some(path.into());
        self
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)?;
        if let Some(p) = &self.field {
            write!(f, " (at {p})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// Who is acting and under which request; carried into every audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestCtx {
    pub actor: String,
    pub request_id: String,
}

/// One entry in the watch stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchEvent {
    #[serde(rename = "type")]
    pub kind: WatchEventType,
    pub object: Resource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchEventType {
    Added,
    Modified,
    Deleted,
}

/// Kinds the API accepts (PRD §4.2, §19). Anything else is rejected at admission.
// ponytail: every kind is namespaced for now; cluster-scoped kinds (Node, StorageClass, ...) get their own scope when a controller needs it.
pub const KINDS: &[&str] = &[
    "Agent", "AgentRevision", "AgentDeployment", "AgentPool", "AgentJob", "AgentCronJob", "AgentSession",
    "AgentIdentity", "AgentSupervisor", "AgentGroup", "AgentCapability", "AgentService", "AgentNetwork",
    "AgentMailbox", "AgentLease", "Workflow", "WorkflowRun", "AgentCheckpoint", "ApprovalTask", "EventTrigger",
    "EventSubscription", "TaskQueue", "StorageClass", "PersistentVolume", "VolumeClaim", "VolumeBinding",
    "VolumeSnapshot", "ContextStore", "ContextClaim", "ArtifactStore", "StorageQuota", "ComputeClaim",
    "ModelClaim", "ToolClaim", "Tool", "BrowserClaim", "NetworkClaim", "DatabaseClaim", "QueueClaim",
    "SecretClaim", "AgentSecret", "AgentConfig", "AgentPolicy", "AgentBudget", "ResourceQuota",
    "PriorityClass", "Node", "AgentImage", "AgentBuild", "BuilderPool", "ImageRegistry", "BuildPolicy",
    "ImagePolicy", "SkillBundle", "ProviderConnection", "AgentRuntimeClass", "SecurityPolicy",
    "NetworkPolicy", "ToolPolicy", "RoleBinding", "ServiceAccount", "Cluster", "PlacementPolicy", "ChatConnector", "Webhook",
];

/// URL path segment for a kind: `AgentPolicy` -> `agentpolicies`, `AgentMailbox` -> `agentmailboxes`.
pub fn plural(kind: &str) -> String {
    let k = kind.to_ascii_lowercase();
    if let Some(stem) = k.strip_suffix('y') {
        format!("{stem}ies")
    } else if k.ends_with('s') || k.ends_with('x') {
        format!("{k}es")
    } else {
        format!("{k}s")
    }
}

/// Inverse of [`plural`]; also accepts the singular lowercase kind (`clank get agent`).
pub fn kind_for(segment: &str) -> Option<&'static str> {
    let s = segment.to_ascii_lowercase();
    KINDS.iter().copied().find(|k| plural(k) == s || k.to_ascii_lowercase() == s)
}

/// Validates the parts of a manifest that are kind-independent.
pub fn validate_envelope(r: &Resource) -> Result<(), ApiError> {
    use ErrorCode::InvalidSpec;
    if r.api_version != API_VERSION {
        return Err(ApiError::new(InvalidSpec, format!("unsupported apiVersion {:?}, want {API_VERSION}", r.api_version)).field("apiVersion"));
    }
    if !KINDS.contains(&r.kind.as_str()) {
        return Err(ApiError::new(InvalidSpec, format!("unknown kind {:?}", r.kind)).field("kind"));
    }
    for (path, v) in [("metadata.name", &r.metadata.name), ("metadata.namespace", &r.metadata.namespace)] {
        if !is_dns_label(v) {
            return Err(ApiError::new(InvalidSpec, format!("{v:?} is not a DNS label (a-z0-9-, max 63)")).field(path));
        }
    }
    Ok(())
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = r#"
        {"apiVersion":"clankr.io/v1alpha1","kind":"Agent",
         "metadata":{"name":"researcher","namespace":"research","labels":{"team":"x"}},
         "spec":{"image":"registry.example/researcher@sha256:abc"}}"#;

    #[test]
    fn golden_manifest_roundtrips_and_validates() {
        let r: Resource = serde_json::from_str(AGENT).unwrap();
        validate_envelope(&r).unwrap();
        assert_eq!(r.metadata.labels["team"], "x");
        let back: Resource = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rejects_bad_envelopes_with_field_paths() {
        let mut r: Resource = serde_json::from_str(AGENT).unwrap();
        r.api_version = "clankr.io/v1".into();
        assert_eq!(validate_envelope(&r).unwrap_err().field.as_deref(), Some("apiVersion"));
        let mut r: Resource = serde_json::from_str(AGENT).unwrap();
        r.metadata.name = "Bad_Name".into();
        assert_eq!(validate_envelope(&r).unwrap_err().field.as_deref(), Some("metadata.name"));
    }

    #[test]
    fn plurals_roundtrip_for_every_kind() {
        assert_eq!(plural("AgentPolicy"), "agentpolicies");
        assert_eq!(plural("AgentMailbox"), "agentmailboxes");
        assert_eq!(plural("StorageClass"), "storageclasses");
        for k in KINDS {
            assert_eq!(kind_for(&plural(k)), Some(*k));
        }
        assert_eq!(kind_for("agent"), Some("Agent"));
        assert_eq!(kind_for("nope"), None);
    }

    #[test]
    fn namespace_defaults() {
        let r: Resource = serde_json::from_str(r#"{"apiVersion":"clankr.io/v1alpha1","kind":"Agent","metadata":{"name":"a"}}"#).unwrap();
        assert_eq!(r.metadata.namespace, "default");
    }
}
