//! Multi-cluster placement (QUE-11, CL-23-11): explicit cross-cluster routing of workflow runs
//! under residency and secret rules. Nothing moves transparently: the remote cluster must
//! already hold the same workflow and the secrets it needs, or the start is refused.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode};

/// `kind: Cluster` (in `clankr-system`): a remote Clankr control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClusterSpec {
    /// Base URL of its API, e.g. `https://clankr.eu-1.example`.
    pub endpoint: String,
    /// Where its data lives, matched against PlacementPolicy `allowedRegions`.
    pub region: String,
    /// A ServiceAccount token on that cluster, kept in a Secret in `clankr-system`.
    pub token_secret_ref: SecretKeyRef,
    /// PEM CA for a private endpoint (else the system roots).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_pem: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretKeyRef {
    pub name: String,
    pub key: String,
}

/// `kind: PlacementPolicy`: where a namespace's runs may go. Every policy in the namespace applies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlacementPolicySpec {
    /// Regions remote runs may be placed in (empty = any).
    #[serde(default)]
    pub allowed_regions: Vec<String>,
    /// Secrets that must already exist (same namespace) on the remote cluster.
    #[serde(default)]
    pub required_secrets: Vec<String>,
    /// Keep every run in this cluster.
    #[serde(default)]
    pub local_only: bool,
}

pub fn admit_cluster(spec: &serde_json::Value) -> Result<ClusterSpec, ApiError> {
    let s: ClusterSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("cluster: {e}")).field("spec"))?;
    let local = s.endpoint.starts_with("http://127.0.0.1") || s.endpoint.starts_with("http://localhost");
    if !(s.endpoint.starts_with("https://") || local) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "endpoint must be https (http only for localhost)").field("spec.endpoint"));
    }
    if s.region.is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "region is required (residency rules match on it)").field("spec.region"));
    }
    Ok(s)
}

pub fn admit_placement_policy(spec: &serde_json::Value) -> Result<PlacementPolicySpec, ApiError> {
    serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("placement policy: {e}")).field("spec"))
}

/// May a run of this namespace go to `cluster`? Returns the secrets the remote must hold.
pub fn placement_allowed(policies: &[(String, PlacementPolicySpec)], cluster: &str, c: &ClusterSpec) -> Result<Vec<String>, String> {
    let mut secrets = vec![];
    for (name, p) in policies {
        if p.local_only {
            return Err(format!("PlacementPolicy {name} keeps runs in this cluster"));
        }
        if !p.allowed_regions.is_empty() && !p.allowed_regions.contains(&c.region) {
            return Err(format!("PlacementPolicy {name} allows regions {:?}; cluster {cluster} is in {}", p.allowed_regions, c.region));
        }
        secrets.extend(p.required_secrets.iter().cloned());
    }
    secrets.sort();
    secrets.dedup();
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn residency_and_secrets() {
        let eu = admit_cluster(&json!({"endpoint": "https://eu.example", "region": "eu-west", "tokenSecretRef": {"name": "eu", "key": "token"}})).unwrap();
        assert!(admit_cluster(&json!({"endpoint": "http://eu.example", "region": "eu", "tokenSecretRef": {"name": "eu", "key": "t"}})).is_err(), "plain http only for localhost");
        let p = |v| ("p".to_owned(), admit_placement_policy(&v).unwrap());
        assert_eq!(placement_allowed(&[], "eu", &eu).unwrap(), Vec::<String>::new());
        assert_eq!(placement_allowed(&[p(json!({"allowedRegions": ["eu-west"], "requiredSecrets": ["db", "api"]}))], "eu", &eu).unwrap(), ["api", "db"]);
        assert!(placement_allowed(&[p(json!({"allowedRegions": ["us-east"]}))], "eu", &eu).unwrap_err().contains("us-east"));
        assert!(placement_allowed(&[p(json!({"localOnly": true}))], "eu", &eu).is_err());
    }
}
