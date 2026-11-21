//! Google Cast: Chromecasts, Google TVs, and TVs with Cast built in. A cast launches the Default
//! Media Receiver on the device and loads newsreel's URL into it, so the device fetches, buffers
//! and seeks by itself, and its own remote works. The engine keeps a connection to the device
//! for the cast's length, asks where it is every second, and sends what the phone asks.
//!
//! The protocol is CASTV2: TLS on port 8009, and JSON in protobuf frames on a few namespaces:
//! `tp.connection` opens virtual connections, `tp.heartbeat` keeps them, `receiver` launches and
//! stops apps and sets the volume, and `media` loads and drives what plays.

pub mod channel;
pub mod frame;
pub mod mdns;

use std::net::SocketAddr;
use std::time::Duration;

use newsreel_core::{Error, ErrorCode, Playback};
use serde_json::{Value, json};

use super::super::media::Media;
use super::super::media::ffmpeg::Picture;
use super::upnp::Failed;
use channel::{Channel, PLATFORM};
use frame::Message;
pub use mdns::Heard;

/// The Default Media Receiver, which plays a URL it is given.
pub const DEFAULT_RECEIVER: &str = "CC1AD845";
const CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const MEDIA: &str = "urn:x-cast:com.google.cast.media";
/// How long a device may take to answer a question.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);
/// How long the receiver may take to read the start of a video and answer its LOAD. A video it
/// has not answered for by then is followed as it plays, or not.
const LOAD_WITHIN: Duration = Duration::from_secs(90);
/// How long the Default Media Receiver may take to start. A device that was idle shows its
/// launch screen first, which took 7 s on a Chromecast with Google TV.
const LAUNCH_WITHIN: Duration = Duration::from_secs(30);

/// Where searches for Cast devices go.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Find {
    /// An mDNS browse from each address on the home network.
    Mdns,
    /// These devices and no others, for a test's fake receivers.
    At(Vec<Heard>),
}

/// The devices that answered a search, video ones only.
pub async fn search(find: &Find, sources: &[std::net::Ipv4Addr]) -> Vec<Heard> {
    let heard = match find {
        Find::Mdns => mdns::browse(sources).await,
        Find::At(heard) => heard.clone(),
    };
    heard.into_iter().filter(Heard::shows_video).collect()
}

/// How a Cast device gets a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// As it is, of this type.
    AsIs { mime: &'static str },
    /// Converted by ffmpeg, its picture copied or encoded again.
    Convert(Picture),
}

/// The audio codecs the Default Media Receiver plays on every Cast device. AC-3 and E-AC-3 play
/// only where the TV or a receiver behind it decodes them, so they are converted.
const AUDIO: [&str; 5] = ["aac", "mp3", "opus", "vorbis", "flac"];

/// How a Cast device that decodes `decodes` gets a file with `media` in it. The Default Media
/// Receiver plays MP4 and WebM, but not Matroska, and not H.264 whose NAL units have lengths of
/// three bytes, as browsers do not.
pub fn form(media: &Media, decodes: Playback) -> Form {
    let Some(video) = &media.video else {
        // Without ffprobe only the name says what is inside.
        return match media.container.as_str() {
            "mp4" => Form::AsIs { mime: "video/mp4" },
            "webm" => Form::AsIs { mime: "video/webm" },
            _ => Form::Convert(Picture::Copy),
        };
    };
    let hevc = matches!(decodes, Playback::Hevc | Playback::HevcHdr);
    let copies = video.codec == "h264" || (hevc && video.codec == "hevc");
    let picture = if copies { Picture::Copy } else { Picture::H264 };
    let audio = media.audio_track(None).is_none_or(|audio| AUDIO.contains(&audio.codec.as_str()));
    let as_is = match media.container.as_str() {
        "mp4" if copies => Some("video/mp4"),
        "webm" if matches!(video.codec.as_str(), "vp8" | "vp9") => Some("video/webm"),
        _ => None,
    };
    match as_is {
        Some(mime) if audio && !video.odd_nal_lengths() => Form::AsIs { mime },
        _ => Form::Convert(picture),
    }
}

/// The newest codec a Cast device of `model` decodes. A Chromecast Ultra and a Google TV
/// Streamer decode HEVC. A Chromecast with Google TV does too, but it calls itself "Chromecast",
/// as the Chromecasts before it, which do not, so it gets H.264 sources.
pub fn decodes(model: &str) -> Playback {
    match model {
        "Chromecast Ultra" | "Google TV Streamer" => Playback::Hevc,
        _ => Playback::H264,
    }
}

/// What the receiver says plays: the Cast media status, as far as newsreel uses it.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Status {
    pub player: Player,
    pub time: Option<f64>,
    pub duration: Option<f64>,
    /// The URL it plays, from the last status that named it.
    pub content: Option<String>,
    pub volume: Option<u8>,
    /// The app newsreel launched is no longer running: another sender's app, or the device's
    /// own remote, closed it.
    pub closed: bool,
    /// Why the receiver gave up on the video, before or while it played.
    pub failed: Option<String>,
}

/// The receiver's `playerState`, and for `Idle` its `idleReason`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Player {
    #[default]
    Loading,
    Buffering,
    Playing,
    Paused,
    /// Stopped by the person, ended, or replaced by another sender's video.
    Idle(Idle),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Idle {
    /// No video yet, or no reason given.
    Unsaid,
    Finished,
    Cancelled,
    Interrupted,
    Error,
}

/// What to load: the URL, its type, how long it is, and where to start.
#[derive(Clone, PartialEq, Debug)]
pub struct Load {
    pub url: String,
    pub mime: String,
    pub title: String,
    pub film: bool,
    pub duration: Option<f64>,
    pub start_at: f64,
    /// An HLS playlist of fragmented MP4 segments, which the receiver must be told of.
    pub hls: bool,
}

/// A connection to a device that plays a cast in its Default Media Receiver.
pub struct Session {
    at: SocketAddr,
    /// The device's name, for errors.
    name: String,
    channel: Option<Channel>,
    /// The sender's ID on the device. Unique, so another sender's statuses are not taken for ours.
    sender: String,
    next: u64,
    app: App,
    media: Option<i64>,
    /// The LOAD's request, whose failure fails the cast.
    loading: Option<u64>,
    /// The URL of the last LOAD.
    url: String,
    status: Status,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct App {
    session: String,
    transport: String,
}

impl Session {
    /// Connects to the device at `at` and launches the Default Media Receiver on it. A device that
    /// runs it already, for another sender, gets a new session of it.
    pub async fn launch(at: SocketAddr, name: &str) -> Result<Session, Failed> {
        let mut word = [0u8; 4];
        getrandom::fill(&mut word).expect("the system gives random bytes");
        let sender = format!("sender-newsreel-{}", u32::from_be_bytes(word));
        let channel = connect(at, name, &sender).await?;
        let mut session = Session {
            at,
            name: name.to_string(),
            channel: Some(channel),
            sender,
            next: 0,
            app: App { session: String::new(), transport: String::new() },
            media: None,
            loading: None,
            url: String::new(),
            status: Status::default(),
        };
        let launch =
            session.ask(PLATFORM, RECEIVER, json!({ "type": "LAUNCH", "appId": DEFAULT_RECEIVER }), LAUNCH_WITHIN);
        let answer = launch.await?;
        let app = match answer["type"].as_str() {
            Some("RECEIVER_STATUS") => running(&answer),
            _ => None,
        };
        let Some(app) = app else {
            let reason = answer["reason"].as_str().unwrap_or("it gave no reason");
            return Err(Failed::Refused(
                0,
                Error::new(ErrorCode::FetchFailed, format!("{name} did not start its media player: {reason}")),
            ));
        };
        session.app = app;
        session.open_app()?;
        Ok(session)
    }

    fn open_app(&mut self) -> Result<(), Failed> {
        let transport = self.app.transport.clone();
        self.send(&transport, CONNECTION, json!({ "type": "CONNECT" }))
    }

    /// Asks the receiver to load and play `load`, and waits for its answer, which comes once it
    /// has read the start of the video. One that refuses the video fails; one that does not answer
    /// in time is left to say how it goes in its statuses.
    pub async fn load(&mut self, load: &Load) -> Result<(), Failed> {
        let mut media = json!({
            "contentId": load.url,
            "contentUrl": load.url,
            "contentType": load.mime,
            "streamType": "BUFFERED",
            "metadata": { "metadataType": if load.film { 1 } else { 0 }, "title": load.title },
        });
        if let Some(duration) = load.duration {
            media["duration"] = json!(duration);
        }
        if load.hls {
            media["hlsSegmentFormat"] = json!("fmp4");
            media["hlsVideoSegmentFormat"] = json!("fmp4");
        }
        let id = self.request_id();
        let transport = self.app.transport.clone();
        let mut message = json!({
            "type": "LOAD",
            "sessionId": self.app.session,
            "media": media,
            "autoplay": true,
            "currentTime": load.start_at,
        });
        self.media = None;
        self.loading = Some(id);
        self.url = load.url.clone();
        self.status = Status { volume: self.status.volume, ..Status::default() };
        message["requestId"] = json!(id);
        self.send(&transport, MEDIA, message)?;
        // The receiver answers at once that it is idle, and then that it plays or that it failed.
        let deadline = tokio::time::Instant::now() + LOAD_WITHIN;
        loop {
            if let Some(why) = self.status.failed.clone() {
                let error =
                    Error::new(ErrorCode::FetchFailed, format!("{} could not play the video ({why})", self.name));
                return Err(Failed::Refused(0, error.with_hint("play it here, or choose another source")));
            }
            if matches!(self.status.player, Player::Playing | Player::Paused | Player::Buffering)
                && self.media.is_some()
            {
                return Ok(());
            }
            let channel = self.channel.as_mut().ok_or_else(|| Failed::Gone(gone(&self.name)))?;
            match tokio::time::timeout_at(deadline, channel.recv()).await {
                Ok(Some(message)) => {
                    self.note(&message, id);
                }
                Ok(None) => {
                    self.channel = None;
                    return Err(Failed::Gone(gone(&self.name)));
                }
                Err(_) => return Ok(()),
            }
        }
    }

    /// What the receiver plays now, and with `volume` the device's volume too. A dropped
    /// connection is opened again first; one that cannot be is `Gone`.
    pub async fn poll(&mut self, volume: bool) -> Result<Status, Failed> {
        if self.channel.as_ref().is_none_or(Channel::closed) {
            self.reconnect().await?;
        }
        let mut ask = json!({ "type": "GET_STATUS" });
        if let Some(media) = self.media {
            ask["mediaSessionId"] = json!(media);
        }
        let transport = self.app.transport.clone();
        // A receiver still loading the video answers once it has, so a status it sends in the
        // meantime is taken in as it comes, and a late answer is no fault.
        match self.ask(&transport, MEDIA, ask, ANSWER_WITHIN).await {
            Ok(_) | Err(Failed::Refused(..)) => {}
            Err(gone) => return Err(gone),
        }
        if volume && !self.status.closed {
            self.ask(PLATFORM, RECEIVER, json!({ "type": "GET_STATUS" }), ANSWER_WITHIN).await?;
        }
        Ok(self.status.clone())
    }

    pub async fn pause(&mut self) -> Result<(), Failed> {
        self.control(json!({ "type": "PAUSE" })).await
    }

    pub async fn play(&mut self) -> Result<(), Failed> {
        self.control(json!({ "type": "PLAY" })).await
    }

    pub async fn seek(&mut self, position: f64) -> Result<(), Failed> {
        self.control(json!({ "type": "SEEK", "currentTime": position })).await
    }

    /// Sets the device's volume, from 0 to 100.
    pub async fn volume(&mut self, level: u8) -> Result<(), Failed> {
        let level = f64::from(level.min(100)) / 100.0;
        self.ask(PLATFORM, RECEIVER, json!({ "type": "SET_VOLUME", "volume": { "level": level } }), ANSWER_WITHIN)
            .await?;
        Ok(())
    }

    /// Stops the video and closes the Default Media Receiver, so the device goes back to where it
    /// was. A device that has gone has stopped too.
    pub async fn stop(&mut self) -> Result<(), Failed> {
        if self.media.is_some() {
            let _ = self.control(json!({ "type": "STOP" })).await;
        }
        let stop = json!({ "type": "STOP", "sessionId": self.app.session });
        self.ask(PLATFORM, RECEIVER, stop, ANSWER_WITHIN).await.map(drop)
    }

    async fn control(&mut self, mut message: Value) -> Result<(), Failed> {
        let Some(media) = self.media else {
            return Err(Failed::Refused(
                0,
                Error::new(ErrorCode::Usage, format!("{} has not started the video yet", self.name)),
            ));
        };
        message["mediaSessionId"] = json!(media);
        let transport = self.app.transport.clone();
        let answer = self.ask(&transport, MEDIA, message, ANSWER_WITHIN).await?;
        match answer["type"].as_str() {
            Some("MEDIA_STATUS") => Ok(()),
            other => {
                let why = answer["reason"].as_str().or(other).unwrap_or("no reason");
                Err(Failed::Refused(0, Error::new(ErrorCode::FetchFailed, format!("{} refused: {why}", self.name))))
            }
        }
    }

    /// Opens the connection again, and the app's virtual connection, after a drop. The receiver
    /// keeps playing meanwhile, and its status says whether newsreel's app still runs.
    async fn reconnect(&mut self) -> Result<(), Failed> {
        self.channel = None;
        self.channel = Some(connect(self.at, &self.name, &self.sender).await?);
        self.open_app()?;
        self.ask(PLATFORM, RECEIVER, json!({ "type": "GET_STATUS" }), ANSWER_WITHIN).await?;
        Ok(())
    }

    fn request_id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn send(&mut self, destination: &str, namespace: &str, payload: Value) -> Result<(), Failed> {
        let message = Message {
            source: self.sender.clone(),
            destination: destination.to_string(),
            namespace: namespace.to_string(),
            payload: payload.to_string(),
        };
        let gone = || Failed::Gone(gone(&self.name));
        self.channel.as_ref().ok_or_else(gone)?.send(message).map_err(|_| gone())
    }

    /// Sends a request and returns its answer, taking in every other message meanwhile.
    async fn ask(
        &mut self,
        destination: &str,
        namespace: &str,
        payload: Value,
        within: Duration,
    ) -> Result<Value, Failed> {
        let id = self.request_id();
        self.exchange(id, destination, namespace, payload, within).await?.ok_or_else(|| {
            let late = Error::new(ErrorCode::NetworkUnavailable, format!("{} did not answer in time", self.name));
            Failed::Refused(0, late)
        })
    }

    /// Sends request `id` and returns its answer, or `None` when none came `within`.
    async fn exchange(
        &mut self,
        id: u64,
        destination: &str,
        namespace: &str,
        mut payload: Value,
        within: Duration,
    ) -> Result<Option<Value>, Failed> {
        payload["requestId"] = json!(id);
        self.send(destination, namespace, payload)?;
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let channel = self.channel.as_mut().ok_or_else(|| Failed::Gone(gone(&self.name)))?;
            let message = match tokio::time::timeout_at(deadline, channel.recv()).await {
                Ok(Some(message)) => message,
                Ok(None) => {
                    self.channel = None;
                    return Err(Failed::Gone(gone(&self.name)));
                }
                Err(_) => return Ok(None),
            };
            if let Some(answer) = self.note(&message, id) {
                return Ok(Some(answer));
            }
        }
    }

    /// Takes in a message from the device, and returns it when it answers request `id`.
    fn note(&mut self, message: &Message, id: u64) -> Option<Value> {
        if message.destination != self.sender && message.destination != "*" {
            return None;
        }
        let payload: Value = serde_json::from_str(&message.payload).ok()?;
        let kind = payload["type"].as_str().unwrap_or_default();
        match (message.namespace.as_str(), kind) {
            (CONNECTION, "CLOSE") if message.source == self.app.transport => self.status.closed = true,
            (RECEIVER, "RECEIVER_STATUS") => {
                let status = &payload["status"];
                if let Some(level) = status["volume"]["level"].as_f64() {
                    self.status.volume = Some((level * 100.0).round().clamp(0.0, 100.0) as u8);
                }
                if !self.app.session.is_empty() && running(&payload).is_none_or(|app| app != self.app) {
                    self.status.closed = true;
                }
            }
            (MEDIA, "MEDIA_STATUS") => self.media_status(&payload),
            (MEDIA, "LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST" | "ERROR")
                if payload["requestId"].as_u64() == self.loading =>
            {
                let detail = payload["detailedErrorCode"].as_i64().map(|code| format!(" ({code})")).unwrap_or_default();
                self.status.failed = Some(format!("{}{detail}", kind.to_ascii_lowercase().replace('_', " ")));
            }
            _ => {}
        }
        (payload["requestId"].as_u64() == Some(id)).then_some(payload)
    }

    fn media_status(&mut self, payload: &Value) {
        let statuses = payload["status"].as_array().map(Vec::as_slice).unwrap_or_default();
        // The status of the video newsreel loaded: its session once known, else the one that
        // names its URL.
        let ours = statuses.iter().find(|status| match self.media {
            Some(media) => status["mediaSessionId"].as_i64() == Some(media),
            None => {
                let content = status["media"]["contentId"].as_str();
                payload["requestId"].as_u64() == self.loading || content == Some(self.url.as_str())
            }
        });
        let Some(status) = ours else {
            // No status for it: the receiver plays nothing, or another sender's video.
            if self.media.is_some() && statuses.iter().all(|s| s["mediaSessionId"].as_i64() != self.media) {
                self.status.player = Player::Idle(Idle::Interrupted);
            }
            return;
        };
        self.media = status["mediaSessionId"].as_i64().or(self.media);
        if let Some(content) = status["media"]["contentId"].as_str() {
            self.status.content = Some(content.to_string());
        }
        if let Some(duration) = status["media"]["duration"].as_f64().filter(|d| *d > 0.0) {
            self.status.duration = Some(duration);
        }
        if let Some(time) = status["currentTime"].as_f64() {
            self.status.time = Some(time);
        }
        self.status.player = match status["playerState"].as_str().unwrap_or_default() {
            "PLAYING" => Player::Playing,
            "PAUSED" => Player::Paused,
            "BUFFERING" => Player::Buffering,
            "LOADING" => Player::Loading,
            "IDLE" => Player::Idle(match status["idleReason"].as_str() {
                Some("FINISHED") => Idle::Finished,
                Some("CANCELLED") => Idle::Cancelled,
                Some("INTERRUPTED") => Idle::Interrupted,
                Some("ERROR") => Idle::Error,
                _ => Idle::Unsaid,
            }),
            _ => self.status.player,
        };
        if self.status.player == Player::Idle(Idle::Error) && self.status.failed.is_none() {
            self.status.failed = Some("error".into());
        }
    }
}

/// The Default Media Receiver in a RECEIVER_STATUS, if it runs.
fn running(payload: &Value) -> Option<App> {
    payload["status"]["applications"]
        .as_array()?
        .iter()
        .find_map(|app| {
            (app["appId"].as_str() == Some(DEFAULT_RECEIVER)).then(|| App {
                session: app["sessionId"].as_str().unwrap_or_default().to_string(),
                transport: app["transportId"].as_str().unwrap_or_default().to_string(),
            })
        })
        .filter(|app| !app.session.is_empty() && !app.transport.is_empty())
}

async fn connect(at: SocketAddr, name: &str, sender: &str) -> Result<Channel, Failed> {
    let channel = Channel::connect(at).await.map_err(|e| {
        Failed::Gone(
            Error::new(ErrorCode::NetworkUnavailable, format!("{name} cannot be reached at {at} ({e})"))
                .with_hint("check that the TV is on and on the same network as the computer running newsreel"),
        )
    })?;
    let message = Message {
        source: sender.to_string(),
        destination: PLATFORM.to_string(),
        namespace: CONNECTION.to_string(),
        payload: json!({ "type": "CONNECT", "origin": {} }).to_string(),
    };
    channel.send(message).map_err(|_| Failed::Gone(gone(name)))?;
    Ok(channel)
}

fn gone(name: &str) -> Error {
    Error::new(ErrorCode::NetworkUnavailable, format!("{name} closed the connection"))
}

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
