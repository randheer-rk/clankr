//! Per-instance egress proxy (ADR 0027). Agents have no network of their own. When an Agent
//! lists `egress` hosts, the node serves an HTTP proxy on a Unix socket in the instance's socket
//! dir, and kennel-init bridges the container's 127.0.0.1:15080 to it. Two request forms:
//!
//! - `CONNECT host:port` tunnels (HTTPS and anything TLS).
//! - Absolute-URI `http://` requests are forwarded as plain HTTP.
//!
//! Only listed hosts pass. The proxy resolves names itself and refuses private, loopback and
//! link-local addresses (no SSRF into the node's network). Every connection is logged with its
//! byte counts.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use clankr_types::agent::{EgressRule, egress_allowed};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// Port the proxy appears on inside the container.
pub const CONTAINER_PORT: u16 = 15080;
const MAX_HEAD: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct Egress {
    pub rules: Vec<EgressRule>,
    /// Development and tests only ($CLANKR_EGRESS_ALLOW_PRIVATE=1): let listed hosts resolve to
    /// private addresses.
    pub allow_private: bool,
    /// Who's connecting, for the log.
    pub who: String,
}

/// Starts serving at `path`; the proxy stops when the handle is aborted or dropped by the caller.
pub fn serve(path: &Path, e: Egress) -> Result<JoinHandle<()>> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    Ok(tokio::spawn(async move {
        while let Ok((c, _)) = l.accept().await {
            let e = e.clone();
            tokio::spawn(async move {
                if let Err(why) = handle(c, &e).await {
                    tracing::debug!(who = %e.who, error = %why, "egress connection ended");
                }
            });
        }
    }))
}

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified() || v.is_broadcast() || v.is_multicast() || o[0] == 0 || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return private(IpAddr::V4(m));
            }
            let s = v.segments()[0];
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
    }
}

async fn reply(c: &mut UnixStream, status: &str, why: &str) -> Result<()> {
    c.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: text/plain\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{why}\n", why.len() + 1).as_bytes()).await?;
    Ok(())
}

async fn handle(mut c: UnixStream, e: &Egress) -> Result<()> {
    // the request head; anything after it (a request body, TLS bytes) is passed through
    let mut buf = Vec::with_capacity(1024);
    let head_end = tokio::time::timeout(TIMEOUT, async {
        loop {
            let mut chunk = [0u8; 2048];
            let n = c.read(&mut chunk).await?;
            if n == 0 {
                anyhow::bail!("client closed before sending a request");
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(i + 4);
            }
            if buf.len() > MAX_HEAD {
                anyhow::bail!("request head too large");
            }
        }
    })
    .await??;
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut first = head.lines().next().unwrap_or_default().split_whitespace();
    let (method, target, version) = (first.next().unwrap_or_default(), first.next().unwrap_or_default(), first.next().unwrap_or("HTTP/1.1"));

    let (host, port, forward_head) = if method.eq_ignore_ascii_case("CONNECT") {
        let Some((h, p)) = target.rsplit_once(':').and_then(|(h, p)| Some((h.trim_matches(['[', ']']).to_owned(), p.parse::<u16>().ok()?))) else {
            return reply(&mut c, "400 Bad Request", "CONNECT wants host:port").await;
        };
        (h, p, None)
    } else if let Some(rest) = target.strip_prefix("http://") {
        let (authority, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
        let (h, p) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(80)),
            None => (authority.to_owned(), 80),
        };
        // origin-form request line for the upstream server
        let mut rewritten = format!("{method} {path} {version}\r\n");
        for l in head.lines().skip(1) {
            if l.is_empty() || l.to_ascii_lowercase().starts_with("proxy-") {
                continue;
            }
            rewritten.push_str(l);
            rewritten.push_str("\r\n");
        }
        rewritten.push_str("\r\n");
        (h, p, Some(rewritten))
    } else {
        return reply(&mut c, "400 Bad Request", "use CONNECT for https, or an absolute http:// URL").await;
    };

    if !egress_allowed(&e.rules, &host, port) {
        tracing::warn!(who = %e.who, %host, port, "egress refused: not in Agent.spec.egress");
        return reply(&mut c, "403 Forbidden", &format!("{host}:{port} is not in this agent's egress allowlist")).await;
    }
    let addrs: Vec<SocketAddr> = match tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((host.as_str(), port))).await {
        Ok(Ok(a)) => a.collect(),
        _ => return reply(&mut c, "502 Bad Gateway", &format!("can't resolve {host}")).await,
    };
    let candidates: Vec<SocketAddr> = addrs.into_iter().filter(|a| e.allow_private || !private(a.ip())).collect();
    if candidates.is_empty() {
        tracing::warn!(who = %e.who, %host, "egress refused: resolves only to private addresses");
        return reply(&mut c, "403 Forbidden", &format!("{host} resolves to a private address")).await;
    }
    // every allowed address in turn (a name may resolve to v6 and v4)
    let mut up = None;
    for a in candidates {
        if let Ok(Ok(s)) = tokio::time::timeout(TIMEOUT, TcpStream::connect(a)).await {
            up = Some(s);
            break;
        }
    }
    let Some(mut up) = up else {
        return reply(&mut c, "502 Bad Gateway", &format!("can't connect to {host}:{port}")).await;
    };
    match forward_head {
        None => c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?,
        Some(h) => up.write_all(h.as_bytes()).await?,
    }
    up.write_all(&buf[head_end..]).await?;
    let (sent, received) = tokio::io::copy_bidirectional(&mut c, &mut up).await.unwrap_or((0, 0));
    tracing::info!(who = %e.who, %host, port, sent, received, "egress");
    Ok(())
}

/// Follows kennel's `--nat-log` (`PROTO ADDR:PORT accept|drop` per new connection) for a
/// direct-network agent, logging each line until aborted.
pub fn tail_nat_log(path: std::path::PathBuf, who: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut f) = tokio::fs::File::open(&path).await else { return };
        let mut partial = String::new();
        loop {
            // the file position carries over: each read returns only what kennel appended
            let mut new = vec![];
            if f.read_to_end(&mut new).await.is_ok_and(|n| n > 0) {
                partial.push_str(&String::from_utf8_lossy(&new));
                while let Some(i) = partial.find('\n') {
                    let line: String = partial.drain(..=i).collect();
                    let mut f = line.split_whitespace();
                    if let (Some(proto), Some(dest), Some(verdict)) = (f.next(), f.next(), f.next()) {
                        if verdict == "drop" {
                            tracing::warn!(%who, proto, dest, verdict, "direct egress");
                        } else {
                            tracing::info!(%who, proto, dest, verdict, "direct egress");
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for ip in ["10.1.2.3", "127.0.0.1", "169.254.169.254", "192.168.0.1", "172.16.5.4", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "140.82.112.3", "2606:4700::1111"] {
            assert!(!private(ip.parse().unwrap()), "{ip}");
        }
    }

    async fn ask(sock: &Path, req: &str) -> String {
        let mut c = UnixStream::connect(sock).await.unwrap();
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn tunnels_forwards_and_refuses() {
        // upstream: answers any request with a fixed HTTP response, then closes
        let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = up.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = up.accept().await {
                tokio::spawn(async move {
                    let mut b = [0u8; 1024];
                    let n = c.read(&mut b).await.unwrap_or(0);
                    let line = String::from_utf8_lossy(&b[..n]).lines().next().unwrap_or_default().to_owned();
                    let body = format!("saw {line}");
                    let _ = c.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("egress.sock");
        let rules = vec![EgressRule::parse(&format!("localhost:{port}")).unwrap()];
        let _h = serve(&sock, Egress { rules: rules.clone(), allow_private: true, who: "t".into() }).unwrap();

        let r = ask(&sock, &format!("CONNECT localhost:{port} HTTP/1.1\r\nhost: localhost\r\n\r\nGET /tunneled HTTP/1.1\r\n\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 200 Connection Established") && r.contains("saw GET /tunneled"), "{r}");
        let r = ask(&sock, &format!("GET http://localhost:{port}/plain?x=1 HTTP/1.1\r\nhost: localhost\r\nproxy-authorization: x\r\n\r\n")).await;
        assert!(r.contains("saw GET /plain?x=1 HTTP/1.1"), "absolute URI rewritten to origin form: {r}");
        let r = ask(&sock, "CONNECT example.com:443 HTTP/1.1\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 403") && r.contains("allowlist"), "{r}");

        // without the dev override, a listed host that resolves privately is still refused
        let sock2 = dir.path().join("strict.sock");
        let _h2 = serve(&sock2, Egress { rules, allow_private: false, who: "t".into() }).unwrap();
        let r = ask(&sock2, &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 403") && r.contains("private"), "{r}");
    }
}
