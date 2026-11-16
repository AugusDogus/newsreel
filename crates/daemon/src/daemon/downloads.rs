//! Downloads by reference: finding what a target names, its files, and getting or playing it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::playback::media::RUNTIME_BYTES;
use crate::playback::stream::{self, Playable};
use newsreel_core::Plays;
use newsreel_core::protocol::{Change, Event, FilesReply, GetRequest, PlayReply, PlayRequest, Stream};
use newsreel_core::text::sanitize;
use newsreel_core::{
    DownloadId, Error, ErrorCode, HexPrefix, Job, JobStatus, Release, ReleaseFile, ResultId, SearchQuery, SearchResult,
    Target,
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use super::BoxFuture;
use super::ctx::Ctx;
use super::files;
use super::http;
use super::layout::Layout;
use super::owner::plays::Destination;
use super::owner::{Command, FileBytes, FilesInfo, Finish, GetCommand, Origin, PlayCommand, Resolved, Source, Update};
use super::search_port::Progress;

/// An NZB for an 80 GB release lists over 100,000 articles, about 15 MB of XML, and its
/// manifest is about as large.
const MAX_RELEASE_BYTES: u64 = 64 * 1024 * 1024;
/// How often a stream of download events with nothing new says it is still there.
const HEARTBEAT: Duration = Duration::from_secs(15);

pub(super) async fn search(ctx: &Ctx, query: SearchQuery) -> Result<SearchResult, Error> {
    let (preferences, player) = ((ctx.preferences)(), query.profile.player);
    let rank_for = || ctx.machine.profile(preferences, player);
    let mut result = ctx.search.search_stream(query, &rank_for, tokio::sync::mpsc::unbounded_channel().0).await?;
    record(ctx, &mut result);
    Ok(result)
}

/// A search as it runs. Each answer is recorded, as `search` records its results.
pub(super) struct Searching<'a> {
    ctx: &'a Ctx,
    /// `None` once every source has answered or the deadline has passed.
    running: Option<BoxFuture<'a, Result<SearchResult, Error>>>,
    answers: mpsc::UnboundedReceiver<Progress>,
}

/// What a running search has to say.
pub(super) enum Answer {
    /// What the sources that have answered found, and the sources still searching.
    SoFar(Progress),
    /// The final results, once every source has answered or the deadline has passed.
    Done(Result<SearchResult, Error>),
}

impl<'a> Searching<'a> {
    pub(super) fn start(ctx: &'a Ctx, query: SearchQuery) -> Self {
        let (progress, answers) = mpsc::unbounded_channel();
        let running = Box::pin(async move {
            let (preferences, player) = ((ctx.preferences)(), query.profile.player);
            let rank_for = || ctx.machine.profile(preferences, player);
            ctx.search.search_stream(query, &rank_for, progress).await
        });
        Self { ctx, running: Some(running), answers }
    }

    pub(super) fn over(&self) -> bool {
        self.running.is_none()
    }

    /// Waits for the next source to answer. Once the search is over it never returns.
    pub(super) async fn next(&mut self) -> Answer {
        let Some(running) = &mut self.running else { return std::future::pending().await };
        let mut answer = tokio::select! {
            biased;
            Some(progress) = self.answers.recv() => Answer::SoFar(progress),
            result = running => {
                self.running = None;
                Answer::Done(result)
            }
        };
        if let Answer::SoFar(Progress { result, .. }) | Answer::Done(Ok(result)) = &mut answer {
            record(self.ctx, result);
        }
        answer
    }
}

/// The events of some downloads: a snapshot of them, then each change, with a heartbeat while
/// nothing changes.
pub(super) struct JobEvents<'a> {
    ctx: &'a Ctx,
    /// The downloads followed, or `None` for every download, those started later too.
    followed: Option<Vec<DownloadId>>,
    updates: broadcast::Receiver<Update>,
    heartbeat: tokio::time::Interval,
    /// Changes were missed, so the next event is a fresh snapshot.
    lagged: bool,
}

impl<'a> JobEvents<'a> {
    /// Follows the downloads `refs` names, or every download when it names none, and answers the
    /// first event: a snapshot of them.
    pub(super) async fn start(ctx: &'a Ctx, refs: Vec<String>) -> Result<(Self, Event), Error> {
        let everything = refs.is_empty();
        let (jobs, updates) = ctx.owner.ask(|reply| Command::Watch { refs, reply }).await?;
        let followed = (!everything).then(|| jobs.iter().map(|job| job.id).collect());
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        Ok((Self { ctx, followed, updates, heartbeat, lagged: false }, Event::Snapshot { jobs }))
    }

    /// The next event, or `None` once the engine stops.
    pub(super) async fn next(&mut self) -> Option<Event> {
        let event = loop {
            if self.lagged {
                let refs = self.followed.iter().flatten().map(|id| id.to_hex()).collect();
                let jobs = self.ctx.owner.ask(|reply| Command::Status { refs, reply }).await.ok()?;
                self.lagged = false;
                break Event::Snapshot { jobs };
            }
            tokio::select! {
                update = self.updates.recv() => match update {
                    Ok(Update::Job(job)) if self.follows(job.id) => break Event::Job { job },
                    Ok(Update::Removed(id)) if self.follows(id) => break Event::Removed { id: id.to_hex() },
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => self.lagged = true,
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
                _ = self.heartbeat.tick() => break Event::Heartbeat { at: newsreel_core::time::now() },
            }
        };
        self.heartbeat.reset();
        Some(event)
    }

    fn follows(&self, id: DownloadId) -> bool {
        self.followed.as_ref().is_none_or(|followed| followed.contains(&id))
    }
}

/// Pauses the downloads `refs` names.
pub(super) async fn pause(ctx: &Ctx, refs: Vec<String>) -> Result<Vec<Change>, Error> {
    ctx.owner.ask(|reply| Command::Pause { refs, reply }).await
}

/// Resumes the downloads `refs` names, or every paused one when `all` is set.
pub(super) async fn resume(ctx: &Ctx, refs: Vec<String>, all: bool) -> Result<Vec<Change>, Error> {
    ctx.owner.ask(|reply| Command::Resume { refs, all, reply }).await
}

/// Removes the downloads `refs` names, and with `delete` their files too.
pub(super) async fn remove(ctx: &Ctx, refs: Vec<String>, delete: bool) -> Result<Vec<Change>, Error> {
    ctx.owner.ask(|reply| Command::Remove { refs, delete, reply }).await
}

pub(super) fn record(ctx: &Ctx, result: &mut SearchResult) {
    for release in &mut result.releases {
        release.name = sanitize(&release.name);
    }
    let saved = result.releases.iter().map(|r| (r.clone(), ctx.search.saved_link(&r.id))).collect();
    ctx.owner.tell(Command::RecordResults(saved));
}

fn too_large(what: &str, size: u64) -> Error {
    Error::new(ErrorCode::InvalidRelease, format!("{what} is {size} bytes, too large for an NZB"))
        .with_hint("check that it is an NZB and not the release itself")
}

/// Resolves an NZB into the manifest the engine reads.
async fn release(ctx: &Ctx, nzb: Vec<u8>) -> Result<Layout, Error> {
    let manifest = ctx.nzb.resolve(nzb).await?;
    if manifest.len() as u64 > MAX_RELEASE_BYTES {
        return Err(too_large("the resolved release", manifest.len() as u64));
    }
    (ctx.parse)(&manifest)
}

async fn read_nzb(ctx: &Ctx, path: &std::path::Path) -> Result<Layout, Error> {
    let unreadable =
        |e: std::io::Error| Error::new(ErrorCode::InvalidRef, format!("cannot read {}: {e}", path.display()));
    let size = tokio::fs::metadata(path).await.map_err(unreadable)?.len();
    if size > MAX_RELEASE_BYTES {
        return Err(too_large(&path.display().to_string(), size));
    }
    release(ctx, tokio::fs::read(path).await.map_err(unreadable)?).await
}

/// Fetches and resolves an NZB, from search result `from` when it names one.
async fn fetch(ctx: &Ctx, url: String, from: Option<ResultId>) -> Result<Layout, Error> {
    let layout = release(ctx, ctx.search.fetch_nzb(url).await?).await?;
    ctx.owner.tell(Command::Cache { layout: layout.clone(), from });
    Ok(layout)
}

pub(super) async fn resolve(ctx: &Ctx, target: Target, command: &'static str) -> Result<Source, Error> {
    match target {
        Target::Prefix(prefix) => {
            let prefix: HexPrefix = prefix.parse()?;
            let results = ctx.search.find(&prefix);
            match ctx.owner.ask(|reply| Command::Resolve { prefix, results, command, reply }).await? {
                Resolved::Job(id) => Ok(Source::Download(id)),
                Resolved::Release { release, cached } => release_source(ctx, *release, cached).await,
            }
        }
        Target::NzbUrl(url) => Ok(Source::Release { layout: fetch(ctx, url, None).await?, origin: Origin::Url }),
        Target::NzbFile(path) => Ok(Source::Release { layout: read_nzb(ctx, &path).await?, origin: Origin::File }),
        Target::Title(video) => Err(newsreel_core::target::not_a_release(&video, command)),
    }
}

pub(super) async fn release_source(ctx: &Ctx, release: Release, cached: Option<Arc<Layout>>) -> Result<Source, Error> {
    if let Some(layout) = cached {
        return Ok(Source::Release { layout: (*layout).clone(), origin: Origin::Cache });
    }
    let url = ctx.search.nzb_url(&release)?.ok_or_else(|| {
        Error::new(ErrorCode::InvalidRef, format!("{} has no NZB link", sanitize(&release.name)))
            .with_hint("search again; the indexer may have removed it")
    })?;
    Ok(Source::Release { layout: fetch(ctx, url, Some(release.id)).await?, origin: Origin::Url })
}

/// The files of the download `id`.
pub(super) async fn download_files(ctx: &Ctx, id: DownloadId) -> Result<FilesInfo, Error> {
    ctx.owner.ask(|reply| Command::Files { id, reply }).await
}

pub(super) async fn get(ctx: &Ctx, request: GetRequest) -> Result<newsreel_core::protocol::GetReply, Error> {
    let GetRequest { target, dir, files, max_size, dry_run } = request;
    let setup_max_size = (ctx.preferences)().max_size;
    let source = resolve(ctx, target, "get").await?;
    let get = GetCommand { source, dir, files, max_size, setup_max_size, dry_run };
    ctx.owner.ask(|reply| Command::Get { get: Box::new(get), reply }).await
}

pub(super) async fn play(ctx: &Ctx, request: PlayRequest) -> Result<PlayReply, Error> {
    let _playing = ctx.playing.open()?;
    let PlayRequest { target, dir, files } = request;
    let setup_max_size = (ctx.preferences)().max_size;
    let source = resolve(ctx, target, "play").await?;
    let to = Destination::Folder(dir);
    let play = PlayCommand { source, to, files: files.clone(), setup_max_size, title: None, finish: Finish::Stream };
    let (job, layout) = ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await?;
    let streams = streams(ctx, job.id, &layout, files);
    Ok(PlayReply { job, streams })
}

/// Where `files` of the download `id` are served, in the order given.
pub(super) fn streams(ctx: &Ctx, id: DownloadId, layout: &Layout, files: Vec<u32>) -> Vec<Stream> {
    files
        .into_iter()
        .map(|index| {
            let Served { index, path, size, .. } = served(id, layout, index);
            let url = http::url(ctx.http_port, id, index, &path);
            Stream { index, path, size, url }
        })
        .collect()
}

/// A file the engine serves while it downloads.
#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "StreamedFile"))]
pub struct Served {
    pub index: u32,
    pub path: String,
    pub size: u64,
    /// The path of `http::url`, for a page on the engine's own origin.
    pub url: String,
    /// Where the file starts in the release, in bytes.
    pub offset: u64,
}

/// File `index` of the download `id`, as the engine serves it.
pub(super) fn served(id: DownloadId, layout: &Layout, index: u32) -> Served {
    let file = &layout.files[index as usize];
    let path = sanitize(&file.path);
    Served {
        index,
        url: http::route_of(id, index, &path),
        path,
        size: file.size,
        offset: layout.files[..index as usize].iter().map(|f| f.size).sum(),
    }
}

pub(super) async fn files(ctx: &Ctx, target: Target) -> Result<FilesReply, Error> {
    let started = Instant::now();
    let info = match resolve(ctx, target, "files").await? {
        Source::Download(id) => download_files(ctx, id).await?,
        Source::Release { layout, origin } => {
            FilesInfo { id: layout.id, layout: Arc::new(layout), source: origin.source(), download: None }
        }
    };
    let m = &info.layout;
    Ok(FilesReply {
        id: info.id.to_hex(),
        name: sanitize(&m.name),
        total_size: m.total(),
        files: listed(ctx, &info).await,
        source: info.source,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
    })
}

/// A release's files, with what its download has selected and verified of each.
pub(super) async fn listed(ctx: &Ctx, info: &FilesInfo) -> Vec<ReleaseFile> {
    let m = &info.layout;
    let verified: Vec<u64> = match info.download.as_ref().map(|d| &d.verified) {
        Some(FileBytes::Known(bytes)) => bytes.clone(),
        Some(FileBytes::Engine { saved }) => {
            let id = info.id;
            ctx.engine.ask(move |e| e.file_progress(id)).await.unwrap_or_else(|_| saved.clone())
        }
        None => Vec::new(),
    };
    let risks = files::risks(m);
    m.files
        .iter()
        .enumerate()
        .map(|(i, f)| ReleaseFile {
            index: i as u32,
            path: sanitize(&f.path),
            size: f.size,
            selected: info.download.as_ref().is_some_and(|d| d.wanted[i]),
            verified: verified.get(i).copied().unwrap_or(0),
            risk: risks[i],
        })
        .collect()
}

/// How many runs `pieces` cuts a file into. The player's seek bar draws one mark for each.
pub(super) const PIECE_RUNS: u32 = 200;

/// Which parts of one file are on disk, as `PIECE_PIECE_RUNS` characters of `0` to `8`.
pub async fn pieces(ctx: &Ctx, id: DownloadId, index: u32) -> Result<String, Error> {
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    if file.complete || file.size == 0 {
        return Ok("8".repeat(PIECE_RUNS as usize));
    }
    let size = file.size;
    let runs = match ctx.engine.ask(move |e| e.written(id, index, 0, size, PIECE_RUNS)).await {
        Ok(runs) => runs,
        // The engine does not hold a download that just finished, one that is stopped, or one that
        // has not started yet.
        Err(_) => {
            let jobs = ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await?;
            if jobs.first().is_some_and(|job| matches!(job.status, JobStatus::Done { .. })) {
                return Ok("8".repeat(PIECE_RUNS as usize));
            }
            if jobs.first().is_none_or(|job| job.status.is_stopped()) {
                let name = super::media::name(&file);
                return Err(Error::new(
                    ErrorCode::NotFound,
                    format!("{name} is not downloading now, so what has arrived of it is not known"),
                )
                .with_hint("resume the download, or play the file again"));
            }
            return Ok("0".repeat(PIECE_RUNS as usize));
        }
    };
    Ok(runs.into_iter().map(|run| run.symbol()).collect())
}

/// The release `target` names, its files, and the video to watch in it with its subtitles: file
/// `file`, or the main video. `reference` is how a person named the release, for the hint when
/// it has no video.
pub(super) async fn prepare(
    ctx: &Ctx,
    target: Target,
    file: Option<u32>,
    reference: Option<&str>,
) -> Result<(Source, Arc<Layout>, Playable), Error> {
    let (source, layout) = match resolve(ctx, target, "play").await? {
        Source::Download(id) => (Source::Download(id), download_files(ctx, id).await?.layout),
        Source::Release { layout, origin } => {
            let shared = Arc::new(layout.clone());
            (Source::Release { layout, origin }, shared)
        }
    };
    let playable = choose(&layout, file, reference)?;
    Ok((source, layout, playable))
}

/// Starts or joins the download of `playable`'s video and subtitles, which come first and in
/// playing order.
pub(super) async fn begin(ctx: &Ctx, source: Source, playable: &Playable) -> Result<(Job, FilesInfo), Error> {
    let files = [playable.video].into_iter().chain(playable.subtitles.iter().copied()).collect();
    let setup_max_size = (ctx.preferences)().max_size;
    let to = Destination::Plays;
    let play = PlayCommand { source, to, files, setup_max_size, title: None, finish: Finish::Stream };
    let (job, _) = ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await?;
    let info = download_files(ctx, job.id).await?;
    fetch_end(ctx, job.id, playable.video, info.layout.files[playable.video as usize].size);
    Ok((job, info))
}

/// Asks the engine for the end of file `index` of a download that starts playing. A player, and
/// ffprobe, read the file's index there before they can start, so it comes with the first bytes
/// instead of after them. The engine does this beside the play, which does not wait for it.
pub(super) fn fetch_end(ctx: &Ctx, id: DownloadId, index: u32, size: u64) {
    let length = size.min(RUNTIME_BYTES);
    ctx.engine.run(move |engine| {
        let _ = engine.want_range(id, index, size - length, length);
    });
}

/// The file to watch and its subtitles: the one the page chose, or the release's main video.
/// `reference` is how a person named the release, for the hint when it has no video.
pub(super) fn choose(layout: &Layout, file: Option<u32>, reference: Option<&str>) -> Result<Playable, Error> {
    let listing = layout.listing();
    let name = sanitize(&layout.name);
    match file {
        Some(index) => stream::around(&listing, index).ok_or_else(|| {
            let count = listing.len();
            let files = if count == 1 { "1 file".to_string() } else { format!("{count} files") };
            Error::new(ErrorCode::Usage, format!("{name} has {files}, so it has no file {index}"))
        }),
        None => stream::pick(&listing, Plays::BROWSER).ok_or_else(|| {
            let error = Error::new(ErrorCode::NoSelection, format!("{name} has no video file"));
            // An NZB URL can carry an API key, so it is never shown.
            match reference.map(str::trim) {
                Some(reference) if reference.contains("://") => {
                    error.with_hint("list its files with newsreel files and the same NZB URL")
                }
                Some(reference) => {
                    let (lead, hex) = reference.strip_prefix('u').map_or(("", reference), |rest| ("u", rest));
                    let short = &hex[..hex.len().min(newsreel_core::id::SHORT_ID)];
                    error.with_hint(format!("run newsreel files {lead}{short} to list its files"))
                }
                None => error,
            }
        }),
    }
}
