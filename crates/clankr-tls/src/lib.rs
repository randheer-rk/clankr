//! Cluster PKI (SEC-04, CL-31-02): a private CA owned by clankrd, a server certificate for the
//! API, and client certificates for nodes issued from node-generated CSRs, so node private keys
//! never leave the node.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

pub const NODE_CERT_DAYS: i64 = 30;

/// SHA-256 of a certificate's DER bytes, hex. Identifies the exact certificate a peer used.
pub fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// SHA-256 fingerprint of the first certificate in a PEM blob.
pub fn fingerprint_pem(pem: &str) -> Result<String> {
    let der = CertificateDer::pem_slice_iter(pem.as_bytes()).next().context("no certificate in PEM")??;
    Ok(fingerprint(&der))
}

fn write_private(path: &Path, data: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

fn validity(p: &mut CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::minutes(5); // tolerate small clock skew
    p.not_after = now + time::Duration::days(days);
}

pub struct Ca {
    pub cert_pem: String,
    key: KeyPair,
}

impl Ca {
    /// Loads `dir/ca.crt` + `dir/ca.key`, creating a new 10-year CA on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let (crt, key) = (dir.join("ca.crt"), dir.join("ca.key"));
        if crt.exists() && key.exists() {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&key)?.permissions().mode() & 0o077 != 0 {
                bail!("{} must be chmod 600", key.display());
            }
            return Ok(Self { cert_pem: std::fs::read_to_string(&crt)?, key: KeyPair::from_pem(&std::fs::read_to_string(&key)?)? });
        }
        std::fs::create_dir_all(dir)?;
        let key_pair = KeyPair::generate()?;
        let mut p = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "clankr cluster CA");
        p.distinguished_name = dn;
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        validity(&mut p, 3650);
        let cert = p.self_signed(&key_pair)?;
        write_private(&key, &key_pair.serialize_pem())?;
        std::fs::write(&crt, cert.pem())?;
        Ok(Self { cert_pem: cert.pem(), key: key_pair })
    }

    fn issuer(&self) -> Result<Issuer<'_, &KeyPair>> {
        Ok(Issuer::from_ca_cert_pem(&self.cert_pem, &self.key)?)
    }

    /// Server certificate for the API, valid for `names` (DNS names or IP literals).
    pub fn server_cert(&self, names: &[String]) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(names.to_vec())?;
        p.distinguished_name.push(DnType::CommonName, "clankrd");
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        validity(&mut p, 365);
        let cert = p.signed_by(&key, &self.issuer()?)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Signs a node's CSR. Only the CSR's public key (and its proof of possession) is used;
    /// subject, usages and lifetime are ours: CN = node name, client-auth only.
    pub fn sign_node_csr(&self, csr_pem: &str, node: &str) -> Result<String> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("invalid CSR (bad PEM or signature)")?;
        let mut p = CertificateParams::default();
        p.distinguished_name.push(DnType::CommonName, node);
        p.subject_alt_names = vec![SanType::DnsName(format!("{node}.node.clankr").try_into()?)];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        validity(&mut p, NODE_CERT_DAYS);
        Ok(p.signed_by(&csr.public_key, &self.issuer()?)?.pem())
    }

    /// rustls server config: our server cert, client certs optional (owners use bearer tokens)
    /// but verified against the CA whenever presented.
    pub fn server_config(&self, cert_pem: &str, key_pem: &str) -> Result<Arc<rustls::ServerConfig>> {
        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(self.cert_pem.as_bytes()) {
            roots.add(c?)?;
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).allow_unauthenticated().build()?;
        let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())?;
        let mut cfg = rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_client_cert_verifier(verifier).with_single_cert(certs, key)?;
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }
}

/// A node's private key (kept local) and the CSR it sends when joining.
pub fn node_key_and_csr(node: &str) -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::default();
    p.distinguished_name.push(DnType::CommonName, node);
    let csr = p.serialize_request(&key)?;
    Ok((key.serialize_pem(), csr.pem()?))
}

/// Expiry of the first certificate in a PEM blob (unix seconds).
pub fn not_after_pem(pem: &str) -> Result<i64> {
    let der = CertificateDer::pem_slice_iter(pem.as_bytes()).next().context("no certificate in PEM")??;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(|e| anyhow::anyhow!("parse certificate: {e}"))?;
    Ok(cert.validity().not_after.timestamp())
}

/// Common name of a DER certificate, for logs and audit.
pub fn common_name(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    cert.subject().iter_common_name().next()?.as_str().ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_signs_node_csr_with_our_terms() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();
        let again = Ca::load_or_create(dir.path()).unwrap();
        assert_eq!(ca.cert_pem, again.cert_pem, "CA persists");
        let (key, csr) = node_key_and_csr("node-a").unwrap();
        assert!(key.contains("PRIVATE KEY") && csr.contains("CERTIFICATE REQUEST"));
        let cert = ca.sign_node_csr(&csr, "node-a").unwrap();
        let der = CertificateDer::pem_slice_iter(cert.as_bytes()).next().unwrap().unwrap();
        assert_eq!(common_name(&der).as_deref(), Some("node-a"));
        assert_eq!(fingerprint_pem(&cert).unwrap(), fingerprint(&der));
        let left = not_after_pem(&cert).unwrap() - time::OffsetDateTime::now_utc().unix_timestamp();
        assert!((NODE_CERT_DAYS * 86400 - 60..=NODE_CERT_DAYS * 86400 + 60).contains(&left), "{left}");
        assert!(ca.sign_node_csr("not a csr", "x").is_err());
        let (scert, skey) = ca.server_cert(&["localhost".into(), "127.0.0.1".into()]).unwrap();
        ca.server_config(&scert, &skey).unwrap();
    }
}
