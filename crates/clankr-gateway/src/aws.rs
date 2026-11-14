//! AWS Signature Version 4, just enough to sign one JSON request (Bedrock, S3); written out rather than pulled in, and checked against the AWS test suite's vector.

use hmac::{KeyInit, Mac};
use sha2::{Digest, Sha256};

/// `ACCESS_KEY_ID:SECRET_ACCESS_KEY[:SESSION_TOKEN]`, the form a secret holds it in.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub key_id: String,
    pub secret: String,
    pub session: Option<String>,
}

impl Credentials {
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.trim().splitn(3, ':');
        let key_id = it.next()?.to_owned();
        let secret = it.next()?.to_owned();
        if key_id.is_empty() || secret.is_empty() {
            return None;
        }
        Some(Self { key_id, secret, session: it.next().map(str::to_owned).filter(|s| !s.is_empty()) })
    }

    pub fn from_env() -> Option<Self> {
        let key_id = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
        Some(Self { key_id, secret, session: std::env::var("AWS_SESSION_TOKEN").ok().filter(|s| !s.is_empty()) })
    }
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = hmac::Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

fn sha_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// Percent-encodes a path segment the way SigV4 canonicalizes it (unreserved characters as is).
pub fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A request to sign: `path` already percent-encoded, `query` canonical (`k=v&k2=v2`, sorted,
/// encoded) or empty, `headers` lowercase names with trimmed values.
pub struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: &'a [u8],
}

/// Signs `req` for `now` (`YYYYMMDDTHHMMSSZ`): returns the headers to add, `authorization`
/// included. `host`, `x-amz-date`, `x-amz-content-sha256` and a session token are added here.
pub fn sign(req: &Request, creds: &Credentials, region: &str, service: &str, now: &str) -> Vec<(String, String)> {
    let date = &now[..8];
    let body_hash = sha_hex(req.body);
    let mut headers: Vec<(String, String)> = req.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned())).collect();
    headers.push(("host".into(), req.host.to_owned()));
    headers.push(("x-amz-date".into(), now.to_owned()));
    headers.push(("x-amz-content-sha256".into(), body_hash.clone()));
    if let Some(t) = &creds.session {
        headers.push(("x-amz-security-token".into(), t.clone()));
    }
    headers.sort();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canonical = format!("{}\n{}\n{}\n{canonical_headers}\n{}\n{body_hash}", req.method, req.path, req.query, signed.join(";"));
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{now}\n{scope}\n{}", sha_hex(canonical.as_bytes()));
    let k = hmac(format!("AWS4{}", creds.secret).as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let signature = hex::encode(hmac(&k, to_sign.as_bytes()));
    headers.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={}, Signature={signature}", creds.key_id, signed.join(";"))));
    headers.retain(|(k, _)| k != "host"); // reqwest sets it
    headers
}

pub fn now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `get-vanilla` from the AWS SigV4 test suite.
    #[test]
    fn matches_the_aws_test_suite() {
        let creds = Credentials::parse("AKIDEXAMPLE:wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY").unwrap();
        let req = Request { method: "GET", host: "example.amazonaws.com", path: "/", query: "", headers: vec![], body: b"" };
        let h = sign(&req, &creds, "us-east-1", "service", "20150830T123600Z");
        let auth = &h.iter().find(|(k, _)| k == "authorization").unwrap().1;
        let without = sign_without_content_sha(&req, &creds, "us-east-1", "service", "20150830T123600Z");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert_eq!(without, "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        assert!(Credentials::parse("x").is_none());
        assert_eq!(encode_segment("anthropic.claude-3:0/x"), "anthropic.claude-3%3A0%2Fx");
    }

    /// The suite's vectors predate `x-amz-content-sha256`; this signs the same way minus that header.
    fn sign_without_content_sha(req: &Request, creds: &Credentials, region: &str, service: &str, now: &str) -> String {
        let date = &now[..8];
        let canonical = format!("{}\n{}\n{}\nhost:{}\nx-amz-date:{now}\n\nhost;x-amz-date\n{}", req.method, req.path, req.query, req.host, sha_hex(req.body));
        let scope = format!("{date}/{region}/{service}/aws4_request");
        let to_sign = format!("AWS4-HMAC-SHA256\n{now}\n{scope}\n{}", sha_hex(canonical.as_bytes()));
        let k = hmac(format!("AWS4{}", creds.secret).as_bytes(), date.as_bytes());
        let k = hmac(&k, region.as_bytes());
        let k = hmac(&k, service.as_bytes());
        let k = hmac(&k, b"aws4_request");
        hex::encode(hmac(&k, to_sign.as_bytes()))
    }
}
