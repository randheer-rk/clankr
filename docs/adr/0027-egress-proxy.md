# 0027 — Agent egress through the node

Status: accepted (2026-11-03)

**Decision.** Agent containers keep their own network namespace with loopback only. Outbound access exists only for the hosts an Agent lists in `spec.egress`:
- `api.github.com` (ports 443 and 80);
- `*.example.com` (subdomains only);
- `host:port`.

An empty list means no network, as before.

**How it works.**
- For each such instance, the node serves an HTTP proxy on `egress.sock` in the instance's socket directory, which is already bind-mounted at `/run/clankr`.
- `kennel run --forward 15080=/run/clankr/egress.sock` makes kennel-init (PID 1, already inside the container's network namespace) bridge `127.0.0.1:15080` to that socket.
- The node sets `HTTP(S)_PROXY`, `NO_PROXY=localhost,127.0.0.1` and `NODE_USE_ENV_PROXY=1`, so curl, Python, Go, Node and Chromium (`@clankr/sdk/browser` passes `--proxy-server`) use it unchanged.
- The proxy handles `CONNECT host:port` tunnels and absolute-URI `http://` requests.
- It checks the allowlist, then resolves names itself. It refuses private, loopback, link-local and CGNAT addresses, so an allowlisted name can't be pointed at the node's own network, and it tries each remaining address in turn.
- It logs every connection with its byte counts, and stops when the instance does.

**Notes.**
- The workload gains no capability. The bridge runs in kennel-init, and the policy lives on the node.
- `CLANKR_EGRESS_ALLOW_PRIVATE=1` on the node lifts the private-address refusal, for development and tests only.
- The process runtime (S0) runs on the host network and ignores `egress`.
- microVMs get no egress yet; that needs the vsock bridge.
