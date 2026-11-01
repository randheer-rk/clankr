# 0013 — Approval inbox

Status: accepted (2026-11-01)

## Requests

Each visit of an `approval` node opens one request. It is an `approvals` row, written in the same transaction as the `WaitStarted` event. The request is bound to:
- the run, node and visit
- the approver group
- whoever started the run (`requested_by`)
- the run data at that moment (`subject`) and a SHA-256 digest over all of the above

## Deciding

`POST .../approvals/{id}/approve|reject {digest, comment}` passes only if all of these hold:
1. **Permission:** RBAC grants ApprovalsDecide in the namespace (the approver, admin or cluster-admin role).
2. **Group:** the principal is in the node's `approverGroup`. The role alone isn't enough, and neither is the group alone.
3. **No self-approval:** the principal didn't start the run.
4. **Action-bound:** the request carries the digest the approver reviewed. A mismatch returns Conflict, so a decision can't be applied to a different action.
5. **Once only:** the request is still pending. The decision and the run resuming happen in one transaction, after locking the run row and then the approval row, so a request is decided at most once.

Raw `approve.*` signals are refused, so the signal API can't bypass these checks.

**Owner token:** it is break-glass. It skips checks 2 and 3, but its decisions are audited with "(break-glass)". Without this, single-user development setups could never get past an approval node.

**Record:** the decision lands in three places: the audit log (`approval approved <node>`), the run history (`by`, `comment` and the approval id in the signal payload), and the approvals row.

## Deadlines

`timeout: "2d"` on an approval node sets a deadline. The engine tick rejects overdue requests as `expired`, and the run takes the `onReject` path or fails. When a run finishes, is cancelled or fails, its pending requests become `cancelled`.

## Inbox

- **API:** `GET .../approvals?state=pending&mine=true` lists what the caller can actually decide: their groups, not their own runs.
- **CLI:**
  - `clank approvals ls|show|approve|reject`
  - `clank run approve RUN NODE` as a shortcut

**Not yet:**
- escalation to a second group
- multi-party approvals (N of M)
- approval tokens consumed by a tool gateway (CL-28-07)
- a cross-namespace inbox
