//! Play on TV. The engine finds the TVs on the home network with SSDP and mDNS, and plays a film,
//! an episode, or a release's video on one over UPnP AV or Google Cast: it mints a cast grant for
//! the file, tells the TV its URL, and asks the TV where it is every second while it plays, which
//! goes to the watch history. The grant is dropped once the cast ends, and the TV can fetch
//! nothing more.

pub mod didl;
pub mod google;
mod ssdp;
mod upnp;
mod xml;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use newsreel_core::filetype::extension;
use newsreel_core::text::sanitize;
use newsreel_core::time::now;
use newsreel_core::title::Span;
use newsreel_core::{
    AttemptEvent, Cast, CastAction, CastRequest, CastState, Container, DownloadId, Error, ErrorCode, Player, Plays,
    Protocol, Quality, Resolution, ResultId, Target, TitleLink, Tv, TvList, VideoRef, Viewer, Watched,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::ctx::Ctx;
use super::downloads;
use super::media;
use super::media::ffmpeg::Picture;
use super::media::hls::{self, Converted};
use super::names::Names;
use super::owner::Command;
use super::owner::plays::Destination;
use super::playing::Open;
use super::titles;
use super::titles::attempt::{self, Outcome, PlayTitle, Played};
use super::titles::watching::{Screen, Watching};
use crate::log::log;
use newsreel_core::title::title_name;
use newsreel_core::watching::{SAVE_EVERY, WORTH_SAVING};
pub use ssdp::Find;
use ssdp::Heard;
use upnp::{Failed, Renderer, Service};

/// A search this recent answers a request for the TVs without searching again.
const FRESH: Duration = Duration::from_secs(30);
/// A TV that misses this many searches in a row leaves the list.
const MISSES: u8 = 2;
/// How long a device description may take to arrive.
const DESCRIBE_WITHIN: Duration = Duration::from_secs(4);
/// How many devices a search describes at once.
const DESCRIBING: usize = 4;
const POLL: Duration = Duration::from_secs(1);
/// How often a cast asks the TV its volume, in polls.
const VOLUME_EVERY: u32 = 5;
/// How long a TV may take to start playing once it has the URL. The first bytes are in by then.
const START_WITHIN: Duration = Duration::from_secs(90);
/// A TV that answers nothing for this long has gone.
const LOST_AFTER: Duration = Duration::from_secs(15);
/// A TV that answers but does not say what it does for this long has stopped playing the cast.
const FAILING_FOR: Duration = Duration::from_secs(60);
/// How long an ended cast stays listed, so whoever started it can see how it ended.
const ENDED_KEPT: Duration = Duration::from_secs(60);
/// How long the engine waits to be reachable on the home network before it gives up on a cast.
const REACHABLE_WITHIN: Duration = Duration::from_secs(5);
/// Where the engine answers with its word, for anyone who asks.
pub const HELLO: &str = "/.well-known/newsreel";
/// Tries at the seek that resumes a video, since some TVs refuse one while they start, and some
/// answer one and lose it.
const RESUME_TRIES: u32 = 3;
/// How long a TV that took the seek that resumes a video may take to get there before it is sent
/// again. Until then the cast shows where it resumes. An LG TV takes about 3 seconds.
const RESUME_WITHIN: Duration = Duration::from_secs(5);
/// How far before where a video resumes a TV may land: a seek goes to the keyframe before it.
const RESUME_SLACK: f64 = 10.0;

/// The TVs on the home network, and a cast for each TV that plays one.
pub struct Casts {
    ctx: Weak<Ctx>,
    find: Find,
    google: google::Find,
    /// Where the segments of the HLS films casts convert are kept, a folder for each cast.
    segments: PathBuf,
    http: reqwest::Client,
    /// A random word the engine answers at `HELLO`, so a cast can tell this engine from another
    /// program on the same port.
    hello: String,
    state: Mutex<State>,
    /// Held while a search runs, so requests that arrive meanwhile take its answer.
    searching: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    /// By ID, so a device that copies a TV's UDN from another address is another TV.
    tvs: BTreeMap<String, Known>,
    searched: Option<Instant>,
    /// By the TV's ID.
    casts: BTreeMap<String, Entry>,
    /// How each file a cast grant serves goes to the TV, by the grant's path.
    served: HashMap<String, Served>,
    /// The largest video each Cast device's player took, by the TV's ID, once it refused one.
    fits: HashMap<String, (u32, u32)>,
    next: u64,
}

struct Known {
    device: Device,
    /// Where its description is, or for a Cast device its address.
    location: String,
    missed: u8,
}

/// A TV on the home network, and how to reach it.
#[derive(Clone, Debug)]
enum Device {
    Dlna(Arc<Renderer>),
    Cast(Arc<Receiver>),
}

impl Device {
    fn tv(&self) -> &Tv {
        match self {
            Device::Dlna(renderer) => &renderer.tv,
            Device::Cast(receiver) => &receiver.tv,
        }
    }
}

/// A Cast device that shows video.
#[derive(Debug)]
struct Receiver {
    tv: Tv,
    at: SocketAddr,
}

impl Receiver {
    fn new(heard: &google::Heard) -> Self {
        let address = heard.at.ip().to_string();
        let tv = Tv {
            id: format!("cast:{}@{address}", heard.id),
            name: heard.name.clone(),
            protocol: Protocol::GoogleCast,
            address,
            plays: [Container::Mp4, Container::WebM].into_iter().collect(),
            decodes: google::decodes(&heard.model),
            volume: true,
        };
        Self { tv, at: heard.at }
    }
}

impl State {
    /// Takes in the renderers one search found. A TV that misses `MISSES` searches in a row
    /// leaves the list, unless a cast plays on it, and one that missed the last search leaves to
    /// keep the list to `ssdp::MOST`.
    fn found(&mut self, seen: Vec<Known>) {
        let seen: BTreeMap<String, Known> = seen.into_iter().map(|k| (k.device.tv().id.clone(), k)).collect();
        let State { tvs, casts, .. } = self;
        for (id, known) in tvs.iter_mut() {
            if !seen.contains_key(id) {
                known.missed = known.missed.saturating_add(1);
            }
        }
        tvs.retain(|id, known| known.missed < MISSES || casts.contains_key(id));
        tvs.extend(seen);
        let mut missing: Vec<String> = tvs
            .iter()
            .filter(|(id, known)| known.missed > 0 && !casts.contains_key(*id))
            .map(|(id, _)| id.clone())
            .collect();
        while tvs.len() > ssdp::MOST
            && let Some(id) = missing.pop()
        {
            tvs.remove(&id);
        }
        self.searched = Some(Instant::now());
    }

    /// The TVs by name.
    fn tvs(&self) -> Vec<Tv> {
        let mut tvs: Vec<Tv> = self.tvs.values().map(|known| known.device.tv().clone()).collect();
        tvs.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        tvs
    }
}

/// A cast, and the way to its task.
struct Entry {
    id: u64,
    view: watch::Receiver<Cast>,
    control: mpsc::UnboundedSender<Control>,
}

enum Control {
    Act(CastAction, oneshot::Sender<Result<Cast, Error>>),
    /// Another cast takes the TV: keep the position, and end without stopping the TV.
    Leave,
}

/// How what a cast grant serves goes to the TV.
pub enum Served {
    /// A file as it is, to a DLNA renderer.
    Dlna(Dlna),
    /// A file as it is, to a Cast device, whose player runs on another origin.
    Cast { mime: String },
    /// An HLS film made as the Cast device asks for it.
    Film(Arc<hls::Film>),
    /// A fragmented MP4 from the time a request asks for.
    Stream(Arc<Stream>),
}

impl Served {
    /// The type and extra headers of a file sent as it is.
    pub fn file(&self) -> Option<(String, String)> {
        match self {
            Served::Dlna(dlna) => Some((dlna.mime.clone(), dlna.headers())),
            Served::Cast { mime } => Some((mime.clone(), CORS.to_string())),
            Served::Film(_) | Served::Stream(_) => None,
        }
    }
}

/// Lets a Cast device's player, a page on Google's origin, read what a cast grant serves. The
/// grant's URL is all it holds.
pub const CORS: &str = "Access-Control-Allow-Origin: *\r\n";

/// A conversion that starts at the time a request asks for.
pub struct Stream {
    pub program: PathBuf,
    pub arguments: hls::Arguments,
}

/// How a file a cast grant serves goes to the TV: its type in the TV's spelling, and the DLNA
/// headers TVs look for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dlna {
    pub mime: String,
    /// The subtitle's URL, which Samsung TVs read from a header of the video's answer.
    caption: Option<String>,
    video: bool,
}

impl Dlna {
    pub fn headers(&self) -> String {
        let mut headers = if self.video {
            format!("transferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {}\r\n", didl::FEATURES)
        } else {
            "transferMode.dlna.org: Interactive\r\n".to_string()
        };
        if let Some(caption) = &self.caption {
            headers.push_str(&format!("CaptionInfo.sec: {caption}\r\n"));
        }
        headers
    }
}

impl Casts {
    pub fn new(ctx: Weak<Ctx>, find: Find, google: google::Find, segments: PathBuf) -> Self {
        // A redirect could lead anywhere, and a TV has no use for one. Each request gets a
        // connection of its own: libupnp, which many renderers run, answers one request per
        // connection without saying so, and drops the next one sent on it.
        let http = reqwest::Client::builder()
            .no_proxy()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("a plain HTTP client builds");
        let mut word = [0u8; 16];
        getrandom::fill(&mut word).expect("the system gives random bytes");
        let hello = word.iter().map(|b| format!("{b:02x}")).collect();
        // What a cast left behind when the engine stopped.
        let _ = std::fs::remove_dir_all(&segments);
        Self {
            ctx,
            find,
            google,
            segments,
            http,
            hello,
            state: Mutex::default(),
            searching: tokio::sync::Mutex::new(()),
        }
    }

    pub fn hello(&self) -> &str {
        &self.hello
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The TVs and the casts as known now.
    pub fn list(&self) -> TvList {
        let state = self.state();
        let mut casts: Vec<Cast> = state.casts.values().map(|entry| entry.view.borrow().clone()).collect();
        casts.sort_by_key(|cast| cast.started_at);
        TvList { tvs: state.tvs(), casts }
    }

    /// The TVs and the casts, after a search of the home network unless one ran in the last 30
    /// seconds and `fresh` is false.
    pub async fn search(&self, fresh: bool) -> TvList {
        let before = self.state().searched;
        let _searching = self.searching.lock().await;
        let searched = self.state().searched;
        let recent = searched.is_some_and(|at| at.elapsed() < FRESH);
        if searched != before || (recent && !fresh) {
            return self.list();
        }
        let lan = Names::read().lan;
        let (answers, receivers) = tokio::join!(ssdp::search(self.find, &lan), google::search(&self.google, &lan));
        let answers = answers.unwrap_or_default();
        let known: HashMap<String, Arc<Renderer>> = {
            let state = self.state();
            let renderers = state.tvs.values().filter_map(|k| match &k.device {
                Device::Dlna(renderer) => Some((k.location.clone(), renderer.clone())),
                Device::Cast(_) => None,
            });
            renderers.collect()
        };
        let mut found = tokio::task::JoinSet::new();
        let describing = Arc::new(tokio::sync::Semaphore::new(DESCRIBING));
        for heard in answers {
            let (http, known) = (self.http.clone(), known.get(heard.location.as_str()).cloned());
            let describing = describing.clone();
            found.spawn(async move {
                let _turn = describing.acquire_owned().await;
                let renderer = match known {
                    Some(renderer) => Some(renderer),
                    None => renderer(&http, &heard).await.map(Arc::new),
                };
                renderer.map(|renderer| (heard.location.to_string(), renderer))
            });
        }
        let mut seen: Vec<Known> = receivers
            .iter()
            .map(|heard| Known {
                device: Device::Cast(Arc::new(Receiver::new(heard))),
                location: heard.at.to_string(),
                missed: 0,
            })
            .collect();
        while let Some(answer) = found.join_next().await {
            if let Ok(Some((location, renderer))) = answer {
                seen.push(Known { device: Device::Dlna(renderer), location, missed: 0 });
            }
        }
        self.state().found(seen);
        self.list()
    }

    /// Starts a cast in place of any on the same TV, unless the engine is stopping for an update.
    /// Returns the cast as it changes, and the steps of finding a title's source.
    pub async fn start(
        &self,
        request: CastRequest,
    ) -> Result<(watch::Receiver<Cast>, broadcast::Receiver<AttemptEvent>), Error> {
        let ctx = self.ctx.upgrade().ok_or_else(|| Error::internal("the engine is stopping"))?;
        if request.start_at.is_some_and(|at| !at.is_finite() || at < 0.0) {
            return Err(Error::new(ErrorCode::Usage, "start_at is not a time in a video"));
        }
        let playing = ctx.playing.open()?;
        let tv = match self.choose(request.tv.as_deref()) {
            Err(error) if error.code == ErrorCode::NotFound => {
                self.search(true).await;
                self.choose(request.tv.as_deref())?
            }
            chosen => chosen?,
        };
        // A title's name comes once its lookup answers, and a release's once its file is chosen.
        let (name, state) = match &request.target {
            Target::Title(video) => (video.to_string(), CastState::Finding),
            _ => ("the video".to_string(), CastState::Loading),
        };
        let cast = Cast {
            tv: tv.tv().id.clone(),
            tv_name: tv.tv().name.clone(),
            name,
            video: None,
            download: None,
            file: None,
            subtitle: None,
            state,
            position: None,
            duration: None,
            volume: None,
            started_at: now(),
        };
        let (view_tx, view) = watch::channel(cast);
        let (steps_tx, steps) = broadcast::channel(64);
        let (control_tx, control) = mpsc::unbounded_channel();
        let (id, before) = {
            let mut state = self.state();
            state.next += 1;
            let id = state.next;
            let entry = Entry { id, view: view.clone(), control: control_tx };
            let before = state.casts.insert(tv.tv().id.clone(), entry).map(|old| {
                let _ = old.control.send(Control::Leave);
                old.view
            });
            (id, before)
        };
        let run = Run { ctx, tv, view: view_tx, id, before };
        tokio::spawn(run.cast(request, control, steps_tx, playing));
        Ok((view, steps))
    }

    /// Asks the cast on the TV `tv` names, or the only cast playing, to pause, resume, seek, stop,
    /// or set the volume, and returns it after.
    pub async fn control(&self, tv: Option<&str>, action: CastAction) -> Result<Cast, Error> {
        let control = {
            let state = self.state();
            let live: Vec<&Entry> = state.casts.values().filter(|entry| !entry.view.borrow().ended()).collect();
            let chosen = match tv {
                None => match live.as_slice() {
                    [one] => *one,
                    [] => return Err(nothing_plays()),
                    many => {
                        let names: Vec<String> = many
                            .iter()
                            .map(|e| {
                                let cast = e.view.borrow();
                                at_address(&cast.tv_name, &cast.tv)
                            })
                            .collect();
                        return Err(Error::new(
                            ErrorCode::AmbiguousRef,
                            format!("casts play on {} TVs: {}", names.len(), names.join(", ")),
                        )
                        .with_hint("name the TV with --tv"));
                    }
                },
                Some(wanted) => {
                    let matching: Vec<&Entry> = live
                        .iter()
                        .copied()
                        .filter(|e| {
                            let cast = e.view.borrow();
                            names_tv(wanted, &cast.tv, &cast.tv_name)
                        })
                        .collect();
                    match matching.as_slice() {
                        [one] => *one,
                        [] => return Err(nothing_plays()),
                        _ => {
                            return Err(Error::new(
                                ErrorCode::AmbiguousRef,
                                format!("{wanted} names more than one TV"),
                            ));
                        }
                    }
                }
            };
            chosen.control.clone()
        };
        let (reply, answer) = oneshot::channel();
        control.send(Control::Act(action, reply)).map_err(|_| nothing_plays())?;
        answer.await.map_err(|_| nothing_plays())?
    }

    /// The largest video the Cast device `tv` names takes, once it refused a larger one.
    fn fits(&self, tv: &str) -> Option<(u32, u32)> {
        self.state().fits.get(tv).copied()
    }

    /// How what a cast grant serves at `path` goes to the TV: the file at that path, or what is
    /// asked of the grant's folder.
    pub fn served(&self, path: &str) -> Option<Served> {
        let state = self.state();
        let folder = path.rsplit_once('/').map(|(folder, _)| format!("{folder}/"));
        let served = state.served.get(path).or_else(|| state.served.get(folder.as_deref()?))?;
        Some(match served {
            Served::Dlna(dlna) => Served::Dlna(dlna.clone()),
            Served::Cast { mime } => Served::Cast { mime: mime.clone() },
            Served::Film(film) => Served::Film(film.clone()),
            Served::Stream(stream) => Served::Stream(stream.clone()),
        })
    }

    /// The TV `wanted` names by its ID or a part of its name, or the only TV on the network.
    fn choose(&self, wanted: Option<&str>) -> Result<Device, Error> {
        let state = self.state();
        let tvs: Vec<&Device> = match wanted {
            Some(wanted) => {
                state.tvs.values().map(|k| &k.device).filter(|d| names_tv(wanted, &d.tv().id, &d.tv().name)).collect()
            }
            None => state.tvs.values().map(|k| &k.device).collect(),
        };
        match (tvs.as_slice(), wanted) {
            ([one], _) => Ok((*one).clone()),
            ([], None) => Err(Error::new(ErrorCode::NotFound, "no TV answered on the home network").with_hint(
                "check that the TV is on and on the same network as the computer running newsreel, and for a TV that does not have Google Cast, that its DLNA or media sharing setting is on",
            )),
            ([], Some(wanted)) => Err(Error::new(ErrorCode::NotFound, format!("no TV named {wanted} answered"))
                .with_hint("run newsreel tv to list the TVs on the network")
                .with_next("newsreel tv")),
            (many, _) => {
                let names: Vec<String> = many.iter().map(|d| at_address(&d.tv().name, &d.tv().id)).collect();
                Err(Error::new(ErrorCode::AmbiguousRef, format!("{} TVs answered: {}", names.len(), names.join(", ")))
                    .with_hint("name one with --tv, by its address if they share a name"))
            }
        }
    }

    /// Takes an ended cast off the list, unless another has taken its TV since.
    fn forget(&self, tv: &str, id: u64) {
        let mut state = self.state();
        if state.casts.get(tv).is_some_and(|entry| entry.id == id) {
            state.casts.remove(tv);
        }
    }
}

/// Waits until the TV plays the cast, and returns it then. A cast that fails first fails with why.
pub async fn playing(view: &mut watch::Receiver<Cast>) -> Result<Cast, Error> {
    let started = |cast: &Cast| matches!(cast.state, CastState::Playing | CastState::Paused | CastState::Ended { .. });
    let cast = view.wait_for(started).await.map_err(|_| Error::internal("the cast stopped without ending"))?.clone();
    match cast.state {
        CastState::Ended { error: Some(error) } => Err(error),
        _ => Ok(cast),
    }
}

fn nothing_plays() -> Error {
    Error::new(ErrorCode::NotFound, "no cast is playing").with_hint("start one with newsreel tv play")
}

/// Whether `wanted` is the TV's ID or address, or a part of its name in any case.
fn names_tv(wanted: &str, id: &str, name: &str) -> bool {
    let wanted = wanted.trim();
    !wanted.is_empty()
        && (id == wanted || address(id) == wanted || name.to_lowercase().contains(&wanted.to_lowercase()))
}

/// The address in a TV's ID, which is its UDN and its address.
fn address(id: &str) -> &str {
    id.rsplit_once('@').map_or("", |(_, address)| address)
}

/// A TV's name with its address, which tells apart two TVs of the same name.
fn at_address(name: &str, id: &str) -> String {
    format!("{name} at {}", address(id))
}

/// The renderer that answered a search, with what its ConnectionManager says it plays. `None` for
/// a device that is not one, or does not answer.
async fn renderer(http: &reqwest::Client, heard: &Heard) -> Option<Renderer> {
    let answer = http.get(heard.location.clone()).timeout(DESCRIBE_WITHIN).send().await.ok()?;
    if !answer.status().is_success() {
        return None;
    }
    let description = upnp::describe(&upnp::read(answer).await.ok()??, &heard.location, heard.from)?;
    let sink = match &description.connection {
        Some(connection) => upnp::call(http, connection, "GetProtocolInfo", &[], &description.name)
            .await
            .ok()
            .and_then(|answer| xml::text(&answer, "Sink"))
            .map(|sink| upnp::sink(&sink))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    Some(Renderer::new(description, heard.from.to_string(), sink))
}

struct Found {
    id: DownloadId,
    file: u32,
    /// Relative to the release's folder, sanitized.
    path: String,
    size: u64,
    name: String,
    video: Option<VideoRef>,
    release: Option<ResultId>,
    /// Seconds.
    start_at: Option<f64>,
    subtitle: Option<(u32, String)>,
}

struct Run {
    ctx: Arc<Ctx>,
    tv: Device,
    view: watch::Sender<Cast>,
    id: u64,
    /// The cast this one took the TV from, which may still be sending it a video.
    before: Option<watch::Receiver<Cast>>,
}

impl Run {
    /// Finds the video and plays it on the TV. The cast counts as playing from its start to its
    /// end, through finding a source and any pause on the TV, so an update waits for it.
    async fn cast(
        self,
        request: CastRequest,
        mut control: mpsc::UnboundedReceiver<Control>,
        steps: broadcast::Sender<AttemptEvent>,
        playing: Open,
    ) {
        let found = tokio::select! {
            found = find(&self.ctx, &self.tv, &request, &steps, &self.view) => Some(found),
            () = until_stopped(&mut control) => None,
        };
        drop(steps);
        let error = match found {
            None => None,
            Some(Err(error)) => Some(error),
            Some(Ok(found)) => {
                self.view.send_modify(|cast| {
                    cast.name = found.name.clone();
                    cast.video = found.video.clone();
                    cast.download = Some(found.id.to_hex());
                    cast.file = Some(found.file);
                    cast.subtitle = found.subtitle.as_ref().map(|(_, path)| path.clone());
                    cast.state = CastState::Loading;
                });
                self.play(&found, &mut control).await.err()
            }
        };
        if let Some(error) = &error {
            log(&format!("the cast to {} ended: {error}", self.tv.tv().name));
        }
        // A cast that takes the TV from this one waits for this end, so it must mean the cast
        // before has ended too.
        if let Some(before) = self.before.clone() {
            ended(before).await;
        }
        drop(playing);
        self.view.send_modify(|cast| cast.state = CastState::Ended { error });
        drop(control);
        tokio::time::sleep(ENDED_KEPT).await;
        self.ctx.casts.forget(&self.tv.tv().id, self.id);
    }

    /// Sends the file to the TV and follows it until the cast ends.
    async fn play(&self, found: &Found, control: &mut mpsc::UnboundedReceiver<Control>) -> Result<(), Error> {
        let ctx = &self.ctx;
        // A TV asks for the file a range at a time and holds nothing open while it pauses, so the
        // cast streams the file until it ends, which keeps its download in the plays folder.
        let (id, index) = (found.id, found.file);
        let _streaming = ctx.owner.ask(|reply| Command::Stream { id, index, reply }).await?;
        if let Some(before) = self.before.clone() {
            tokio::select! {
                () = ended(before) => {}
                () = until_stopped(control) => return Ok(()),
            }
        }
        let host = facing(&self.tv.tv().address)?;
        let _keeping = found.video.as_ref().map(|video| Keeping::new(&ctx.titles.watching, video, self.id));
        match &self.tv {
            Device::Dlna(renderer) => self.play_dlna(renderer, found, host, control).await,
            Device::Cast(receiver) => self.play_cast(receiver, found, host, control).await,
        }
    }

    async fn play_dlna(
        &self,
        renderer: &Renderer,
        found: &Found,
        host: IpAddr,
        control: &mut mpsc::UnboundedReceiver<Control>,
    ) -> Result<(), Error> {
        let ctx = &self.ctx;
        let tv = &renderer.tv;
        let video = ctx.devices.grant(found.id, found.file, &found.path)?;
        let caption =
            found.subtitle.as_ref().map(|(index, path)| ctx.devices.grant(found.id, *index, path)).transpose()?;
        reachable(ctx, host).await?;
        let at = |path: &str| format!("http://{}{path}", std::net::SocketAddr::new(host, ctx.http_port));
        let url = at(video.path());
        let caption_url = caption.as_ref().map(|grant| at(grant.path()));
        let container = Container::of_path(&found.path).ok_or_else(|| refused(tv, &found.path))?;
        let mime = renderer.mime(container).to_string();
        let mut served = vec![(
            video.path().to_string(),
            Served::Dlna(Dlna { mime: mime.clone(), caption: caption_url.clone(), video: true }),
        )];
        if let (Some(grant), Some((_, path))) = (&caption, &found.subtitle) {
            let kind = extension(path);
            served.push((
                grant.path().to_string(),
                Served::Dlna(Dlna { mime: format!("text/{kind}"), caption: None, video: false }),
            ));
        }
        let _serving = Serving::new(&ctx.casts, served);
        let duration = media::probed(ctx, found.id, found.file).and_then(|media| media.duration);
        let film = found.video.as_ref().is_some_and(|video| video.episode.is_none());
        let kind = found.subtitle.as_ref().map(|(_, path)| extension(path));
        let metadata = didl::didl(&didl::Item {
            title: &found.name,
            url: &url,
            mime: &mime,
            size: found.size,
            duration,
            film,
            caption: caption_url.as_deref().zip(kind.as_deref()).map(|(url, kind)| didl::Caption { url, kind }),
        });
        let mut remote = Remote::Dlna(renderer);
        self.send(renderer, &url, &metadata).await?;
        self.follow(found, video.token(), duration, control, &mut remote).await
    }

    /// Plays on a Cast device: the file as it is when the device plays it, or else an HLS film
    /// converted as the device asks for it. A device whose player refuses a film as large as the
    /// video gets one encoded again at 720p, as do its casts after.
    async fn play_cast(
        &self,
        receiver: &Receiver,
        found: &Found,
        host: IpAddr,
        control: &mut mpsc::UnboundedReceiver<Control>,
    ) -> Result<(), Error> {
        let ctx = &self.ctx;
        let tv = &receiver.tv;
        let media = media::probe(ctx, found.id, found.file).await?;
        let start_at = found.start_at.filter(|at| *at > WORTH_SAVING).unwrap_or(0.0);
        let larger =
            |(width, height): (u32, u32)| media.video.as_ref().is_some_and(|v| v.width > width || v.height > height);
        let mut form = match google::form(&media, tv.decodes) {
            google::Form::Convert(Picture::Copy) if ctx.casts.fits(&tv.id).is_some_and(larger) => {
                let (width, height) = FIT;
                google::Form::Convert(Picture::Fit(width, height))
            }
            form => form,
        };
        let launch = google::Session::launch(receiver.at, &tv.name);
        let (session, sending) = tokio::join!(launch, self.sending(found, &media, form, host, start_at));
        let (mut session, mut sending) = (session?, sending?);
        loop {
            reachable(ctx, host).await?;
            log(&format!("casting {} to {} as {}", found.name, tv.name, sending.sent.name()));
            match session.load(&sending.load).await {
                Ok(()) => break,
                Err(Failed::Refused(..)) if form == google::Form::Convert(Picture::Copy) && larger(FIT) => {
                    log(&format!("{} refused the film at its own size, so it gets one at 720p", tv.name));
                    ctx.casts.state().fits.insert(tv.id.clone(), FIT);
                    form = google::Form::Convert(Picture::Fit(FIT.0, FIT.1));
                    drop(sending);
                    sending = self.sending(found, &media, form, host, start_at).await?;
                }
                Err(failed) => return Err(failed.into()),
            }
        }
        let Sending { grant, serving, load, sent, duration } = sending;
        let mut remote = Remote::Cast(Box::new(CastRemote::new(session, load, sent)));
        let ended = self.follow(found, grant.token(), duration, control, &mut remote).await;
        if let Remote::Cast(remote) = &mut remote
            && remote.finished
        {
            // At the video's end the device goes back to where it was.
            let _ = remote.session.stop().await;
        }
        drop(serving);
        ended
    }

    /// The grant, and what it serves, for sending `found` to a Cast device in `form`.
    async fn sending(
        &self,
        found: &Found,
        media: &media::Media,
        form: google::Form,
        host: IpAddr,
        start_at: f64,
    ) -> Result<Sending<'_>, Error> {
        let ctx = &self.ctx;
        let at = |path: &str| format!("http://{}{path}", std::net::SocketAddr::new(host, ctx.http_port));
        let duration = media.duration;
        let (grant, served, load, sent, duration) = match form {
            google::Form::AsIs { mime } => {
                let grant = ctx.devices.grant(found.id, found.file, &found.path)?;
                let load = self.load(at(grant.path()), mime.to_string(), duration, start_at, false, found);
                (grant, Served::Cast { mime: mime.to_string() }, load, Sent::AsIs, duration)
            }
            google::Form::Convert(picture) => {
                let grant = ctx.devices.grant_folder(found.id, found.file)?;
                let dir = ctx.casts.segments.join(grant.token());
                match hls::convert(ctx, found.id, found.file, picture, &dir, start_at).await? {
                    Converted::Film(film) => {
                        let url = at(&format!("{}{FILM}", grant.path()));
                        let duration = Some(film.plan().duration);
                        let load = self.load(url, hls::PLAYLIST.to_string(), duration, start_at, true, found);
                        (grant, Served::Film(Arc::new(film)), load, Sent::Film, duration)
                    }
                    Converted::Stream { program, arguments } => {
                        let base = at(&format!("{}{STREAM}", grant.path()));
                        let url = stream_url(&base, start_at);
                        let load = self.load(url, "video/mp4".to_string(), duration, start_at, false, found);
                        let stream = Arc::new(Stream { program, arguments });
                        (grant, Served::Stream(stream), load, Sent::Stream { base }, duration)
                    }
                }
            }
        };
        let serving = Serving::new(&ctx.casts, vec![(grant.path().to_string(), served)]);
        Ok(Sending { grant, serving, load, sent, duration })
    }

    fn load(
        &self,
        url: String,
        mime: String,
        duration: Option<f64>,
        start_at: f64,
        hls: bool,
        found: &Found,
    ) -> google::Load {
        let film = found.video.as_ref().is_some_and(|video| video.episode.is_none());
        google::Load { url, mime, title: found.name.clone(), film, duration, start_at, hls }
    }

    /// Tells the TV to play `url`. A TV that refuses a new video while it plays one gets a Stop
    /// first. One that does not answer may still hold what it played before, which a Play would
    /// bring back, so the cast fails there.
    async fn send(&self, tv: &Renderer, url: &str, metadata: &str) -> Result<(), Error> {
        let set = [("InstanceID", "0"), ("CurrentURI", url), ("CurrentURIMetaData", metadata)];
        match self.transport(tv, "SetAVTransportURI", &set).await {
            Ok(_) => {}
            Err(Failed::Refused(..)) => {
                let _ = self.transport(tv, "Stop", &[("InstanceID", "0")]).await;
                self.transport(tv, "SetAVTransportURI", &set).await?;
            }
            Err(gone @ Failed::Gone(_)) => return Err(gone.into()),
        }
        self.transport(tv, "Play", &[("InstanceID", "0"), ("Speed", "1")]).await?;
        Ok(())
    }

    /// Asks the TV where it is every second, keeps the position in the watch history, and carries
    /// out what the remote control asks, until the cast ends. A TV whose track no longer holds the
    /// grant's `token` plays something else now.
    async fn follow(
        &self,
        found: &Found,
        token: &str,
        known: Option<f64>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        remote: &mut Remote<'_>,
    ) -> Result<(), Error> {
        let tv = self.tv.tv();
        // A Cast device playing a stream says its length is what it has read so far.
        let cast = matches!(remote, Remote::Cast(_));
        let mut contact = Contact::new(Instant::now());
        let mut saved = Instant::now();
        let mut resume = found.start_at.filter(|at| *at > WORTH_SAVING).map(|at| match remote {
            Remote::Dlna(_) => Resume::new(at),
            // The LOAD asked to start there.
            Remote::Cast(_) => Resume::loaded(at),
        });
        let mut polls = 0u32;
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                message = control.recv() => {
                    let Some(Control::Act(action, reply)) = message else {
                        self.save(found);
                        return Ok(());
                    };
                    let done = self.act(remote, action).await;
                    if matches!(action, CastAction::Seek { .. }) && done.is_ok() {
                        resume = None;
                    }
                    if action == CastAction::Resume && done.is_ok() {
                        self.start(found);
                    }
                    let stop = action == CastAction::Stop;
                    if stop {
                        self.save(found);
                        self.let_go(found);
                        self.view.send_modify(|cast| cast.state = CastState::Ended { error: None });
                    } else if matches!(action, CastAction::Pause) && done.is_ok() {
                        self.save(found);
                    }
                    let _ = reply.send(done.map(|()| self.view.borrow().clone()));
                    if stop {
                        return Ok(());
                    }
                    continue;
                }
            }
            polls += 1;
            let polled = self.poll(remote, polls % VOLUME_EVERY == 1).await;
            let now = Instant::now();
            contact.heard(now, &polled);
            if let Ok(Polled { failed: Some(error), .. }) = &polled {
                self.save(found);
                return Err(error.clone());
            }
            if let Ok(polled) = polled
                && let Some(transport) = polled.transport
            {
                let elsewhere = polled.uri.as_deref().is_some_and(|uri| !uri.is_empty() && !uri.contains(token));
                match transport {
                    Transport::Playing | Transport::Paused if !elsewhere => {
                        contact.played = true;
                        let paused = transport == Transport::Paused;
                        if !paused && self.view.borrow().state == CastState::Paused {
                            self.start(found);
                        }
                        let duration = if cast { known.or(polled.duration) } else { polled.duration.or(known) };
                        resume = resume.and_then(|resume| match resume.check(polled.position, now) {
                            Check::Landed => None,
                            Check::Waiting(resume) => Some(resume),
                            Check::Lost => {
                                let at = upnp::clock(resume.at);
                                let there = polled.position.map_or("where it is".into(), upnp::clock);
                                log(&format!(
                                    "{} did not go to {at} after {RESUME_TRIES} seeks, so it plays from {there}",
                                    tv.name
                                ));
                                None
                            }
                        });
                        let position = resume.map(|resume| resume.at).or(polled.position);
                        self.view.send_modify(|cast| {
                            cast.state = if paused { CastState::Paused } else { CastState::Playing };
                            cast.position = position.or(cast.position);
                            cast.duration = duration;
                            cast.volume = polled.volume.or(cast.volume);
                        });
                        // A TV that says it plays may not have read the video yet, and loses a seek
                        // then: it can seek once it knows the length, or once it is under way.
                        let seekable = polled.duration.is_some() || polled.position.is_some_and(|at| at > 0.0);
                        if !paused
                            && seekable
                            && let Some(Resume { at, seek: Seek::Due(tries) }) = resume
                        {
                            let left = tries - 1;
                            resume = match self.seek(remote, at).await {
                                Ok(()) => Some(Resume { at, seek: Seek::Taken(Instant::now(), left) }),
                                Err(_) if left > 0 => Some(Resume { at, seek: Seek::Due(left) }),
                                Err(failed) => {
                                    let why = Error::from(failed).message;
                                    log(&format!("{} refused to seek to {}: {why}", tv.name, upnp::clock(at)));
                                    None
                                }
                            };
                        }
                    }
                    _ if contact.played && elsewhere => {
                        self.save(found);
                        return Ok(());
                    }
                    Transport::Stopped if contact.played => {
                        self.save(found);
                        return Ok(());
                    }
                    _ => {}
                }
            }
            if let Some(error) = contact.lost(now, &tv.name) {
                self.save(found);
                return Err(error);
            }
            if saved.elapsed() >= SAVE_EVERY {
                self.save(found);
                saved = Instant::now();
            }
        }
    }

    async fn act(&self, remote: &mut Remote<'_>, action: CastAction) -> Result<(), Error> {
        let tv = self.tv.tv();
        match action {
            CastAction::Pause => {
                match remote {
                    Remote::Dlna(renderer) => {
                        self.transport(renderer, "Pause", &[("InstanceID", "0")]).await.map(drop)?
                    }
                    Remote::Cast(cast) => cast.session.pause().await?,
                }
                self.view.send_modify(|cast| cast.state = CastState::Paused);
            }
            CastAction::Resume => {
                match remote {
                    Remote::Dlna(renderer) => {
                        self.transport(renderer, "Play", &[("InstanceID", "0"), ("Speed", "1")]).await.map(drop)?
                    }
                    Remote::Cast(cast) => cast.session.play().await?,
                }
                self.view.send_modify(|cast| cast.state = CastState::Playing);
            }
            CastAction::Seek { position } => {
                let duration = self.view.borrow().duration;
                if !position.is_finite() || position < 0.0 || duration.is_some_and(|d| position > d) {
                    return Err(Error::new(ErrorCode::Usage, format!("{position} is not a time in this video")));
                }
                self.seek(remote, position).await?;
                self.view.send_modify(|cast| cast.position = Some(position));
            }
            CastAction::Stop => match remote {
                // A TV that has gone has stopped too.
                Remote::Dlna(renderer) => {
                    if let Err(Failed::Refused(_, error)) =
                        self.transport(renderer, "Stop", &[("InstanceID", "0")]).await
                    {
                        log(&format!("{} refused to stop: {error}", tv.name));
                    }
                }
                Remote::Cast(cast) => {
                    if let Err(Failed::Refused(_, error)) = cast.session.stop().await {
                        log(&format!("{} refused to stop: {error}", tv.name));
                    }
                }
            },
            CastAction::Volume { level } => {
                let level = level.min(100);
                match remote {
                    Remote::Dlna(renderer) => {
                        let Some(rendering) = &renderer.rendering else {
                            return Err(Error::new(
                                ErrorCode::Usage,
                                format!("newsreel cannot set the volume of {}", tv.name),
                            ));
                        };
                        let desired = level.to_string();
                        let args = [("InstanceID", "0"), ("Channel", "Master"), ("DesiredVolume", desired.as_str())];
                        self.call(rendering, "SetVolume", &args).await?;
                    }
                    Remote::Cast(cast) => cast.session.volume(level).await?,
                }
                self.view.send_modify(|cast| cast.volume = Some(level));
            }
        }
        Ok(())
    }

    /// Seeks by the time into the video, or by the absolute time on a TV that has only that. A
    /// Cast device playing a stream gets it again from there, as it seeks only in what it read.
    async fn seek(&self, remote: &mut Remote<'_>, position: f64) -> Result<(), Failed> {
        match remote {
            Remote::Dlna(renderer) => {
                let target = upnp::clock(position);
                let seek = |unit| [("InstanceID", "0"), ("Unit", unit), ("Target", target.as_str())];
                match self.transport(renderer, "Seek", &seek("REL_TIME")).await {
                    Err(Failed::Refused(710, _)) => self.transport(renderer, "Seek", &seek("ABS_TIME")).await.map(drop),
                    done => done.map(drop),
                }
            }
            Remote::Cast(cast) => cast.seek(position).await,
        }
    }

    /// What the TV does, and where it is when it says. A TV that faults on where it is still
    /// says whether it plays.
    async fn poll(&self, remote: &mut Remote<'_>, volume: bool) -> Result<Polled, Failed> {
        let renderer = match remote {
            Remote::Dlna(renderer) => *renderer,
            Remote::Cast(cast) => return cast.poll(volume, &self.tv.tv().name).await,
        };
        let transport = self.transport(renderer, "GetTransportInfo", &[("InstanceID", "0")]).await?;
        let position = self.transport(renderer, "GetPositionInfo", &[("InstanceID", "0")]).await.unwrap_or_default();
        let volume = match (&renderer.rendering, volume) {
            (Some(rendering), true) => {
                let args = [("InstanceID", "0"), ("Channel", "Master")];
                let answer = self.call(rendering, "GetVolume", &args).await.ok();
                answer.and_then(|answer| xml::text(&answer, "CurrentVolume")?.parse::<u8>().ok()).map(|v| v.min(100))
            }
            _ => None,
        };
        Ok(Polled::read(&transport, &position, volume))
    }

    async fn transport(&self, tv: &Renderer, action: &str, args: &[(&str, &str)]) -> Result<String, Failed> {
        self.call(&tv.transport, action, args).await
    }

    async fn call(&self, service: &Service, action: &str, args: &[(&str, &str)]) -> Result<String, Failed> {
        upnp::call(&self.ctx.casts.http, service, action, args, &self.tv.tv().name).await
    }

    /// The TV resumed the video, so it keeps the place from now on.
    fn start(&self, found: &Found) {
        if let Some(video) = &found.video {
            self.ctx.titles.watching.start(&Screen::Cast(self.id), video);
        }
    }

    /// The cast stops before it answers, so another screen that plays the video keeps the place
    /// from its next save.
    fn let_go(&self, found: &Found) {
        if let Some(video) = &found.video {
            self.ctx.titles.watching.stop(&Screen::Cast(self.id), video);
        }
    }

    /// Keeps where the TV is in the watch history, unless another screen started or resumed the
    /// video since.
    fn save(&self, found: &Found) {
        let cast = self.view.borrow().clone();
        let (Some(video), Some(position), Some(duration)) = (&found.video, cast.position, cast.duration) else {
            return;
        };
        let watched = Watched {
            video: video.clone(),
            position: position.min(duration),
            duration: Some(duration),
            release: found.release,
            at: now(),
        };
        if let Err(error) = self.ctx.titles.watching.save(&Screen::Cast(self.id), &watched) {
            log(&format!("cannot keep where {} stopped: {error}", found.name));
        }
    }
}

/// What a cast sends a Cast device, for as long as it plays.
struct Sending<'a> {
    grant: super::devices::CastGrant,
    serving: Serving<'a>,
    load: google::Load,
    sent: Sent,
    /// Seconds, as ffprobe or the film's plan says.
    duration: Option<f64>,
}

/// The largest video a Cast device gets once its player refused a film as large as the video:
/// 720p, which a Chromecast with Google TV on a 720p screen took in HLS when it refused 768p.
const FIT: (u32, u32) = (1280, 720);

/// What a cast drives: a DLNA renderer by its SOAP actions, or a Cast device by its session.
enum Remote<'a> {
    Dlna(&'a Renderer),
    Cast(Box<CastRemote>),
}

/// How a cast sent its video to a Cast device.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Sent {
    AsIs,
    Film,
    /// A stream, whose URL is `base` with the time it starts at.
    Stream {
        base: String,
    },
}

impl Sent {
    fn name(&self) -> &'static str {
        match self {
            Sent::AsIs => "the file as it is",
            Sent::Film => "an HLS film converted as it plays",
            Sent::Stream { .. } => "a converted stream",
        }
    }
}

/// A Cast session, and what it takes to load the video again where it was.
struct CastRemote {
    session: google::Session,
    load: google::Load,
    sent: Sent,
    /// Where the device last played, and since when it has not.
    played_at: Option<f64>,
    stuck_since: Option<Instant>,
    /// The device played the video to its end.
    finished: bool,
}

/// How long a Cast device may buffer, having played, before the cast loads the video again where
/// it was: a stall from which the device does not come back by itself.
const STALLED_FOR: Duration = Duration::from_secs(20);

impl CastRemote {
    fn new(session: google::Session, load: google::Load, sent: Sent) -> Self {
        Self { session, load, sent, played_at: None, stuck_since: None, finished: false }
    }

    async fn seek(&mut self, position: f64) -> Result<(), Failed> {
        match &self.sent {
            Sent::Stream { base } => {
                let load = google::Load { url: stream_url(base, position), start_at: position, ..self.load.clone() };
                self.reload(load).await
            }
            Sent::AsIs | Sent::Film => self.session.seek(position).await,
        }
    }

    async fn reload(&mut self, load: google::Load) -> Result<(), Failed> {
        self.session.load(&load).await?;
        self.load = load;
        self.stuck_since = None;
        Ok(())
    }

    async fn poll(&mut self, volume: bool, name: &str) -> Result<Polled, Failed> {
        let status = self.session.poll(volume).await?;
        let now = Instant::now();
        let failed = status.failed.as_ref().map(|why| {
            Error::new(ErrorCode::FetchFailed, format!("{name} could not play the video ({why})"))
                .with_hint("play it here, or choose another source")
        });
        let transport = match status.player {
            _ if status.closed => Some(Transport::Stopped),
            google::Player::Playing => Some(Transport::Playing),
            google::Player::Paused => Some(Transport::Paused),
            google::Player::Buffering | google::Player::Loading => Some(Transport::Busy),
            google::Player::Idle(google::Idle::Unsaid) => Some(Transport::Busy),
            google::Player::Idle(google::Idle::Finished) => {
                self.finished = true;
                Some(Transport::Stopped)
            }
            google::Player::Idle(_) => Some(Transport::Stopped),
        };
        match transport {
            Some(Transport::Playing | Transport::Paused) => {
                self.played_at = status.time.or(self.played_at);
                self.stuck_since = None;
            }
            Some(Transport::Busy) => {
                let since = *self.stuck_since.get_or_insert(now);
                // A stream the device's own remote sent past what it read goes back to the start
                // and stalls there, and never says where it was going.
                let lost = matches!(self.sent, Sent::Stream { .. })
                    && status.time.is_some_and(|t| t < 1.0)
                    && self.played_at.is_some_and(|at| at > 10.0);
                if let Some(at) = self.played_at.filter(|_| lost || now.saturating_duration_since(since) >= STALLED_FOR)
                {
                    log(&format!("{name} stalled, so the cast loads the video again at {}", upnp::clock(at)));
                    let load = match &self.sent {
                        Sent::Stream { base } => {
                            google::Load { url: stream_url(base, at), start_at: at, ..self.load.clone() }
                        }
                        Sent::AsIs | Sent::Film => google::Load { start_at: at, ..self.load.clone() },
                    };
                    self.reload(load).await?;
                }
            }
            _ => {}
        }
        Ok(Polled {
            transport,
            position: status.time,
            duration: status.duration,
            uri: status.content,
            volume: status.volume,
            failed,
        })
    }
}

/// The name of an HLS film's playlist, and of a stream, in a cast grant's folder.
const FILM: &str = "film.m3u8";
const STREAM: &str = "film.mp4";

fn stream_url(base: &str, at: f64) -> String {
    if at > 0.0 { format!("{base}?t={at:.3}") } else { base.to_string() }
}

/// Where a cast resumes the video, which it shows until the TV gets there, so the bar does not
/// show the top of the video while the TV starts there and seeks.
#[derive(Clone, Copy, Debug)]
struct Resume {
    /// Seconds.
    at: f64,
    seek: Seek,
}

#[derive(Clone, Copy, Debug)]
enum Seek {
    /// To send once the TV can take it, with this many tries left, at least one.
    Due(u32),
    /// The TV took it at this time, with this many tries left should it not get there.
    Taken(Instant, u32),
    /// A Cast device was asked to start there, with this many tries left should it not.
    Loaded(u32),
}

/// How resuming goes, by where the TV says it is.
#[derive(Clone, Copy, Debug)]
enum Check {
    /// The TV got there.
    Landed,
    Waiting(Resume),
    /// Every seek is spent and the TV is not there, so the cast shows where it is.
    Lost,
}

impl Resume {
    fn new(at: f64) -> Self {
        Self { at, seek: Seek::Due(RESUME_TRIES) }
    }

    /// For a Cast device that was asked to start at `at` with the video.
    fn loaded(at: f64) -> Self {
        Self { at, seek: Seek::Loaded(RESUME_TRIES - 1) }
    }

    /// Whether the TV, at `position` at `now`, got there, or took the seek `RESUME_WITHIN` ago
    /// and is still not there, which sends it again while tries are left.
    fn check(self, position: Option<f64>, now: Instant) -> Check {
        if position.is_some_and(|position| position >= self.at - RESUME_SLACK) {
            return Check::Landed;
        }
        match self.seek {
            Seek::Taken(taken, left) if now.saturating_duration_since(taken) >= RESUME_WITHIN => match left {
                0 => Check::Lost,
                left => Check::Waiting(Self { seek: Seek::Due(left), ..self }),
            },
            // It plays, and somewhere else.
            Seek::Loaded(left) if position.is_some_and(|position| position > 0.0) => match left {
                0 => Check::Lost,
                left => Check::Waiting(Self { seek: Seek::Due(left), ..self }),
            },
            _ => Check::Waiting(self),
        }
    }
}

/// When a cast last heard from its TV, which says when it gives up on the TV.
#[derive(Clone, Copy, Debug)]
struct Contact {
    started: Instant,
    /// The last answer of any kind, a fault too.
    answered: Instant,
    /// The last poll that said what the TV does.
    said: Instant,
    /// Whether the TV has played the cast.
    played: bool,
}

impl Contact {
    fn new(now: Instant) -> Self {
        Self { started: now, answered: now, said: now, played: false }
    }

    /// Takes in one poll at `now`. A fault or an answer without a transport state is an answer,
    /// but it does not say what the TV does.
    fn heard(&mut self, now: Instant, polled: &Result<Polled, Failed>) {
        match polled {
            Ok(Polled { transport: Some(_), .. }) => (self.answered, self.said) = (now, now),
            Ok(_) | Err(Failed::Refused(..)) => self.answered = now,
            Err(Failed::Gone(_)) => {}
        }
    }

    /// Why the cast ends at `now`, if it does: the TV `tv` names has stopped answering, has failed
    /// every poll for a minute, or never started playing.
    fn lost(&self, now: Instant, tv: &str) -> Option<Error> {
        if now.saturating_duration_since(self.answered) >= LOST_AFTER {
            let error = Error::new(ErrorCode::NetworkUnavailable, format!("{tv} stopped answering"));
            return Some(error.with_hint("check that the TV is on, then press Play on TV again"));
        }
        if now.saturating_duration_since(self.said) >= FAILING_FOR {
            let error = Error::new(ErrorCode::FetchFailed, format!("{tv} stopped saying what it plays"));
            return Some(error.with_hint("check what the TV shows, then press Play on TV again"));
        }
        if !self.played && now.saturating_duration_since(self.started) >= START_WITHIN {
            let error = Error::new(ErrorCode::FetchFailed, format!("{tv} took the video but did not start playing it"));
            return Some(
                error.with_hint("check what the TV shows, and play it here if the TV says it cannot play the file"),
            );
        }
        None
    }
}

/// What a TV's AVTransport says it is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    Playing,
    Paused,
    /// Stopped, or holding nothing to play.
    Stopped,
    /// Loading, seeking, or a state newsreel has no use for.
    Busy,
}

impl Transport {
    /// The state a TV says, or `None` for a state that UPnP AV does not name, or none at all.
    fn of(state: &str) -> Option<Self> {
        match state {
            "PLAYING" => Some(Self::Playing),
            "PAUSED_PLAYBACK" => Some(Self::Paused),
            "STOPPED" | "NO_MEDIA_PRESENT" => Some(Self::Stopped),
            "TRANSITIONING" | "PAUSED_RECORDING" | "RECORDING" => Some(Self::Busy),
            _ => None,
        }
    }
}

struct Polled {
    /// `None` when its answer said no state, which says nothing of what it does.
    transport: Option<Transport>,
    position: Option<f64>,
    duration: Option<f64>,
    uri: Option<String>,
    volume: Option<u8>,
    /// Why the TV gave up on the video, when it says.
    failed: Option<Error>,
}

impl Polled {
    /// Reads the answers to GetTransportInfo and GetPositionInfo.
    fn read(transport: &str, position: &str, volume: Option<u8>) -> Self {
        Polled {
            transport: Transport::of(&xml::text(transport, "CurrentTransportState").unwrap_or_default()),
            position: xml::text(position, "RelTime").and_then(|t| upnp::seconds(&t)),
            duration: xml::text(position, "TrackDuration").and_then(|t| upnp::seconds(&t)).filter(|d| *d > 0.0),
            uri: xml::text(position, "TrackURI"),
            volume,
            failed: None,
        }
    }
}

/// Returns once the cast `view` follows has ended, and can send its TV nothing more.
async fn ended(mut view: watch::Receiver<Cast>) {
    let _ = view.wait_for(Cast::ended).await;
}

/// Answers every action but a stop while a cast finds its source, and returns once it is stopped.
async fn until_stopped(control: &mut mpsc::UnboundedReceiver<Control>) {
    while let Some(message) = control.recv().await {
        match message {
            Control::Act(CastAction::Stop, reply) => {
                let _ = reply.send(Err(Error::new(ErrorCode::Interrupted, "the cast stopped before it started")));
                return;
            }
            Control::Act(_, reply) => {
                let _ = reply.send(Err(Error::new(ErrorCode::Usage, "the cast has not reached the TV yet")
                    .with_hint("wait until it plays, or stop it")));
            }
            Control::Leave => return,
        }
    }
    std::future::pending().await
}

/// The download and file to play: the first source of a title that starts, or a release's video.
async fn find(
    ctx: &Ctx,
    tv: &Device,
    request: &CastRequest,
    steps: &broadcast::Sender<AttemptEvent>,
    view: &watch::Sender<Cast>,
) -> Result<Found, Error> {
    let (id, file, name, video, release, resume_at) = match &request.target {
        Target::Title(video) => {
            let pin = match &request.source {
                Some(prefix) => {
                    let release = titles::pinned(ctx, video, prefix).await?;
                    if !Plays::tv(tv.tv()).decodes(Quality::parse(&release.name)) {
                        return Err(Error::new(
                            ErrorCode::Usage,
                            format!("{} cannot decode the video of this source", tv.tv().name),
                        )
                        .with_hint("let newsreel choose a source, or play it here"));
                    }
                    Some(release.id)
                }
                None => None,
            };
            let ask = PlayTitle {
                video: video.clone(),
                pin,
                replace: None,
                // A TV says what it decodes but not its screen, and 1080p plays well on any TV.
                player: Player::Device(Viewer { screen: Resolution::P1080, playback: tv.tv().decodes, decoders: None }),
                plays: Plays::tv(tv.tv()),
                to: Destination::Plays,
            };
            let outcome = attempt::run(ctx, ask, |step| {
                if let AttemptEvent::Resolved { title, video, .. } = &step {
                    view.send_modify(|cast| cast.name = title_name(title, video.episode));
                }
                let _ = steps.send(step);
            })
            .await;
            match outcome {
                Outcome::Played(played) => {
                    let Played { name, video, release, job, file, resume_at, .. } = *played;
                    (job, file, name, Some(video), Some(release.id), resume_at)
                }
                Outcome::Failed { error, .. } => return Err(error),
            }
        }
        target => {
            let (source, layout, playable) = downloads::prepare(ctx, target.clone(), request.file, None).await?;
            let path = sanitize(&layout.files[playable.video as usize].path);
            if !Plays::tv(tv.tv()).opens(&path, ctx.media.ffmpeg.converts()) {
                return Err(refused(tv.tv(), &path));
            }
            let (job, _) = downloads::begin(ctx, source, &playable).await?;
            let video = request.video.clone().or_else(|| job.title.as_ref().and_then(video_of));
            let release = job.title.as_ref().and_then(|link| link.release);
            let resume_at = match &video {
                Some(video) => ctx.titles.watching.resume_at(video)?,
                None => None,
            };
            let name = match &video {
                Some(video) => ctx
                    .titles
                    .catalog
                    .title(&video.title)
                    .await
                    .map_or_else(|_| file_name(&path), |details| title_name(&details.card, video.episode)),
                None => file_name(&path),
            };
            (job.id, playable.video, name, video, release, resume_at)
        }
    };
    let info = downloads::download_files(ctx, id).await?;
    let entry = info
        .layout
        .files
        .get(file as usize)
        .ok_or_else(|| Error::internal(format!("the download has no file {file}")))?;
    let path = sanitize(&entry.path);
    let subtitle = match request.subtitle {
        None => None,
        Some(index) => {
            let playable = downloads::choose(&info.layout, Some(file), None)?;
            if !playable.subtitles.contains(&index) {
                return Err(Error::new(
                    ErrorCode::Usage,
                    format!("file {index} is not a subtitle of {}", file_name(&path)),
                )
                .with_hint(format!(
                    "run newsreel files {} to list its files",
                    &id.to_hex()[..newsreel_core::id::SHORT_ID]
                )));
            }
            Some((index, sanitize(&info.layout.files[index as usize].path)))
        }
    };
    Ok(Found {
        id,
        file,
        size: entry.size,
        path,
        name,
        video,
        release,
        start_at: request.start_at.or(resume_at),
        subtitle,
    })
}

/// The film or episode a download made by playing a title holds, if it holds just one.
fn video_of(link: &TitleLink) -> Option<VideoRef> {
    let episode = match link.span {
        Span::Film => None,
        Span::Episode(episode) => Some(episode),
        Span::Season(_) | Span::Seasons { .. } | Span::Complete => return None,
    };
    Some(VideoRef { title: link.title.clone(), episode })
}

fn refused(tv: &Tv, path: &str) -> Error {
    let container = extension(path).to_ascii_uppercase();
    let plays: Vec<String> = tv.plays.iter().map(|c| c.name().to_ascii_uppercase()).collect();
    let error = Error::new(
        ErrorCode::Usage,
        format!("{} does not play {container} files, only {}", tv.name, plays.join(" and ")),
    );
    match tv.protocol {
        Protocol::Dlna => error.with_hint("play it here, or choose a source in a format the TV plays"),
        Protocol::GoogleCast => {
            error.with_hint(format!("{} to convert it for the TV, or play it here", crate::ffmpeg::INSTALL))
        }
    }
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// This Mac's address on the network the TV at `address` is on.
fn facing(address: &str) -> Result<IpAddr, Error> {
    let unreachable = |e: std::io::Error| {
        Error::new(ErrorCode::NetworkUnavailable, format!("newsreel has no way to the TV at {address} ({e})"))
    };
    let tv: IpAddr = address.parse().map_err(|_| Error::internal(format!("{address} is not an IP address")))?;
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(unreachable)?;
    socket.connect((tv, ssdp::GROUP.port())).map_err(unreachable)?;
    Ok(socket.local_addr().map_err(unreachable)?.ip())
}

/// Waits until this engine answers at `host`, where the TV will ask for the video. The engine
/// listens on the home network once a grant is live, and until then, or while another program
/// holds the port there, something else may answer, so only the engine's own word counts.
async fn reachable(ctx: &Ctx, host: IpAddr) -> Result<(), Error> {
    let url = format!("http://{}{HELLO}", std::net::SocketAddr::new(host, ctx.http_port));
    let deadline = Instant::now() + REACHABLE_WITHIN;
    while !ours(&ctx.casts, &url).await {
        if Instant::now() >= deadline {
            let why = ctx.devices.list(ctx.http_port).unreachable;
            return Err(Error::new(
                ErrorCode::NetworkUnavailable,
                why.unwrap_or_else(|| format!("the TV cannot reach newsreel at {host}:{}", ctx.http_port)),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Whether what answers at `url` is this engine, by the word it gives.
async fn ours(casts: &Casts, url: &str) -> bool {
    let Ok(answer) = casts.http.get(url).timeout(Duration::from_secs(1)).send().await else { return false };
    let hello = casts.hello.as_bytes();
    answer.content_length() == Some(hello.len() as u64) && answer.bytes().await.is_ok_and(|body| body == hello)
}

/// The files a cast serves, listed for the HTTP server until it is dropped.
struct Serving<'a> {
    casts: &'a Casts,
    paths: Vec<String>,
}

impl<'a> Serving<'a> {
    fn new(casts: &'a Casts, served: Vec<(String, Served)>) -> Self {
        let paths = served.iter().map(|(path, _)| path.clone()).collect();
        casts.state().served.extend(served);
        Self { casts, paths }
    }
}

impl Drop for Serving<'_> {
    fn drop(&mut self) {
        let mut state = self.casts.state();
        for path in &self.paths {
            state.served.remove(path);
        }
    }
}

/// A cast's claim on its video's place in the watch history, from its start until it is
/// dropped. A screen that starts or resumes the video later takes the place from it.
struct Keeping<'a> {
    watching: &'a Watching,
    video: VideoRef,
    screen: Screen,
}

impl<'a> Keeping<'a> {
    fn new(watching: &'a Watching, video: &VideoRef, id: u64) -> Self {
        let screen = Screen::Cast(id);
        watching.start(&screen, video);
        Self { watching, video: video.clone(), screen }
    }
}

impl Drop for Keeping<'_> {
    fn drop(&mut self) {
        self.watching.stop(&self.screen, &self.video);
    }
}

#[cfg(test)]
mod tests;
