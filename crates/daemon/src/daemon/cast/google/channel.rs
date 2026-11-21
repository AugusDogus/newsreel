//! The TLS connection to a Cast device on port 8009, and its heartbeat.
//!
//! Cast devices present certificates of their own making, or ones that chain to Google's device
//! authorities, which no system trusts. So any certificate is taken, but the handshake must still
//! be signed by the key it names. This follows mitsuhiko/tvpipe's `cast/channel.rs`
//! (Apache-2.0), which follows Chromium's Open Screen and pychromecast.
//!
//! One task owns the connection: it answers the device's PING with PONG, sends a PING every five
//! seconds, and passes every other message on. A device silent for 20 seconds has gone, and so
//! has one whose connection breaks; either way the task ends and the inbox closes.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::frame::{self, Message};

pub const HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
/// The receiver end of the device itself, as against an app running on it.
pub const PLATFORM: &str = "receiver-0";
/// How often a PING goes to the device.
const PING_EVERY: Duration = Duration::from_secs(5);
/// A device that sends nothing for this long has gone.
const SILENT_FOR: Duration = Duration::from_secs(20);
const CONNECT_WITHIN: Duration = Duration::from_secs(5);

/// A connection to a Cast device. Dropping it closes the connection.
pub struct Channel {
    out: mpsc::UnboundedSender<Message>,
    inbox: mpsc::UnboundedReceiver<Message>,
    task: JoinHandle<()>,
}

impl Channel {
    pub async fn connect(address: SocketAddr) -> io::Result<Channel> {
        let tcp = tokio::time::timeout(CONNECT_WITHIN, TcpStream::connect(address))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the connection was not answered within 5 s"))??;
        tcp.set_nodelay(true)?;
        let tls = tokio_rustls::TlsConnector::from(config()?);
        let name = ServerName::IpAddress(address.ip().into());
        let stream = tokio::time::timeout(CONNECT_WITHIN, tls.connect(name, tcp))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the TLS handshake did not finish within 5 s"))??;
        let (out, outgoing) = mpsc::unbounded_channel();
        let (deliver, inbox) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(stream, outgoing, deliver));
        Ok(Channel { out, inbox, task })
    }

    /// Queues `message`. Fails once the connection has closed.
    pub fn send(&self, message: Message) -> io::Result<()> {
        self.out.send(message).map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    /// The next message from the device, or `None` once the connection has closed.
    pub async fn recv(&mut self) -> Option<Message> {
        self.inbox.recv().await
    }

    pub fn closed(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run<S>(stream: S, mut outgoing: mpsc::UnboundedReceiver<Message>, deliver: mpsc::UnboundedSender<Message>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut read, mut write) = tokio::io::split(stream);
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heard = tokio::time::Instant::now();
    loop {
        let sending = tokio::select! {
            read = read.read(&mut chunk) => {
                let Ok(read @ 1..) = read else { return };
                heard = tokio::time::Instant::now();
                buffer.extend_from_slice(&chunk[..read]);
                let Ok(messages) = frame::split(&mut buffer) else { return };
                let mut answers = Vec::new();
                for message in messages {
                    if message.namespace == HEARTBEAT {
                        if message.payload.contains("\"PING\"") {
                            answers.push(heartbeat(&message.destination, &message.source, "PONG"));
                        }
                    } else if deliver.send(message).is_err() {
                        return;
                    }
                }
                answers
            }
            message = outgoing.recv() => match message {
                Some(message) => vec![message],
                None => return,
            },
            _ = ping.tick() => {
                if heard.elapsed() >= SILENT_FOR {
                    return;
                }
                vec![heartbeat("sender-0", PLATFORM, "PING")]
            }
        };
        for message in sending {
            if write.write_all(&message.framed()).await.is_err() {
                return;
            }
        }
        if write.flush().await.is_err() {
            return;
        }
    }
}

fn heartbeat(source: &str, destination: &str, kind: &str) -> Message {
    Message {
        source: source.into(),
        destination: destination.into(),
        namespace: HEARTBEAT.into(),
        payload: format!("{{\"type\":\"{kind}\"}}"),
    }
}

fn config() -> io::Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
        .map_err(io::Error::other)
}

/// Takes any certificate, but checks that the handshake is signed by its key.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
