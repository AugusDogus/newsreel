//! A fake Cast device for tests: it speaks CASTV2 over TLS on a local port, runs a Default Media
//! Receiver that plays in real time, and fetches what it is told to load the way a Google TV
//! does: an MP4 with a Range request, an HLS film by its playlist, init segment and segments,
//! from a page on another origin. Its "remote" pauses, seeks and stops as the TV's own does.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::channel::HEARTBEAT;
use super::frame::{self, Message};
use super::mdns::Heard;
use super::{CONNECTION, DEFAULT_RECEIVER, MEDIA, RECEIVER};

const ORIGIN: &str = "https://www.gstatic.com";

/// What the fake device has been asked, and what it plays.
#[derive(Default)]
pub struct Screen {
    /// Each request's namespace's last word and its type, such as `receiver LAUNCH`, oldest first.
    pub asked: Vec<String>,
    pub app: Option<(String, String)>,
    pub sessions: u32,
    pub media: Option<Playing>,
    pub volume: f64,
    /// The LOADs, oldest first.
    pub loads: Vec<Value>,
    /// Each fetch of what it loaded: the URL's path, and the status.
    pub fetches: Vec<String>,
    /// LOADs it fails, as a receiver that cannot play the video does.
    pub refuse_loads: bool,
    /// Seeks it answers but does not do.
    pub ignored_seeks: u32,
    /// It starts every video at the top, whatever time the LOAD asks for.
    pub starts_at_top: bool,
    /// It fails an HLS film whose video is taller than this, as a Google TV on a 720p screen
    /// does.
    pub tallest_film: Option<u32>,
    next_media: i64,
}

pub struct Playing {
    pub id: i64,
    pub url: String,
    pub state: &'static str,
    pub idle: Option<&'static str>,
    /// Where it was at `since`, playing on from there unless paused.
    pub at: f64,
    pub since: Instant,
    pub duration: Option<f64>,
}

impl Playing {
    pub fn time(&self) -> f64 {
        if self.state == "PLAYING" { self.at + self.since.elapsed().as_secs_f64() } else { self.at }
    }

    fn set(&mut self, state: &'static str) {
        self.at = self.time();
        self.since = Instant::now();
        self.state = state;
    }
}

#[derive(Clone)]
pub struct FakeCast {
    pub at: SocketAddr,
    pub screen: Arc<Mutex<Screen>>,
    /// Counts the statuses every connection is to send, and the times every one is to hang up.
    broadcast: watch::Sender<(u64, u64)>,
}

impl FakeCast {
    pub async fn start(ip: IpAddr) -> FakeCast {
        let listener = TcpListener::bind((ip, 0)).await.unwrap();
        let at = listener.local_addr().unwrap();
        let screen = Arc::new(Mutex::new(Screen { volume: 0.5, ..Screen::default() }));
        let (broadcast, _) = watch::channel((0, 0));
        let fake = FakeCast { at, screen, broadcast };
        let acceptor = tokio_rustls::TlsAcceptor::from(config());
        let served = fake.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, fake) = (acceptor.clone(), served.clone());
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(tcp).await {
                        fake.serve(tls).await;
                    }
                });
            }
        });
        fake
    }

    /// The device as an mDNS browse finds it.
    pub fn heard(&self, name: &str) -> Heard {
        Heard {
            id: format!("fake{}", self.at.port()),
            name: name.into(),
            model: "Chromecast".into(),
            at: self.at,
            capabilities: 464901,
        }
    }

    pub fn screen(&self) -> MutexGuard<'_, Screen> {
        self.screen.lock().unwrap()
    }

    pub fn asked(&self) -> Vec<String> {
        self.screen().asked.clone()
    }

    /// Presses a button on the TV's own remote: `PAUSE`, `PLAY`, `STOP`, or a seek.
    pub fn remote(&self, button: &str, seek: Option<f64>) {
        {
            let mut screen = self.screen();
            let Some(media) = screen.media.as_mut() else { return };
            match (button, seek) {
                (_, Some(to)) => {
                    media.at = to;
                    media.since = Instant::now();
                }
                ("STOP", _) => {
                    media.set("IDLE");
                    media.idle = Some("CANCELLED");
                }
                (state, _) => media.set(if state == "PAUSE" { "PAUSED" } else { "PLAYING" }),
            }
        }
        self.broadcast.send_modify(|(statuses, _)| *statuses += 1);
    }

    /// Seeks with the TV's own remote, and fetches the segment there for an HLS film.
    pub async fn remote_seek(&self, to: f64) {
        self.remote("", Some(to));
        let url = self.screen().media.as_ref().map(|media| media.url.clone());
        if let Some(url) = url.filter(|url| url.contains(".m3u8")) {
            self.fetch_segment_at(&url, to).await;
        }
    }

    /// Breaks every connection, as Wi-Fi that drops for a moment does. It plays on.
    pub fn hang_up(&self) {
        self.broadcast.send_modify(|(_, hang_ups)| *hang_ups += 1);
    }

    pub async fn until(&self, what: &str, done: impl Fn(&Screen) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(&self.screen()) {
            assert!(Instant::now() < deadline, "the fake Cast device never saw {what}: {:?}", self.asked());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn serve(&self, tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>) {
        let (mut read, mut write) = tokio::io::split(tls);
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        let mut hang_up = self.broadcast.subscribe();
        let hung_up = hang_up.borrow().1;
        let mut senders: Vec<String> = Vec::new();
        let writer = tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if write.write_all(&message.framed()).await.is_err() {
                    return;
                }
            }
        });
        let mut buffer = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            tokio::select! {
                read = read.read(&mut chunk) => {
                    let Ok(read @ 1..) = read else { break };
                    buffer.extend_from_slice(&chunk[..read]);
                    let Ok(messages) = frame::split(&mut buffer) else { break };
                    for message in messages {
                        if !senders.contains(&message.source) {
                            senders.push(message.source.clone());
                        }
                        // A receiver answers a LOAD once it has read the video, and everything else
                        // meanwhile.
                        if message.namespace == MEDIA && message.payload.contains("\"LOAD\"") {
                            let (fake, out) = (self.clone(), out.clone());
                            tokio::spawn(async move {
                                for answer in fake.answer(&message).await {
                                    let _ = out.send(answer);
                                }
                            });
                        } else {
                            for answer in self.answer(&message).await {
                                let _ = out.send(answer);
                            }
                        }
                    }
                }
                changed = hang_up.changed() => {
                    if changed.is_err() || hang_up.borrow().1 != hung_up {
                        break;
                    }
                    for sender in &senders {
                        let _ = out.send(self.media_status(sender, 0));
                    }
                }
            }
        }
        writer.abort();
    }

    async fn answer(&self, message: &Message) -> Vec<Message> {
        let payload: Value = serde_json::from_str(&message.payload).unwrap_or_default();
        let kind = payload["type"].as_str().unwrap_or_default().to_string();
        let id = payload["requestId"].as_u64().unwrap_or(0);
        let to = |namespace: &str, from: &str, body: Value| Message {
            source: from.to_string(),
            destination: message.source.clone(),
            namespace: namespace.to_string(),
            payload: body.to_string(),
        };
        if message.namespace == HEARTBEAT {
            return if kind == "PING" {
                vec![to(HEARTBEAT, &message.destination, json!({ "type": "PONG" }))]
            } else {
                vec![]
            };
        }
        let word = message.namespace.rsplit('.').next().unwrap_or_default();
        self.screen().asked.push(format!("{word} {kind}"));
        match (message.namespace.as_str(), kind.as_str()) {
            (CONNECTION, _) => vec![],
            (RECEIVER, "LAUNCH") => {
                let mut screen = self.screen();
                if screen.app.is_none() {
                    screen.sessions += 1;
                    let n = screen.sessions;
                    screen.app = Some((format!("session-{n}"), format!("web-{n}")));
                }
                drop(screen);
                vec![to(RECEIVER, "receiver-0", self.receiver_status(id))]
            }
            (RECEIVER, "GET_STATUS") => vec![to(RECEIVER, "receiver-0", self.receiver_status(id))],
            (RECEIVER, "SET_VOLUME") => {
                self.screen().volume = payload["volume"]["level"].as_f64().unwrap_or(0.5);
                vec![to(RECEIVER, "receiver-0", self.receiver_status(id))]
            }
            (RECEIVER, "STOP") => {
                let mut screen = self.screen();
                let transport = screen.app.take().map(|(_, transport)| transport).unwrap_or_default();
                screen.media = None;
                drop(screen);
                vec![
                    to(RECEIVER, "receiver-0", self.receiver_status(id)),
                    to(CONNECTION, &transport, json!({ "type": "CLOSE" })),
                ]
            }
            (MEDIA, "LOAD") => {
                let url = payload["media"]["contentId"].as_str().unwrap_or_default().to_string();
                let refused = {
                    let mut screen = self.screen();
                    screen.loads.push(payload.clone());
                    screen.refuse_loads
                };
                if refused {
                    return vec![to(MEDIA, &message.destination, json!({ "type": "LOAD_FAILED", "requestId": id }))];
                }
                let hls = payload["media"]["contentType"].as_str() == Some(super::super::super::media::hls::PLAYLIST);
                let fetched = self.fetch(&url, hls).await;
                let mut screen = self.screen();
                screen.next_media += 1;
                let media_id = screen.next_media;
                let start = if screen.starts_at_top { 0.0 } else { payload["currentTime"].as_f64().unwrap_or(0.0) };
                // A video it was not told the length of, it reads the length of.
                let duration = payload["media"]["duration"].as_f64().or(Some(5760.0));
                screen.media = Some(Playing {
                    id: media_id,
                    url,
                    state: if fetched { "PLAYING" } else { "IDLE" },
                    idle: (!fetched).then_some("ERROR"),
                    at: start,
                    since: Instant::now(),
                    duration,
                });
                drop(screen);
                vec![self.media_status(&message.source, id)]
            }
            (MEDIA, "GET_STATUS") => vec![self.media_status(&message.source, id)],
            (MEDIA, "PAUSE" | "PLAY" | "STOP") => {
                if let Some(media) = self.screen().media.as_mut() {
                    match kind.as_str() {
                        "PAUSE" => media.set("PAUSED"),
                        "PLAY" => media.set("PLAYING"),
                        _ => {
                            media.set("IDLE");
                            media.idle = Some("CANCELLED");
                        }
                    }
                }
                vec![self.media_status(&message.source, id)]
            }
            (MEDIA, "SEEK") => {
                let to_time = payload["currentTime"].as_f64().unwrap_or(0.0);
                let url = {
                    let mut screen = self.screen();
                    if screen.ignored_seeks > 0 {
                        screen.ignored_seeks -= 1;
                    } else if let Some(media) = screen.media.as_mut() {
                        media.at = to_time;
                        media.since = Instant::now();
                    }
                    screen.media.as_ref().map(|media| media.url.clone())
                };
                // An HLS film fetches the segment at the new place.
                if let Some(url) = url.filter(|url| url.contains(".m3u8")) {
                    self.fetch_segment_at(&url, to_time).await;
                }
                vec![self.media_status(&message.source, id)]
            }
            _ => vec![],
        }
    }

    fn receiver_status(&self, id: u64) -> Value {
        let screen = self.screen();
        let applications: Vec<Value> = screen
            .app
            .iter()
            .map(|(session, transport)| json!({ "appId": DEFAULT_RECEIVER, "sessionId": session, "transportId": transport, "displayName": "Default Media Receiver" }))
            .collect();
        json!({ "type": "RECEIVER_STATUS", "requestId": id, "status": { "applications": applications, "volume": { "level": screen.volume, "muted": false } } })
    }

    fn media_status(&self, to: &str, id: u64) -> Message {
        let screen = self.screen();
        let transport = screen.app.as_ref().map_or("receiver-0".to_string(), |(_, transport)| transport.clone());
        let status: Vec<Value> = screen
            .media
            .iter()
            .map(|media| {
                let mut status = json!({
                    "mediaSessionId": media.id,
                    "playerState": media.state,
                    "currentTime": media.time(),
                    "supportedMediaCommands": 274447,
                    "media": { "contentId": media.url, "duration": media.duration },
                });
                if let Some(idle) = media.idle {
                    status["idleReason"] = json!(idle);
                }
                status
            })
            .collect();
        Message {
            source: transport,
            destination: to.to_string(),
            namespace: MEDIA.to_string(),
            payload: json!({ "type": "MEDIA_STATUS", "requestId": id, "status": status }).to_string(),
        }
    }

    /// Fetches what a LOAD names, as the receiver does before it plays: the first bytes of a
    /// video, or an HLS film's playlist, init segment and first segment.
    async fn fetch(&self, url: &str, hls: bool) -> bool {
        if !hls {
            return self.get(url, Some("bytes=0-1023")).await.is_some();
        }
        let start = self.screen().loads.last().and_then(|load| load["currentTime"].as_f64()).unwrap_or(0.0);
        self.fetch_segment_at(url, start).await
    }

    async fn fetch_segment_at(&self, playlist_url: &str, at: f64) -> bool {
        let Some(playlist) = self.get(playlist_url, None).await else { return false };
        let playlist = String::from_utf8_lossy(&playlist).into_owned();
        let base = playlist_url.rsplit_once('/').map_or(playlist_url, |(base, _)| base);
        // The segment that holds `at`, by the lengths the playlist gives.
        let (mut start, mut segment) = (0.0, None);
        let mut lines = playlist.lines();
        while let Some(line) = lines.next() {
            if let Some(length) =
                line.strip_prefix("#EXTINF:").and_then(|l| l.trim_end_matches(',').parse::<f64>().ok())
            {
                let name = lines.next().unwrap_or_default();
                if at < start + length || segment.is_none() {
                    segment = Some(name.to_string());
                }
                if at < start + length {
                    break;
                }
                start += length;
            }
        }
        let Some(segment) = segment else { return false };
        let Some(init) = self.get(&format!("{base}/init.mp4"), None).await else { return false };
        let tallest = self.screen().tallest_film;
        if tallest.is_some_and(|tallest| height(&init) > tallest) {
            return false;
        }
        self.get(&format!("{base}/{segment}"), None).await.is_some()
    }

    async fn get(&self, url: &str, range: Option<&str>) -> Option<Vec<u8>> {
        let client = reqwest::Client::builder().no_proxy().build().ok()?;
        let mut request = client.get(url).header("Origin", ORIGIN);
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        let path = url.split_once("/cast/").map_or(url.to_string(), |(_, path)| {
            format!("/cast/{}", path.split_once('/').map_or("", |(_, name)| name))
        });
        let answer = request.send().await;
        let (status, body) = match answer {
            Ok(answer) => {
                let status = answer.status().as_u16();
                let cors = answer.headers().get("access-control-allow-origin").is_some();
                let body = answer.bytes().await.ok().map(|b| b.to_vec());
                (if cors { status } else { 999 }, body)
            }
            Err(_) => (0, None),
        };
        self.screen().fetches.push(format!("{path} {status}"));
        body.filter(|_| (200..300).contains(&status))
    }
}

/// The height of the tallest track an init segment's `tkhd` boxes give, which is the video's.
fn height(init: &[u8]) -> u32 {
    init.windows(4)
        .enumerate()
        .filter(|(_, kind)| *kind == b"tkhd")
        .filter_map(|(at, _)| {
            let size = u32::from_be_bytes(init.get(at - 4..at)?.try_into().ok()?) as usize;
            let end = at - 4 + size;
            Some(u32::from_be_bytes(init.get(end - 4..end)?.try_into().ok()?) >> 16)
        })
        .max()
        .unwrap_or(0)
}

fn config() -> Arc<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cert = CertificateDer::from(include_bytes!("testdata/cert.der").to_vec());
    let key = PrivateKeyDer::try_from(include_bytes!("testdata/key.der").to_vec()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    Arc::new(config)
}
