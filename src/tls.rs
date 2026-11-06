//! A throwaway certificate authority and server certificate for serving HTTPS locally.

use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

/// File name a fake writes the certificate authority's certificate under, in its data
/// directory.
pub const CA_FILE: &str = "ca.pem";

/// TLS material for one server: clients trust `ca_pem` to accept its certificate.
pub struct Tls {
    pub ca_pem: String,
    pub config: Arc<ServerConfig>,
}

/// Creates a certificate authority named `name` and a certificate for `localhost` and
/// `127.0.0.1` signed by it.
pub fn generate(name: &str) -> Result<Tls, String> {
    let error = |error: rcgen::Error| error.to_string();
    let ca_key = KeyPair::generate().map_err(error)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(error)?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, name);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = ca_params.self_signed(&ca_key).map_err(error)?;
    let issuer = Issuer::new(ca_params, ca_key);

    let key = KeyPair::generate().map_err(error)?;
    let certificate = server_params()
        .map_err(error)?
        .signed_by(&key, &issuer)
        .map_err(error)?;

    let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.der().clone(), ca.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .map_err(|error| error.to_string())?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Tls {
        ca_pem: ca.pem(),
        config: Arc::new(config),
    })
}

/// Parameters for the `localhost` server certificate. Python 3.13+ verifies with
/// `VERIFY_X509_STRICT`, which rejects a leaf without an authority key identifier.
fn server_params() -> Result<CertificateParams, rcgen::Error> {
    let mut params = CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    params.use_authority_key_identifier_extension = true;
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DER encoding of the authority key identifier extension's OID, 2.5.29.35.
    const AUTHORITY_KEY_IDENTIFIER_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x1d, 0x23];

    #[test]
    fn server_certificate_names_its_issuer_key() {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let issuer = Issuer::new(ca_params, ca_key);

        let certificate = server_params()
            .unwrap()
            .signed_by(&KeyPair::generate().unwrap(), &issuer)
            .unwrap();

        assert!(
            certificate
                .der()
                .windows(AUTHORITY_KEY_IDENTIFIER_OID.len())
                .any(|window| window == AUTHORITY_KEY_IDENTIFIER_OID)
        );
    }
}
