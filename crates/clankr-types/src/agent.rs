//! `kind: Agent` spec, shared by admission (clankrd) and the node daemon.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSpec {
    pub runtime: Runtime,
    /// ModelClaims this agent may call. Anything else is refused by the node (capability check).
    #[serde(default)]
    pub models: Vec<String>,
    /// ContextClaims this agent may read/write.
    #[serde(default)]
    pub contexts: Vec<String>,
    /// Lease TTL for this agent's tasks; the node heartbeats at a third of it.
    #[serde(default = "default_ttl")]
    pub task_ttl_seconds: u64,
    /// Ceilings for each instance (CL-32-07); enforced by kennel cgroups for `oci` runtimes.
    #[serde(default)]
    pub resources: Resources,
    /// Persistent volumes mounted into each instance (VolumeClaims in the same namespace).
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// AgentMailboxes this agent may send to and receive from.
    #[serde(default)]
    pub mailboxes: Vec<String>,
}

/// `kind: AgentMailbox` (CL-30-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxSpec {
    #[serde(default = "default_max_deliveries")]
    pub max_deliveries: i32,
    /// Default message TTL.
    #[serde(default = "default_retention")]
    pub retention_seconds: i64,
}

fn default_max_deliveries() -> i32 {
    5
}
fn default_retention() -> i64 {
    86_400
}

pub fn admit_mailbox(spec: &serde_json::Value) -> Result<MailboxSpec, ApiError> {
    let s: MailboxSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("mailbox spec: {e}")).field("spec"))?;
    if !(1..=100).contains(&s.max_deliveries) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "maxDeliveries must be 1..=100").field("spec.maxDeliveries"));
    }
    if !(1..=30 * 86_400).contains(&s.retention_seconds) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "retentionSeconds must be 1..=2592000").field("spec.retentionSeconds"));
    }
    Ok(s)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VolumeMount {
    pub claim: String,
    pub mount_path: String,
    #[serde(default)]
    pub read_only: bool,
}

/// `kind: VolumeClaim` (CL-29-01/02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VolumeClaimSpec {
    #[serde(default = "default_storage_class")]
    pub storage_class: String,
    #[serde(rename = "capacityMiB")]
    pub capacity_mib: u64,
    #[serde(default = "default_access_mode")]
    pub access_mode: String,
    #[serde(default = "default_reclaim")]
    pub reclaim_policy: String,
}

fn default_storage_class() -> String {
    "local-path".into()
}
fn default_access_mode() -> String {
    "ReadWriteOnce".into()
}
fn default_reclaim() -> String {
    "Retain".into()
}

pub fn admit_volume_claim(spec: &serde_json::Value) -> Result<VolumeClaimSpec, ApiError> {
    let s: VolumeClaimSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("volume claim: {e}")).field("spec"))?;
    if s.storage_class != "local-path" {
        return Err(ApiError::new(ErrorCode::UnsupportedCapability, format!("storageClass {:?} isn't available; local-path is", s.storage_class)).field("spec.storageClass"));
    }
    // local disks have no shared-writer semantics (CL-29-04)
    if s.access_mode != "ReadWriteOnce" {
        return Err(ApiError::new(ErrorCode::UnsupportedCapability, format!("local-path supports ReadWriteOnce only, not {}", s.access_mode)).field("spec.accessMode"));
    }
    if s.reclaim_policy != "Retain" {
        return Err(ApiError::new(ErrorCode::UnsupportedCapability, "only reclaimPolicy Retain is supported yet").field("spec.reclaimPolicy"));
    }
    if s.capacity_mib == 0 {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "capacityMiB must be > 0").field("spec.capacityMiB"));
    }
    Ok(s)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resources {
    #[serde(default = "default_memory", rename = "memoryMiB")]
    pub memory_mib: u64,
    #[serde(default = "default_cpus")]
    pub cpus: f64,
    #[serde(default = "default_pids")]
    pub pids: i64,
}

impl Default for Resources {
    fn default() -> Self {
        Self { memory_mib: default_memory(), cpus: default_cpus(), pids: default_pids() }
    }
}

fn default_memory() -> u64 {
    1024
}
fn default_cpus() -> f64 {
    1.0
}
fn default_pids() -> i64 {
    256
}

fn default_ttl() -> u64 {
    60
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Runtime {
    /// S0 trusted local development (PRD §13.4): a plain host process, no sandbox.
    Process {
        command: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
    },
    /// S1+: an OCI image the node runs through `kennel` (sandboxed, rootless).
    Oci { image: String },
}

pub fn admit_agent(spec: &serde_json::Value) -> Result<AgentSpec, ApiError> {
    let s: AgentSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("agent spec: {e}")).field("spec"))?;
    match &s.runtime {
        Runtime::Process { command, .. } if command.is_empty() => {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "process runtime needs a command").field("spec.runtime.command"));
        }
        Runtime::Oci { image } if !image.contains("@sha256:") => {
            // deploy by digest only (CL-24-08); tags are mutable
            return Err(ApiError::new(ErrorCode::InvalidSpec, "image must be pinned by digest (name@sha256:...)").field("spec.runtime.image"));
        }
        _ => {}
    }
    let r = &s.resources;
    if !(16..=1 << 20).contains(&r.memory_mib) || !(0.01..=256.0).contains(&r.cpus) || !(8..=1 << 20).contains(&r.pids) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "resources out of range (memoryMiB 16.., cpus 0.01..256, pids 8..)").field("spec.resources"));
    }
    for (i, v) in s.volumes.iter().enumerate() {
        if !v.mount_path.starts_with('/') || v.mount_path.split('/').any(|c| c == "..") || v.mount_path == "/" {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "mountPath must be an absolute path below /").field(format!("spec.volumes[{i}].mountPath")));
        }
        if s.volumes.iter().filter(|o| o.claim == v.claim).count() > 1 {
            return Err(ApiError::new(ErrorCode::InvalidSpec, format!("claim {} mounted twice", v.claim)).field(format!("spec.volumes[{i}].claim")));
        }
    }
    if !(5..=3600).contains(&s.task_ttl_seconds) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "taskTtlSeconds must be 5..=3600").field("spec.taskTtlSeconds"));
    }
    Ok(s)
}

/// `kind: AgentDeployment`: keep `replicas` instances of an Agent running across nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentSpec {
    pub agent: String,
    pub replicas: u32,
    /// Only nodes whose labels include all of these.
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
}

pub fn admit_deployment(spec: &serde_json::Value) -> Result<DeploymentSpec, ApiError> {
    let s: DeploymentSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("deployment spec: {e}")).field("spec"))?;
    if s.replicas > 1000 {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "replicas must be <= 1000").field("spec.replicas"));
    }
    if s.agent.is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "agent is required").field("spec.agent"));
    }
    Ok(s)
}

/// What a node offers the scheduler (CL-23-01).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeCapacity {
    #[serde(rename = "memoryMiB")]
    pub memory_mib: u64,
    pub cpus: f64,
    /// Runtime types this node can run: "process", "oci".
    pub runtimes: Vec<String>,
}

impl Runtime {
    pub fn kind(&self) -> &'static str {
        match self {
            Runtime::Process { .. } => "process",
            Runtime::Oci { .. } => "oci",
        }
    }
}
