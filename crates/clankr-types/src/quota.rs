//! Namespace quotas and budgets (PLA-015, CL-31-12, CL-27-06).
//!
//! `ResourceQuota` caps what a namespace may hold or run; it's checked at admission (and at run
//! start for `activeRuns`). `AgentBudget` caps model spend per calendar period; the model
//! gateway reserves against it before every call.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode};

/// `kind: ResourceQuota`. Keys of `hard`:
/// `cpus`, `memoryMiB`, `instances` (sum over AgentDeployments: replicas x Agent resources),
/// `volumeMiB` (sum of VolumeClaim capacity), `activeRuns` (running workflow runs),
/// `count/<plural>` (objects of a kind, e.g. `count/agents`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceQuotaSpec {
    pub hard: BTreeMap<String, f64>,
}

pub const QUOTA_KEYS: &[&str] = &["cpus", "memoryMiB", "instances", "volumeMiB", "activeRuns"];

pub fn admit_resource_quota(spec: &serde_json::Value) -> Result<ResourceQuotaSpec, ApiError> {
    let s: ResourceQuotaSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("resource quota: {e}")).field("spec"))?;
    for (k, v) in &s.hard {
        let known = QUOTA_KEYS.contains(&k.as_str()) || k.strip_prefix("count/").is_some_and(|p| crate::kind_for(p).is_some());
        if !known {
            return Err(ApiError::new(ErrorCode::InvalidSpec, format!("unknown quota {k:?}; want one of {QUOTA_KEYS:?} or count/<plural>")).field(format!("spec.hard.{k}")));
        }
        if !v.is_finite() || *v < 0.0 {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "quota must be a non-negative number").field(format!("spec.hard.{k}")));
        }
    }
    Ok(s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Period {
    Day,
    Month,
    Total,
}

impl Period {
    /// Bucket key for the window containing `at` (calendar UTC).
    pub fn window(self, at: chrono::DateTime<chrono::Utc>) -> String {
        match self {
            Period::Day => at.format("day:%Y-%m-%d").to_string(),
            Period::Month => at.format("month:%Y-%m").to_string(),
            Period::Total => "total".into(),
        }
    }
}

/// `kind: AgentBudget`: model spend cap for the whole namespace, per period. Every budget in a
/// namespace applies; a call must fit all of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudgetSpec {
    pub period: Period,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    /// Needs `pricing` for the model on its ProviderConnection; unpriced calls are refused.
    #[serde(default, rename = "maxCostUSD", skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

pub fn admit_agent_budget(spec: &serde_json::Value) -> Result<AgentBudgetSpec, ApiError> {
    let s: AgentBudgetSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("agent budget: {e}")).field("spec"))?;
    if s.max_tokens.is_none() && s.max_cost_usd.is_none() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "set maxTokens and/or maxCostUSD").field("spec"));
    }
    if s.max_tokens.is_some_and(|t| t < 0) || s.max_cost_usd.is_some_and(|c| !c.is_finite() || c < 0.0) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "limits must be non-negative").field("spec"));
    }
    Ok(s)
}

/// USD to the integer micro-dollars budgets are kept in.
pub fn micro_usd(usd: f64) -> i64 {
    (usd * 1e6).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admission_and_windows() {
        assert!(admit_resource_quota(&json!({"hard": {"cpus": 4, "count/agents": 10, "activeRuns": 3}})).is_ok());
        assert_eq!(admit_resource_quota(&json!({"hard": {"gpus": 1}})).unwrap_err().field.as_deref(), Some("spec.hard.gpus"));
        assert!(admit_resource_quota(&json!({"hard": {"count/nonsense": 1}})).is_err());
        assert!(admit_resource_quota(&json!({"hard": {"cpus": -1}})).is_err());
        assert!(admit_agent_budget(&json!({"period": "day"})).is_err());
        let b = admit_agent_budget(&json!({"period": "month", "maxCostUSD": 12.5})).unwrap();
        assert_eq!(micro_usd(b.max_cost_usd.unwrap()), 12_500_000);
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-01T23:59:59Z").unwrap().to_utc();
        assert_eq!((Period::Day.window(at), Period::Month.window(at), Period::Total.window(at)), ("day:2026-11-01".into(), "month:2026-11".into(), "total".into()));
    }
}
