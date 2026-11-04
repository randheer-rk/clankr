//! Access control (SEC-02, CL-31-03): principals, built-in roles and the permission each API
//! route needs. Deny by default: a route with no mapping is refused.

use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode};

/// Namespace whose RoleBindings apply cluster-wide.
pub const SYSTEM_NAMESPACE: &str = "clankr-system";

/// Who is calling, after authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Principal {
    /// Bootstrap owner token: implicit cluster-admin (break-glass; every action audited).
    Owner,
    /// OIDC user: `id` is the verified email (or subject), `groups` from the token.
    User { id: String, groups: Vec<String> },
    ServiceAccount { namespace: String, name: String, groups: Vec<String> },
    Node { name: String },
}

impl Principal {
    /// Actor string used in audit records and provenance.
    pub fn actor(&self) -> String {
        match self {
            Principal::Owner => "owner".into(),
            Principal::User { id, .. } => format!("user:{id}"),
            Principal::ServiceAccount { namespace, name, .. } => format!("serviceaccount:{namespace}/{name}"),
            Principal::Node { name } => format!("node:{name}"),
        }
    }

    /// Subject strings a RoleBinding can name to match this principal.
    pub fn subjects(&self) -> Vec<String> {
        let mut s = vec![self.actor()];
        if let Principal::User { groups, .. } | Principal::ServiceAccount { groups, .. } = self {
            s.extend(groups.iter().map(|g| format!("group:{g}")));
        }
        s
    }

    pub fn groups(&self) -> &[String] {
        match self {
            Principal::User { groups, .. } | Principal::ServiceAccount { groups, .. } => groups,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Perm {
    ResourcesRead,
    ResourcesWrite,
    RunsRead,
    RunsWrite,
    ApprovalsDecide,
    ContextRead,
    /// Doing agent work: lease/complete tasks, model calls, context writes, mailbox traffic.
    WorkExecute,
    SecretsRead,
    SecretsWrite,
    /// RoleBindings, ServiceAccounts and their tokens.
    RbacWrite,
    /// Moving an image digest between AgentImage stages (deploys it wherever that stage is tracked).
    ImagesPromote,
    /// A shell into a running agent instance (`clank exec`).
    InstancesExec,
    /// Cluster-only: node maintenance.
    NodesManage,
    /// Cluster-only: the audit log.
    AuditRead,
}

impl Perm {
    /// Only grantable by a binding in the system namespace.
    pub fn cluster_only(self) -> bool {
        matches!(self, Perm::NodesManage | Perm::AuditRead)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Viewer,
    Editor,
    Approver,
    Admin,
    ClusterAdmin,
}

impl Role {
    pub fn grants(self, p: Perm) -> bool {
        use Perm::*;
        let viewer = matches!(p, ResourcesRead | RunsRead | ContextRead);
        match self {
            Role::Viewer => viewer,
            Role::Editor => viewer || matches!(p, ResourcesWrite | RunsWrite | WorkExecute),
            Role::Approver => viewer || p == ApprovalsDecide,
            Role::Admin => !p.cluster_only(),
            Role::ClusterAdmin => true,
        }
    }
}

/// `kind: RoleBinding` (namespaced; in `clankr-system` it applies everywhere).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleBindingSpec {
    pub role: Role,
    /// `user:<email>`, `group:<name>`, `serviceaccount:<ns>/<name>`
    pub subjects: Vec<String>,
}

/// `kind: ServiceAccount`: a non-human identity that holds API tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceAccountSpec {
    /// Groups the account belongs to (for group bindings and approval routing).
    #[serde(default)]
    pub groups: Vec<String>,
}

pub fn admit_role_binding(spec: &serde_json::Value) -> Result<RoleBindingSpec, ApiError> {
    let s: RoleBindingSpec = serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("role binding: {e}")).field("spec"))?;
    if s.subjects.is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "at least one subject").field("spec.subjects"));
    }
    for (i, sub) in s.subjects.iter().enumerate() {
        let ok = match sub.split_once(':') {
            Some(("user" | "group", v)) => !v.is_empty(),
            Some(("serviceaccount", v)) => v.split_once('/').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty()),
            _ => false,
        };
        if !ok {
            return Err(ApiError::new(ErrorCode::InvalidSpec, format!("subject {sub:?}: want user:<id>, group:<name> or serviceaccount:<ns>/<name>")).field(format!("spec.subjects[{i}]")));
        }
    }
    Ok(s)
}

pub fn admit_service_account(spec: &serde_json::Value) -> Result<ServiceAccountSpec, ApiError> {
    serde_json::from_value(spec.clone()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("service account: {e}")).field("spec"))
}

/// The permission a request needs and the namespace it applies to (None = cluster scope).
/// `None` overall means the route isn't covered by RBAC (health, join, node API, whoami).
pub fn required(method: &str, path: &str) -> Option<(Option<String>, Perm)> {
    use Perm::*;
    let read = method == "GET";
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let ns = |n: &str| Some(n.to_owned());
    Some(match seg.as_slice() {
        ["v1", "whoami"] | ["v1", "auth", "config"] => return None,
        ["v1", "audit"] => (None, AuditRead),
        ["v1", "nodes", ..] => (None, NodesManage),
        ["v1", "namespaces", n, "secrets", ..] => (ns(n), if read { SecretsRead } else { SecretsWrite }),
        ["v1", "namespaces", n, "serviceaccounts", _, "tokens", ..] => (ns(n), if read { ResourcesRead } else { RbacWrite }),
        ["v1", "namespaces", n, "agentimages", _, "promotions" | "rollback"] if !read => (ns(n), ImagesPromote),
        // recorded prompts and responses can hold anything an agent saw: secret-level access
        ["v1", "namespaces", n, "runs", _, "traces"] => (ns(n), SecretsRead),
        ["v1", "namespaces", n, "workflows", _, "runs"] | ["v1", "namespaces", n, "runs", ..] => (ns(n), if read { RunsRead } else { RunsWrite }),
        ["v1", "namespaces", n, "remoteruns", ..] => (ns(n), if read { RunsRead } else { RunsWrite }),
        ["v1", "namespaces", n, "approvals", ..] => (ns(n), if read { RunsRead } else { ApprovalsDecide }),
        ["v1", "namespaces", n, "tasks" | "models", ..] => (ns(n), WorkExecute),
        ["v1", "namespaces", n, "instances", _, "checkpoint"] => (ns(n), WorkExecute),
        ["v1", "namespaces", n, "instances", _, "exec"] | ["v1", "namespaces", n, "exec", ..] => (ns(n), InstancesExec),
        ["v1", "namespaces", n, "toolclaims", _, "call"] => (ns(n), WorkExecute),
        ["v1", "namespaces", n, "toolcalls", _, "complete"] => (ns(n), WorkExecute),
        // snapshots and restores are an operator's tool, not an agent's
        ["v1", "namespaces", n, "contexts", _, "snapshots", ..] => (ns(n), if read { ContextRead } else { ResourcesWrite }),
        ["v1", "namespaces", n, "contexts", ..] => (ns(n), if read { ContextRead } else { WorkExecute }),
        ["v1", "namespaces", n, "mailboxes", _, "messages", _, "requeue"] => (ns(n), ResourcesWrite),
        ["v1", "namespaces", n, "mailboxes", _, "messages"] if read => (ns(n), ResourcesRead),
        ["v1", "namespaces", n, "mailboxes", ..] => (ns(n), WorkExecute),
        ["v1", "namespaces", n, "services", _, "call" | "reply"] | ["v1", "namespaces", n, "topics", _, "publish"] => (ns(n), WorkExecute),
        ["v1", "namespaces", n, "usage", ..] => (ns(n), ResourcesRead),
        // A2A: the agent card reads like a resource; sending a message is agent work
        ["a2a", n, _, ".well-known", ..] => (ns(n), ResourcesRead),
        ["a2a", n, _] => (ns(n), WorkExecute),
        ["metrics"] => (None, AuditRead),
        ["v1", "namespaces", n, plural, ..] => {
            let rbac = matches!(*plural, "rolebindings" | "serviceaccounts");
            (ns(n), if read { ResourcesRead } else if rbac { RbacWrite } else { ResourcesWrite })
        }
        // listing a kind across all namespaces needs a cluster-wide grant
        ["v1", _plural] if read => (None, ResourcesRead),
        _ => (None, Perm::NodesManage), // unknown: deny unless cluster-admin
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_routes() {
        assert!(Role::Viewer.grants(Perm::RunsRead) && !Role::Viewer.grants(Perm::RunsWrite));
        assert!(Role::Editor.grants(Perm::WorkExecute) && !Role::Editor.grants(Perm::SecretsWrite));
        assert!(Role::Approver.grants(Perm::ApprovalsDecide) && !Role::Approver.grants(Perm::ResourcesWrite));
        assert!(Role::Admin.grants(Perm::SecretsWrite) && !Role::Admin.grants(Perm::NodesManage));
        assert!(Role::Admin.grants(Perm::ImagesPromote) && !Role::Editor.grants(Perm::ImagesPromote));
        assert!(Role::ClusterAdmin.grants(Perm::AuditRead));

        let r = |m, p| required(m, p).map(|(n, perm)| (n.unwrap_or_default(), perm));
        assert_eq!(r("GET", "/v1/namespaces/a/agents"), Some(("a".into(), Perm::ResourcesRead)));
        assert_eq!(r("POST", "/v1/namespaces/a/rolebindings"), Some(("a".into(), Perm::RbacWrite)));
        assert_eq!(r("GET", "/v1/namespaces/a/secrets/x"), Some(("a".into(), Perm::SecretsRead)));
        assert_eq!(r("POST", "/v1/namespaces/a/workflows/w/runs"), Some(("a".into(), Perm::RunsWrite)));
        assert_eq!(r("PUT", "/v1/namespaces/a/contexts/c/keys/k"), Some(("a".into(), Perm::WorkExecute)));
        assert_eq!(r("GET", "/v1/agents"), Some(("".into(), Perm::ResourcesRead)));
        assert_eq!(r("GET", "/v1/audit"), Some(("".into(), Perm::AuditRead)));
        assert_eq!(r("GET", "/v1/whoami"), None);
        assert_eq!(r("POST", "/v1/namespaces/a/agentimages/x/promotions"), Some(("a".into(), Perm::ImagesPromote)));
        assert_eq!(r("GET", "/v1/namespaces/a/agentimages/x/promotions"), Some(("a".into(), Perm::ResourcesRead)));
        assert_eq!(r("GET", "/v1/namespaces/a/runs/x/traces"), Some(("a".into(), Perm::SecretsRead)));
        assert_eq!(r("GET", "/v1/namespaces/a/runs/x/history"), Some(("a".into(), Perm::RunsRead)));
    }

    #[test]
    fn subjects_and_admission() {
        let p = Principal::User { id: "a@x.io".into(), groups: vec!["eng".into()] };
        assert_eq!(p.subjects(), ["user:a@x.io", "group:eng"]);
        assert!(admit_role_binding(&serde_json::json!({"role": "editor", "subjects": ["user:a@x.io", "serviceaccount:ns/ci"]})).is_ok());
        assert!(admit_role_binding(&serde_json::json!({"role": "editor", "subjects": ["alice"]})).is_err());
        assert!(admit_role_binding(&serde_json::json!({"role": "god", "subjects": ["user:a"]})).is_err());
    }
}
