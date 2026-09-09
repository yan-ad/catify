use std::{fmt, io, net::SocketAddr, path::Path, sync::Arc};

use thiserror::Error;
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinError, JoinHandle, JoinSet},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
};

/// A loopback-only TLS terminator that forwards decrypted bytes to a TCP backend.
///
/// Dropping the proxy requests shutdown and releases the listener without
/// waiting. Call [`TlsProxy::stop`] when orderly, observable cleanup is
/// required before continuing.
pub struct TlsProxy {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl fmt::Debug for TlsProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsProxy")
            .field("local_addr", &self.local_addr)
            .field("running", &self.task.is_some())
            .finish()
    }
}

/// Errors returned while configuring or running a [`TlsProxy`].
#[derive(Debug, Error)]
pub enum TlsProxyError {
    #[error("TLS proxy listen address must be loopback: {0}")]
    NonLoopbackListen(SocketAddr),
    #[error("TLS proxy backend address must be loopback: {0}")]
    NonLoopbackBackend(SocketAddr),
    #[error("failed to read TLS certificate file")]
    ReadCertificate(#[source] io::Error),
    #[error("TLS certificate file contains an invalid PEM certificate")]
    InvalidCertificate(#[source] io::Error),
    #[error("TLS certificate file contains no certificates")]
    MissingCertificate,
    #[error("failed to read TLS private key file")]
    ReadPrivateKey(#[source] io::Error),
    #[error("TLS private key file contains an invalid PEM key")]
    InvalidPrivateKey(#[source] io::Error),
    #[error("TLS private key file contains no supported private key")]
    MissingPrivateKey,
    #[error("TLS certificate and private key are incompatible")]
    InvalidIdentity(#[source] tokio_rustls::rustls::Error),
    #[error("failed to bind TLS proxy listener at {address}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("TLS proxy task failed")]
    Task(#[source] JoinError),
    #[error("TLS proxy listener failed")]
    Listener(#[source] io::Error),
}

impl TlsProxy {
    /// Starts a TLS proxy. Port zero may be used to request an ephemeral port;
    /// the selected address is available through [`TlsProxy::local_addr`].
    pub async fn start(
        listen: SocketAddr,
        backend: SocketAddr,
        certificate_path: impl AsRef<Path>,
        private_key_path: impl AsRef<Path>,
    ) -> Result<Self, TlsProxyError> {
        if !listen.ip().is_loopback() {
            return Err(TlsProxyError::NonLoopbackListen(listen));
        }
        if !backend.ip().is_loopback() {
            return Err(TlsProxyError::NonLoopbackBackend(backend));
        }

        let certificates = load_certificates(certificate_path.as_ref())?;
        let private_key = load_private_key(private_key_path.as_ref())?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .map_err(TlsProxyError::InvalidIdentity)?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|source| TlsProxyError::Bind {
                address: listen,
                source,
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| TlsProxyError::Bind {
                address: listen,
                source,
            })?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run(listener, backend, acceptor, shutdown_rx));

        Ok(Self {
            local_addr,
            shutdown,
            task: Some(task),
        })
    }

    /// Returns the bound listener address, including an OS-selected port.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting connections, closes active connections, and waits until
    /// all proxy tasks have been cleaned up. Calling this more than once is safe.
    pub async fn stop(&mut self) -> Result<(), TlsProxyError> {
        let _ = self.shutdown.send(true);
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(TlsProxyError::Task)?
            .map_err(TlsProxyError::Listener)
    }
}

impl Drop for TlsProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        // A drop cannot await graceful cleanup. Signalling lets an actively
        // polled accept loop finish normally, while aborting ensures the
        // listener is released even when Windows has not yet scheduled that
        // task (for example, during test-runtime teardown).
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsProxyError> {
    let certificates = CertificateDer::pem_file_iter(path)
        .map_err(|error| match error {
            tokio_rustls::rustls::pki_types::pem::Error::Io(error) => {
                TlsProxyError::ReadCertificate(error)
            }
            error => TlsProxyError::InvalidCertificate(pem_error(error)),
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsProxyError::InvalidCertificate(pem_error(error)))?;
    if certificates.is_empty() {
        return Err(TlsProxyError::MissingCertificate);
    }
    Ok(certificates)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsProxyError> {
    PrivateKeyDer::from_pem_file(path).map_err(|error| match error {
        tokio_rustls::rustls::pki_types::pem::Error::NoItemsFound => {
            TlsProxyError::MissingPrivateKey
        }
        tokio_rustls::rustls::pki_types::pem::Error::Io(error) => {
            TlsProxyError::ReadPrivateKey(error)
        }
        error => TlsProxyError::InvalidPrivateKey(pem_error(error)),
    })
}

fn pem_error(error: tokio_rustls::rustls::pki_types::pem::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

async fn run(
    listener: TcpListener,
    backend: SocketAddr,
    acceptor: TlsAcceptor,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (client, _) = accepted?;
                let acceptor = acceptor.clone();
                connections.spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(client).await
                        && let Ok(mut upstream) = TcpStream::connect(backend).await
                    {
                        let _ = copy_bidirectional(&mut tls, &mut upstream).await;
                    }
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
    Ok(())
}
