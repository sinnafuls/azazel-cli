//! One TLS implementation for the whole binary: rustls with the `ring` provider.
//!
//! `reqwest` is built without a provider of its own (its default one, aws-lc, is a second
//! cryptography library of over a megabyte), so it takes the process-wide one, and building a
//! client before that is installed panics. Every client is therefore made here; the workspace's
//! clippy configuration turns `reqwest::Client::new` and `reqwest::Client::builder` away.

use std::fmt;
use std::sync::Once;

use serde::{Deserialize, Serialize};

/// Makes `ring` the TLS provider of this process. Cheap to call again; a provider the embedding
/// program installed first is left alone.
pub fn install() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// `reqwest::Client::builder()`, with the TLS provider in place.
#[allow(clippy::disallowed_methods)]
pub fn client_builder() -> reqwest::ClientBuilder {
    install();
    reqwest::Client::builder()
}

/// `reqwest::Client::new()`, with the TLS provider in place.
#[allow(clippy::disallowed_methods)]
pub fn client() -> reqwest::Client {
    install();
    reqwest::Client::new()
}

/// Whether `text` is PEM itself (what a `{file:}` reference fills in) rather than a path.
pub fn is_pem(text: &str) -> bool {
    text.trim_start().starts_with("-----BEGIN")
}

/// `[providers.<id>] tls`: the certificate a provider's client presents and the authority its
/// server's certificate must come from. Each is a path to a PEM file or the PEM text itself.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSettings {
    /// The client certificate (its chain), with its private key unless `key` names that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The certificates the server's is checked against, in place of the system's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
}

impl TlsSettings {
    /// Each value that is a path made into what `path` makes of it; PEM text stays.
    pub fn anchor(&mut self, path: impl Fn(&str) -> std::path::PathBuf) {
        for value in [&mut self.cert, &mut self.key, &mut self.ca]
            .into_iter()
            .flatten()
        {
            if !is_pem(value) && !value.trim().is_empty() {
                *value = path(value).to_string_lossy().into_owned();
            }
        }
    }
}

/// A private key may be among the values: only paths are shown.
impl fmt::Debug for TlsSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = |value: &Option<String>| match value {
            Some(text) if is_pem(text) => Some("<PEM>".to_string()),
            other => other.clone(),
        };
        f.debug_struct("TlsSettings")
            .field("cert", &shown(&self.cert))
            .field("key", &shown(&self.key))
            .field("ca", &shown(&self.ca))
            .finish()
    }
}

/// The HTTPS client of one provider that presents a client certificate or trusts its own
/// authority, built once when the configuration is read.
#[derive(Clone)]
pub struct ClientTls {
    /// The PEM the client was built from: the identity, then the authority.
    pem: (Option<String>, Option<String>),
    client: reqwest::Client,
}

impl ClientTls {
    /// `builder` (with every other setting of the provider's client) with what `settings` names.
    /// An error names the setting that is wrong.
    pub fn new(settings: &TlsSettings, builder: reqwest::ClientBuilder) -> Result<Self, String> {
        let read = |name: &str, value: &Option<String>| -> Result<Option<String>, String> {
            match value.as_deref().map(str::trim) {
                None | Some("") => Ok(None),
                Some(text) if is_pem(text) => Ok(Some(text.to_string())),
                Some(path) => std::fs::read_to_string(path)
                    .map(Some)
                    .map_err(|e| format!("tls.{name}: cannot read {path}: {e}")),
            }
        };
        let (cert, key, ca) = (
            read("cert", &settings.cert)?,
            read("key", &settings.key)?,
            read("ca", &settings.ca)?,
        );
        let identity = match (cert, key) {
            (Some(cert), Some(key)) => Some(format!("{}\n{}", cert.trim_end(), key)),
            (Some(cert), None) => Some(cert),
            (None, Some(_)) => return Err("tls.key: needs `cert` beside it".to_string()),
            (None, None) => None,
        };
        if identity.is_none() && ca.is_none() {
            return Err("tls: set `cert` (and `key`), `ca`, or both".to_string());
        }
        let mut builder = builder;
        if let Some(pem) = &identity {
            let identity = reqwest::Identity::from_pem(pem.as_bytes())
                .map_err(|e| format!("tls.cert: not a certificate with its private key: {}", chain(&e)))?;
            builder = builder.identity(identity);
        }
        if let Some(pem) = &ca {
            let roots = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
                .ok()
                .filter(|roots| !roots.is_empty())
                .ok_or("tls.ca: holds no PEM certificate")?;
            builder = builder.tls_certs_only(roots);
        }
        let client = builder.build().map_err(|e| format!("tls: {}", chain(&e)))?;
        Ok(ClientTls {
            pem: (identity, ca),
            client,
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

impl PartialEq for ClientTls {
    fn eq(&self, other: &Self) -> bool {
        self.pem == other.pem
    }
}

impl fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientTls")
            .field("cert", &self.pem.0.is_some())
            .field("ca", &self.pem.1.is_some())
            .finish()
    }
}

/// An error with its causes: `reqwest` names only the outermost.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// A TLS server on the loopback interface with a certificate made for the test, so a handshake
/// can be tested without the network.
#[cfg(test)]
pub(crate) mod local {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::server::WebPkiClientVerifier;
    use rustls::{RootCertStore, ServerConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    pub(crate) struct Server {
        pub(crate) addr: SocketAddr,
        /// The server's self-signed certificate, PEM encoded: the only root a client should trust.
        pub(crate) certificate: String,
    }

    impl Server {
        pub(crate) fn url(&self) -> String {
            format!("https://localhost:{}", self.addr.port())
        }
    }

    /// Whether `request` holds the whole request: its head and as many body bytes as the head
    /// announces.
    fn complete(request: &[u8]) -> bool {
        let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
        let body = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|length| length.trim().parse::<usize>().ok())
            .unwrap_or(0);
        request.len() >= head_end + 4 + body
    }

    /// Serves `response` (a whole HTTP/1.1 response) to every request, over TLS only.
    pub(crate) async fn serve(response: &'static str) -> Server {
        serve_with(response, None).await
    }

    /// A client certificate a [`serve_mutual`] server accepts.
    pub(crate) struct ClientIdentity {
        pub(crate) cert: String,
        pub(crate) key: String,
    }

    /// Like [`serve`], but the handshake fails for a client that presents no certificate the
    /// server trusts; the one it trusts is returned.
    pub(crate) async fn serve_mutual(response: &'static str) -> (Server, ClientIdentity) {
        let mut params = rcgen::CertificateParams::new(vec!["client".to_string()]).unwrap();
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let server = serve_with(response, Some(cert.der().clone())).await;
        let identity = ClientIdentity {
            cert: cert.pem(),
            key: key.serialize_pem(),
        };
        (server, identity)
    }

    async fn serve_with(response: &'static str, client_root: Option<CertificateDer<'static>>) -> Server {
        let signed = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signed.signing_key.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = match client_root {
            None => builder.with_no_client_auth(),
            Some(root) => {
                let mut roots = RootCertStore::empty();
                roots.add(root).unwrap();
                let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                    .build()
                    .unwrap();
                builder.with_client_cert_verifier(verifier)
            }
        };
        let config = builder
            .with_single_cert(vec![signed.cert.der().clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !complete(&request) {
                        match stream.read(&mut chunk).await {
                            Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
                            _ => return,
                        }
                    }
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Server {
            addr,
            certificate: signed.cert.pem(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_client_completes_a_tls_handshake_and_reads_the_response() {
        let server =
            local::serve("HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello").await;
        let root = reqwest::Certificate::from_pem(server.certificate.as_bytes()).unwrap();
        let client = client_builder()
            .tls_certs_only([root])
            .no_proxy()
            .build()
            .unwrap();

        let response = client.get(server.url()).send().await.unwrap();

        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn a_server_the_client_has_no_root_for_is_refused() {
        let server =
            local::serve("HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello").await;
        let client = client_builder().no_proxy().build().unwrap();

        let error = client.get(server.url()).send().await.unwrap_err();

        assert!(error.is_connect(), "{error:?}");
    }

    const HELLO: &str = "HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello";

    #[tokio::test]
    async fn a_server_that_wants_a_client_certificate_takes_the_configured_one_only() {
        let (server, identity) = local::serve_mutual(HELLO).await;
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = (dir.path().join("client.pem"), dir.path().join("client.key"));
        std::fs::write(&cert, &identity.cert).unwrap();
        std::fs::write(&key, &identity.key).unwrap();
        // Paths, or the PEM itself as a `{file:}` reference fills it in.
        let by_path = TlsSettings {
            cert: Some(cert.to_string_lossy().into_owned()),
            key: Some(key.to_string_lossy().into_owned()),
            ca: Some(server.certificate.clone()),
        };
        let inline = TlsSettings {
            cert: Some(format!("{}{}", identity.cert, identity.key)),
            key: None,
            ca: Some(server.certificate.clone()),
        };
        for settings in [by_path, inline] {
            let tls = ClientTls::new(&settings, client_builder().no_proxy()).unwrap();
            let response = tls.client().get(server.url()).send().await.unwrap();
            assert_eq!(response.text().await.unwrap(), "hello");
        }

        // Trusting the server is not enough without the certificate.
        let only_ca = TlsSettings {
            ca: Some(server.certificate.clone()),
            ..TlsSettings::default()
        };
        let tls = ClientTls::new(&only_ca, client_builder().no_proxy()).unwrap();
        let refused = match tls.client().get(server.url()).send().await {
            Err(_) => true,
            Ok(response) => response.text().await.is_err(),
        };
        assert!(refused);
    }

    #[test]
    fn a_setting_that_cannot_be_used_is_named() {
        let error = |settings: TlsSettings| ClientTls::new(&settings, client_builder()).unwrap_err();
        assert!(error(TlsSettings::default()).starts_with("tls:"));
        let missing = TlsSettings {
            cert: Some("/no/such/client.pem".to_string()),
            ..TlsSettings::default()
        };
        assert!(error(missing).starts_with("tls.cert: cannot read /no/such/client.pem"));
        let lone_key = TlsSettings {
            key: Some("-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----".to_string()),
            ..TlsSettings::default()
        };
        assert!(error(lone_key).starts_with("tls.key:"));
        let not_pem = TlsSettings {
            ca: Some("-----BEGIN NOTHING-----".to_string()),
            ..TlsSettings::default()
        };
        assert!(error(not_pem).starts_with("tls.ca:"));
    }
}
