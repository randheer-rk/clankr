//! Workload identity (CL-31-02): short-lived tokens naming one agent instance on one node at one
//! placement epoch. Issued in heartbeat replies, required on every call a node makes for an agent,
//! and re-checked against the current placement, so a node that lost an instance is refused at
//! once. HMAC-SHA256 with a key derived from the master key: only clankrd issues and verifies.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use clankr_types::{ApiError, ErrorCode};
use hmac::{KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const HEADER: &str = "x-clankr-workload";
pub const TTL_SECS: i64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub ns: String,
    pub agent: String,
    pub instance: String,
    pub node: String,
    pub epoch: i64,
    pub exp: i64,
}

#[derive(Clone)]
pub struct Signer([u8; 32]);

type Hmac = hmac::Hmac<Sha256>;

impl Signer {
    pub fn derive(master_key_hex: &str) -> Self {
        let mut h = Sha256::new();
        h.update(b"clankr workload identity v1\0");
        h.update(master_key_hex.as_bytes());
        Self(h.finalize().into())
    }

    fn mac(&self, body: &[u8]) -> Vec<u8> {
        let mut m = <Hmac as KeyInit>::new_from_slice(&self.0).expect("any key length");
        m.update(body);
        m.finalize().into_bytes().to_vec()
    }

    pub fn issue(&self, c: &Claims) -> String {
        let body = B64.encode(serde_json::to_vec(c).expect("serializable"));
        let sig = B64.encode(self.mac(body.as_bytes()));
        format!("{body}.{sig}")
    }

    /// Signature and expiry only; placement is checked by the caller.
    pub fn verify(&self, token: &str) -> Result<Claims, ApiError> {
        let bad = |why: &str| ApiError::new(ErrorCode::Unauthenticated, format!("workload token {why}"));
        let (body, sig) = token.split_once('.').ok_or_else(|| bad("malformed"))?;
        let sig = B64.decode(sig).map_err(|_| bad("malformed"))?;
        let mut m = <Hmac as KeyInit>::new_from_slice(&self.0).expect("any key length");
        m.update(body.as_bytes());
        m.verify_slice(&sig).map_err(|_| bad("signature invalid"))?;
        let c: Claims = serde_json::from_slice(&B64.decode(body).map_err(|_| bad("malformed"))?).map_err(|_| bad("malformed"))?;
        if c.exp < chrono::Utc::now().timestamp() {
            return Err(bad("expired"));
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_verify_tamper_expire() {
        let s = Signer::derive("abc");
        let mut c = Claims { ns: "ns".into(), agent: "a".into(), instance: "i".into(), node: "n".into(), epoch: 3, exp: chrono::Utc::now().timestamp() + 60 };
        let t = s.issue(&c);
        assert_eq!(s.verify(&t).unwrap(), c);
        assert!(Signer::derive("other").verify(&t).is_err(), "other key");
        let (body, sig) = t.split_once('.').unwrap();
        let mut forged = c.clone();
        forged.epoch = 4;
        let forged_body = B64.encode(serde_json::to_vec(&forged).unwrap());
        assert!(s.verify(&format!("{forged_body}.{sig}")).is_err(), "claims can't be edited");
        assert!(s.verify(body).is_err());
        c.exp -= 3600;
        assert_eq!(s.verify(&s.issue(&c)).unwrap_err().code, ErrorCode::Unauthenticated);
    }
}
