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
    /// ToolClaims this agent may call tools through.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Lease TTL for this agent's tasks; the node heartbeats at a third of it.
    #[serde(default = "default_ttl")]
    pub task_ttl_seconds: u64,
    /// Ceilings for each instance (CL-32-07); enforced by kennel cgroups for `oci` runtimes.
    #[serde(default)]
    pub resources: Resources,
    /// Persistent volumes mounted into each instance (VolumeClaims in the same namespace).
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// Follow a stage of an AgentImage: promoting or rolling back that stage rewrites
    /// `runtime.image` here, and the scheduler restarts instances on the new digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<ImageTrack>,
    /// AgentMailboxes this agent may send to and receive from.
    #[serde(default)]
    pub mailboxes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageTrack {
    /// AgentImage name in the same namespace.
    pub image: String,
    pub stage: String,
}

/// `kind: AgentImage`: an image repository with promotion stages (BLD-17). Digests move
/// through `stages` in order; a stage can require evaluation evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentImageSpec {
    /// e.g. `registry.example/team/researcher` (no tag or digest).
    pub repository: String,
    #[serde(default = "default_stages")]
    pub stages: Vec<String>,
    /// Per-stage requirements for promoting into it.
    #[serde(default)]
    pub gates: BTreeMap<String, StageGate>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StageGate {
    /// Promotion evidence must include an evaluation report with at least this pass rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_eval_pass_rate: Option<f64>,
}

fn default_stages() -> Vec<String> {
    vec!["dev".into(), "staging".into(), "production".into()]
}

pub fn admit_agent_image(spec: &serde_json::Value) -> Result<AgentImageSpec, ApiError> {
    let s: AgentImageSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("agent image: {e}")).field("spec"))?;
    if s.repository.is_empty() || s.repository.contains('@') || s.repository.rsplit('/').next().is_some_and(|last| last.contains(':')) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "repository without tag or digest, e.g. registry.example/team/agent").field("spec.repository"));
    }
    let mut seen = std::collections::BTreeSet::new();
    if s.stages.is_empty() || !s.stages.iter().all(|st| !st.is_empty() && seen.insert(st)) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "stages must be non-empty and unique").field("spec.stages"));
    }
    if let Some(k) = s.gates.keys().find(|k| !s.stages.contains(k)) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, format!("gate for unknown stage {k:?}")).field(format!("spec.gates.{k}")));
    }
    Ok(s)
}

/// `sha256:<64 lowercase hex>`
pub fn is_digest(d: &str) -> bool {
    d.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// `kind: ImagePolicy` (SEC-06/07/19): which images may run in a namespace. Every policy in the
/// namespace applies. The API checks repositories when an Agent is saved; the node re-checks
/// everything, including signatures (via `kennel run --verify-key`), before every launch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImagePolicySpec {
    /// Repository prefixes images must come from, e.g. `registry.example/team/` (empty = any).
    #[serde(default)]
    pub allowed_repositories: Vec<String>,
    /// Images must carry a cosign-format signature from one of `keys`.
    #[serde(default)]
    pub require_signature: bool,
    /// PEM public keys (ECDSA P-256) trusted for signatures.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Allow the unsandboxed `process` runtime (S0, development only) here.
    #[serde(default)]
    pub allow_process_runtime: bool,
    /// OCI runtime classes allowed here (empty = any), e.g. `[microvm]` for untrusted code.
    #[serde(default)]
    pub runtime_classes: Vec<SandboxClass>,
}

pub fn admit_image_policy(spec: &serde_json::Value) -> Result<ImagePolicySpec, ApiError> {
    let s: ImagePolicySpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("image policy: {e}")).field("spec"))?;
    if s.require_signature && s.keys.is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "requireSignature needs at least one public key").field("spec.keys"));
    }
    for (i, k) in s.keys.iter().enumerate() {
        if !(k.contains("-----BEGIN PUBLIC KEY-----") && k.contains("-----END PUBLIC KEY-----")) {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "want a PEM public key (kennel keygen writes <name>.pub)").field(format!("spec.keys[{i}]")));
        }
    }
    Ok(s)
}

/// What launching `runtime` under `policies` requires: the trusted keys a signature must verify
/// with (empty = no signature needed). Err explains which rule refuses it.
pub fn image_allowed(policies: &[(String, ImagePolicySpec)], runtime: &Runtime) -> Result<Vec<String>, String> {
    let mut keys = vec![];
    match runtime {
        Runtime::Process { .. } => {
            if let Some((name, _)) = policies.iter().find(|(_, p)| !p.allow_process_runtime) {
                return Err(format!("ImagePolicy {name} doesn't allow the unsandboxed process runtime"));
            }
        }
        Runtime::Oci { image, class } => {
            if let Some((name, p)) = policies.iter().find(|(_, p)| !p.runtime_classes.is_empty() && !p.runtime_classes.contains(class)) {
                return Err(format!("ImagePolicy {name} requires runtime class {:?}", p.runtime_classes.iter().map(|c| c.kennel_arg()).collect::<Vec<_>>()));
            }
            let repo = image.split('@').next().unwrap_or(image);
            for (name, p) in policies {
                if !p.allowed_repositories.is_empty() && !p.allowed_repositories.iter().any(|pre| repo.starts_with(pre.as_str())) {
                    return Err(format!("ImagePolicy {name} allows only images from {}", p.allowed_repositories.join(", ")));
                }
                if p.require_signature {
                    keys.extend(p.keys.iter().cloned());
                }
            }
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
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
    Oci {
        image: String,
        /// Isolation from the node's kernel; nodes advertise the classes they can run.
        #[serde(default, skip_serializing_if = "SandboxClass::is_container")]
        class: SandboxClass,
    },
}

/// kennel runtime classes, weakest to strongest isolation (PRD §13.4 S1–S3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SandboxClass {
    /// Namespaces, seccomp and cgroups on the host kernel.
    #[default]
    Container,
    /// gVisor's user-space kernel between the workload and the host.
    Gvisor,
    /// A Firecracker microVM with its own kernel.
    Microvm,
}

impl SandboxClass {
    pub fn is_container(&self) -> bool {
        *self == SandboxClass::Container
    }
    /// `kennel run --runtime` value.
    pub fn kennel_arg(self) -> &'static str {
        match self {
            SandboxClass::Container => "container",
            SandboxClass::Gvisor => "gvisor",
            SandboxClass::Microvm => "microvm",
        }
    }
}

pub fn admit_agent(spec: &serde_json::Value) -> Result<AgentSpec, ApiError> {
    let s: AgentSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("agent spec: {e}")).field("spec"))?;
    match &s.runtime {
        Runtime::Process { command, .. } if command.is_empty() => {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "process runtime needs a command").field("spec.runtime.command"));
        }
        Runtime::Oci { image, .. } if !image.contains("@sha256:") => {
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
    #[serde(default)]
    pub rollout: Rollout,
    /// Size the deployment from its queue instead of `replicas`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoscale: Option<Autoscale>,
    /// Checkpoint every instance and release its compute; durable claims stay bound. Setting
    /// it back to false resumes, each slot from its checkpoint.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hibernate: bool,
}

/// Queue-driven scaling: replicas = ceil(tasks ready or in progress / targetTasksPerReplica),
/// clamped to [min, max]. Up immediately; down only after demand stayed lower for the delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Autoscale {
    #[serde(default)]
    pub min_replicas: u32,
    pub max_replicas: u32,
    #[serde(default = "one")]
    pub target_tasks_per_replica: u32,
    #[serde(default = "five_minutes")]
    pub scale_down_delay_seconds: u32,
}

fn five_minutes() -> u32 {
    300
}

/// How an Agent change rolls through a deployment's instances.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rollout {
    /// Instances that may be restarting (or otherwise not ready) at once while rolling.
    #[serde(default = "one")]
    pub max_unavailable: u32,
    /// A restarted instance counts as available after being ready this long.
    #[serde(default = "five")]
    pub min_ready_seconds: u32,
    /// Try a new Agent generation on a few instances first; promote or roll back automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<Canary>,
}

impl Default for Rollout {
    fn default() -> Self {
        Self { max_unavailable: 1, min_ready_seconds: 5, canary: None }
    }
}

/// The rest of the deployment stays on the previous generation while `replicas` canaries run
/// the new one. Promoted once every canary has been ready for `analysisSeconds` (having
/// finished at least `minTasks`); rolled back if they aren't ready within
/// `progressDeadlineSeconds` or more than `maxFailureRatio` of their tasks fail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Canary {
    #[serde(default = "one")]
    pub replicas: u32,
    #[serde(default = "sixty")]
    pub analysis_seconds: u32,
    #[serde(default = "two_minutes")]
    pub progress_deadline_seconds: u32,
    #[serde(default = "fifth")]
    pub max_failure_ratio: f64,
    #[serde(default)]
    pub min_tasks: u32,
}

fn sixty() -> u32 {
    60
}
fn two_minutes() -> u32 {
    120
}
fn fifth() -> f64 {
    0.2
}

fn one() -> u32 {
    1
}
fn five() -> u32 {
    5
}

pub fn admit_deployment(spec: &serde_json::Value) -> Result<DeploymentSpec, ApiError> {
    let s: DeploymentSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("deployment spec: {e}")).field("spec"))?;
    if s.replicas > 1000 {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "replicas must be <= 1000").field("spec.replicas"));
    }
    if s.agent.is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "agent is required").field("spec.agent"));
    }
    if let Some(a) = &s.autoscale
        && (a.max_replicas == 0 || a.max_replicas > 1000 || a.min_replicas > a.max_replicas || a.target_tasks_per_replica == 0)
    {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "autoscale needs 0 ≤ minReplicas ≤ maxReplicas ≤ 1000, maxReplicas ≥ 1, targetTasksPerReplica ≥ 1").field("spec.autoscale"));
    }
    if let Some(c) = &s.rollout.canary
        && (c.replicas == 0 || !(0.0..=1.0).contains(&c.max_failure_ratio) || c.progress_deadline_seconds == 0)
    {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "canary needs replicas ≥ 1, progressDeadlineSeconds ≥ 1 and maxFailureRatio in 0..=1").field("spec.rollout.canary"));
    }
    if s.rollout.max_unavailable == 0 {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "maxUnavailable must be at least 1, or a rollout could never start").field("spec.rollout.maxUnavailable"));
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
            Runtime::Oci { class: SandboxClass::Container, .. } => "oci",
            Runtime::Oci { class: SandboxClass::Gvisor, .. } => "oci/gvisor",
            Runtime::Oci { class: SandboxClass::Microvm, .. } => "oci/microvm",
        }
    }
}

#[cfg(test)]
mod image_policy_tests {
    use super::*;
    use serde_json::json;

    const KEY: &str = "-----BEGIN PUBLIC KEY-----\nMFkw\n-----END PUBLIC KEY-----\n";

    #[test]
    fn policies_combine() {
        assert!(admit_image_policy(&json!({"requireSignature": true})).is_err(), "signature without keys");
        assert!(admit_image_policy(&json!({"keys": ["not a key"]})).is_err());
        let team = admit_image_policy(&json!({"allowedRepositories": ["registry.example/team/"], "requireSignature": true, "keys": [KEY]})).unwrap();
        let open = admit_image_policy(&json!({"allowProcessRuntime": true})).unwrap();
        let ps = vec![("team".to_owned(), team), ("open".to_owned(), open)];
        let oci = |i: &str| Runtime::Oci { image: i.into(), class: SandboxClass::Container };
        assert_eq!(image_allowed(&ps, &oci("registry.example/team/a@sha256:00")).unwrap(), [KEY]);
        assert!(image_allowed(&ps, &oci("docker.io/evil/a@sha256:00")).unwrap_err().contains("ImagePolicy team allows only"));
        let process = Runtime::Process { command: vec!["sh".into()], env: Default::default(), working_dir: None };
        assert!(image_allowed(&ps, &process).unwrap_err().contains("ImagePolicy team doesn't allow"), "every policy must allow process runtimes");
        assert!(image_allowed(&[], &process).unwrap().is_empty(), "no policy, no restriction");
    }

    #[test]
    fn runtime_classes() {
        let a = admit_agent(&serde_json::json!({"runtime": {"type": "oci", "image": format!("r/a@sha256:{}", "0".repeat(64)), "class": "microvm"}})).unwrap();
        assert_eq!(a.runtime.kind(), "oci/microvm");
        let plain = admit_agent(&serde_json::json!({"runtime": {"type": "oci", "image": format!("r/a@sha256:{}", "0".repeat(64))}})).unwrap();
        assert_eq!(plain.runtime.kind(), "oci", "container is the default and keeps the old kind");
        assert!(serde_json::to_value(&plain.runtime).unwrap().get("class").is_none(), "default class isn't serialized");
        let strict = vec![("untrusted".to_owned(), admit_image_policy(&serde_json::json!({"runtimeClasses": ["gvisor", "microvm"]})).unwrap())];
        assert!(image_allowed(&strict, &a.runtime).is_ok());
        assert!(image_allowed(&strict, &plain.runtime).unwrap_err().contains("requires runtime class"));
    }
}
