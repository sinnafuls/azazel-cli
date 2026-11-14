//! One TLS implementation for the whole binary: rustls with the `ring` provider.
//!
//! `reqwest` is built without a provider of its own (its default one, aws-lc, is a second
//! cryptography library of over a megabyte), so it takes the process-wide one, and building a
//! client before that is installed panics. Every client is therefore made here; the workspace's
//! clippy configuration turns `reqwest::Client::new` and `reqwest::Client::builder` away.

use std::sync::Once;

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

/// A TLS server on the loopback interface with a certificate made for the test, so a handshake
/// can be tested without the network.
#[cfg(test)]
pub(crate) mod local {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use rustls::ServerConfig;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
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
        let signed = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signed.signing_key.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
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
}
