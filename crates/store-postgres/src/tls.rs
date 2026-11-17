//! TLS for the Postgres client, through rustls and ring.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsConnector;

/// Makes each connection's TLS, 1.3 only, checking the server's certificate
/// against a set of roots, and its name.
#[derive(Clone)]
pub(crate) struct Tls(Arc<ClientConfig>);

impl Tls {
    pub(crate) fn new(roots: Arc<RootCertStore>) -> Result<Self, rustls::Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self(Arc::new(config)))
    }
}

impl<S> MakeTlsConnect<S> for Tls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = Stream<S>;
    type TlsConnect = Connect;
    type Error = rustls::pki_types::InvalidDnsNameError;

    fn make_tls_connect(&mut self, host: &str) -> Result<Connect, Self::Error> {
        Ok(Connect {
            config: Arc::clone(&self.0),
            name: ServerName::try_from(host.to_owned())?,
        })
    }
}

/// One connection's TLS, to the server of one name.
pub(crate) struct Connect {
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
}

impl<S> TlsConnect<S> for Connect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = Stream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Stream<S>>> + Send>>;

    fn connect(self, socket: S) -> Self::Future {
        let connector = TlsConnector::from(self.config);
        Box::pin(async move { connector.connect(self.name, socket).await.map(Stream) })
    }
}

/// A connection over TLS.
pub(crate) struct Stream<S>(tokio_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream for Stream<S> {
    /// None: SCRAM is spoken without binding to the channel. The server is
    /// already proven by its certificate, which is checked.
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Stream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(context, buffer)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Stream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(context, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(context)
    }
}
