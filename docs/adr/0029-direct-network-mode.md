# 0029 — Policy-controlled direct network

Status: accepted (2026-11-03)

**Context.** The default (ADR 0027) is no network at all, plus HTTP(S)/CONNECT egress to allowlisted hosts through the node's proxy. Some agents need protocols or destinations that proxy model can't express, for example raw TCP to arbitrary public services.

**Decision.** `Agent.spec.network: direct` gives an agent its own outbound network: NAT through a pasta namespace (`kennel run --network nat`).
- **Destinations:** private, loopback, link-local (cloud metadata), CGNAT, multicast and reserved destinations are dropped by an nft filter, unless the NetworkPolicy opens specific CIDRs.
- **Protocol:** IPv6 is off, and DNS goes to pasta's forwarder.
- **Gates:**
  1. Runtime class `gvisor` or `microvm`. Direct network access calls for the stronger sandboxes, so plain containers are refused at admission (and by kennel).
  2. `kind: NetworkPolicy { allowDirect, allowCidrs }`. At least one must exist, and every policy in the namespace must allow it. The private CIDRs opened are those every policy lists. This is checked when the Agent is saved (a refusal is an API error, audited like any write) and again by the node before every launch.
- A direct-network agent gets no proxy. Each new connection is still logged by the node (`direct egress`: protocol, destination, and the filter's accept or drop), from kennel's `--nat-log` (nft's trace of each connection's first packet). Byte counts aren't recorded, unlike proxy egress.
