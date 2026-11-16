use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use newsreel_core::protocol::{Change, ChangeResult, FileListSource, GetOutcome, GetReply};
use newsreel_core::time::now;
use newsreel_core::{
    DownloadId, Error, ErrorCode, HexPrefix, Job, PieceMap, Release, ResultId, Selection, Target, TitleCard, TitleId,
    TitleLink,
};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::admission::{self, Cap, Plan};
use super::book::{self, Home, Intent, JobEntry, JobRecord, Linked, Observed, Phase, Verify};
use super::engine_port::{Activity, AddDownload, EngineEvent, EngineHandle};
use super::files;
use super::layout::{Layout, ParseLayout};
use super::playing::Playing;
use super::resolve::{self, Found, Scope, no_download};
use super::search_port::SavedLink;
use super::shelf::Shelf;
use super::store::{Store, Tombstone};
use crate::disk::{Disk, Space};
use crate::log::{log, persist};
use deleter::Deleter;
use holds::{Before, Holds, OnDisk, PutBack};
use plays::{Destination, Plays, Tending};
use reach::Reach;
pub use streams::Lease;
use streams::Streams;

pub mod deleter;
mod holds;
pub mod plays;
mod reach;
mod streams;
pub mod trash;

#[derive(Clone, Debug)]
pub struct OwnerConfig {
    pub default_dir: PathBuf,
    pub max_active: usize,
    pub save_resume_every: Duration,
    pub stall_after: Duration,
    pub writes_stop_within: Duration,
    pub plays: Plays,
    /// Where the files of a download in a folder the person chose go when they delete them.
    pub trash: trash::Trash,
    /// How long a play leaving the plays folder waits before it goes: no time, except in a test
    /// that does something in between.
    pub deletion_pause: Duration,
    pub trash_pause: deleter::TrashPause,
}

pub type Reply<T> = oneshot::Sender<Result<T, Error>>;

/// How long the engine waits as it starts for the folders of its downloads to answer. Those that
/// take longer start once they do.
const FOLDERS_AT_START: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub enum Source {
    Download(DownloadId),
    Release { layout: Layout, origin: Origin },
}

/// Where a release's file list came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    File,
    Url,
    Cache,
}

impl Origin {
    pub fn source(self) -> FileListSource {
        match self {
            Self::File => FileListSource::NzbFile,
            Self::Url => FileListSource::NzbUrl,
            Self::Cache => FileListSource::Cache,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Resolved {
    Job(DownloadId),
    Release { release: Box<Release>, cached: Option<Arc<Layout>> },
}

#[derive(Clone, Debug)]
pub struct FilesInfo {
    pub id: DownloadId,
    pub layout: Arc<Layout>,
    pub source: FileListSource,
    pub download: Option<DownloadFiles>,
}

#[derive(Clone, Debug)]
pub struct DownloadFiles {
    pub wanted: Vec<bool>,
    pub verified: FileBytes,
}

#[derive(Clone, Debug)]
pub enum FileBytes {
    Known(Vec<u64>),
    Engine { saved: Vec<u64> },
}

pub struct GetCommand {
    pub source: Source,
    pub dir: Option<PathBuf>,
    pub files: Vec<String>,
    pub max_size: Option<u64>,
    pub setup_max_size: Option<u64>,
    pub dry_run: bool,
}

pub struct PlayCommand {
    pub source: Source,
    /// Where the download goes, if the play starts one.
    pub to: Destination,
    /// Indexes into the release's file list.
    pub files: Vec<u32>,
    pub setup_max_size: Option<u64>,
    /// The title play this is part of. A play by hand has none.
    pub title: Option<TitlePlay>,
    pub finish: Finish,
}

/// A title play's test of one source.
pub struct TitlePlay {
    pub link: TitleLink,
    pub hold: Hold,
}

/// A title play's claim on the download it is testing. The play makes one for each source it
/// tries. `Command::Played` ends every hold on a download, which then stays. `Command::Release`
/// gives one up, and the owner alone decides what becomes of the download.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Hold(u64);

impl Hold {
    /// A hold no other play has.
    pub fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// What the caller does with a play's download once it starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Finish {
    /// Watches it as it arrives, so it downloads in playing order and takes no download slot.
    Stream,
    /// Keeps it, so it downloads as `get` downloads.
    Keep,
}

/// A selected file of a download, for the HTTP server.
#[derive(Clone, Debug)]
pub struct StreamFile {
    pub path: PathBuf,
    /// The release's name and the file's path inside it, which say what a video is.
    pub release: String,
    pub inside: String,
    pub size: u64,
    /// The download is done, so every byte is on disk.
    pub complete: bool,
}

/// A change to the library.
pub enum Shelving {
    Save(TitleCard),
    Unsave(TitleId),
}

/// What `rm` takes off the list: a download, or one already removed, which it answers for.
enum Removing {
    Download(DownloadId),
    Gone(Tombstone),
}

pub enum Command {
    Resolve {
        prefix: HexPrefix,
        results: Vec<Release>,
        command: &'static str,
        reply: Reply<Resolved>,
    },
    Get {
        get: Box<GetCommand>,
        reply: Reply<GetReply>,
    },
    Play {
        play: Box<PlayCommand>,
        reply: Reply<(Job, Arc<Layout>)>,
    },
    Played {
        id: DownloadId,
        reply: Reply<()>,
    },
    Release {
        hold: Hold,
        reply: Reply<()>,
    },
    /// The files of a download.
    Files {
        id: DownloadId,
        reply: Reply<FilesInfo>,
    },
    /// A fetched release to keep, and the search result whose link it came from.
    Cache {
        layout: Layout,
        from: Option<ResultId>,
    },
    Status {
        refs: Vec<String>,
        reply: Reply<Vec<Job>>,
    },
    Pause {
        refs: Vec<String>,
        reply: Reply<Vec<Change>>,
    },
    Resume {
        refs: Vec<String>,
        all: bool,
        reply: Reply<Vec<Change>>,
    },
    Remove {
        refs: Vec<String>,
        delete: bool,
        reply: Reply<Vec<Change>>,
    },
    Verify {
        reference: String,
        repair: bool,
        reply: Reply<Vec<Change>>,
    },
    Watch {
        refs: Vec<String>,
        reply: Reply<(Vec<Job>, broadcast::Receiver<Update>)>,
    },
    StreamFile {
        id: DownloadId,
        index: u32,
        reply: Reply<StreamFile>,
    },
    Stream {
        id: DownloadId,
        index: u32,
        reply: Reply<(StreamFile, Lease)>,
    },
    Library {
        shelving: Shelving,
        reply: Reply<()>,
    },
    RecordResults(Vec<(Release, Option<SavedLink>)>),
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    Restart {
        reply: Reply<()>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    Stopped,
    /// Stopped with every download saved, to start again from an installed update.
    Restart,
}

#[derive(Clone, Debug)]
pub enum Update {
    Job(Box<Job>),
    Removed(DownloadId),
}

/// A command waiting for a folder it looks in to answer.
struct Parked {
    dir: PathBuf,
    since: Instant,
    command: Command,
}

struct PendingPause {
    waiting: BTreeSet<DownloadId>,
    changes: Vec<Change>,
    deadline: Instant,
    reply: Reply<Vec<Change>>,
}

struct Stopping {
    waiting: BTreeSet<DownloadId>,
    deadline: Instant,
    replies: Vec<oneshot::Sender<()>>,
    /// An update's restart, which happens only if the stop saves everything and no one else
    /// asked the engine to stop meanwhile.
    restart: Option<Reply<()>>,
    /// A download's progress failed to save during the stop.
    unsaved: bool,
}

pub struct Owner {
    store: Store,
    shelf: Arc<Shelf>,
    /// The titles in the library. Every save goes through the owner, so this is the library.
    saved: BTreeSet<TitleId>,
    jobs: BTreeMap<DownloadId, JobEntry>,
    tombstones: Vec<Tombstone>,
    draining: BTreeSet<DownloadId>,
    /// Played downloads, which run in order and take no download slot until the engine exits.
    streamed: BTreeSet<DownloadId>,
    /// Who streams which download now, and how far ahead of each player its film is.
    streams: Streams,
    holds: Holds,
    pauses: Vec<PendingPause>,
    tending: Tending,
    stopping: Option<Stopping>,
    ended: Option<Ended>,
    engine: EngineHandle,
    parse: ParseLayout,
    /// The disk, and which folders on it answer now. The owner looks in a folder only through it.
    reach: Reach,
    parked: Vec<Parked>,
    updates: broadcast::Sender<Update>,
    config: OwnerConfig,
    resume_saved_at: Instant,
}

impl Owner {
    pub fn new(
        mut store: Store,
        shelf: Arc<Shelf>,
        engine: EngineHandle,
        parse: ParseLayout,
        disk: Arc<dyn Disk>,
        playing: Playing,
        config: OwnerConfig,
    ) -> Result<Self, Error> {
        // Opened first, for a plays folder other than the one used before changes what the plays are.
        let deleter = Deleter::open(&config.plays.root, config.trash.clone(), config.trash_pause.clone(), &mut store)?;
        let mut jobs = BTreeMap::new();
        for stored in store.jobs()? {
            let layout = Arc::new(parse(&stored.layout).map_err(|e| {
                Error::new(
                    ErrorCode::StateCorrupt,
                    format!("the stored file list of {} does not parse: {}", stored.record.id, e.message),
                )
            })?);
            jobs.insert(stored.record.id, JobEntry::new(stored.record, layout, Observed::Absent));
        }
        let now = now();
        let (updates, _) = broadcast::channel(256);
        let mut owner = Self {
            tombstones: store.tombstones(now)?,
            tending: Tending::new(deleter),
            store,
            saved: shelf.saved()?.into_iter().map(|card| card.id).collect(),
            shelf,
            jobs,
            draining: BTreeSet::new(),
            streamed: BTreeSet::new(),
            streams: Streams::new(playing),
            holds: Holds::default(),
            pauses: Vec::new(),
            stopping: None,
            ended: None,
            engine,
            parse,
            reach: Reach::new(disk, vec![config.plays.root.clone()]),
            parked: Vec::new(),
            updates,
            config,
            resume_saved_at: Instant::now(),
        };
        // A download in a folder that has not answered yet starts once it does.
        let folders: Vec<PathBuf> = owner.jobs.values().map(|job| job.record.dir.clone()).collect();
        owner.reach.wait(&folders, FOLDERS_AT_START);
        // Nothing is in the engine yet, so what a removal cut short by a crash left goes now.
        owner.finish_deletions();
        owner.schedule();
        Ok(owner)
    }

    pub async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<EngineEvent>,
    ) -> Ended {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            if let Some(ended) = self.ended {
                return ended;
            }
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command),
                    None => self.begin_stop(Stopper::Exit(None)),
                },
                event = events.recv() => match event {
                    Some(event) => self.engine_event(event),
                    None => {
                        log("the engine thread stopped, so the engine process is exiting");
                        self.ended = Some(Ended::Stopped);
                    }
                },
                _ = tick.tick() => {
                    self.catch_up(&mut events);
                    self.tick();
                }
            }
        }
    }

    /// Applies what the engine sent while the owner was busy, so it counts before a deadline is judged.
    fn catch_up(&mut self, events: &mut mpsc::UnboundedReceiver<EngineEvent>) {
        for _ in 0..events.len() {
            let Ok(event) = events.try_recv() else { break };
            self.engine_event(event);
        }
    }

    fn command(&mut self, command: Command) {
        self.run_or_park(command, Instant::now());
    }

    /// Runs `command` once every folder it looks in has answered, which it may have waited for
    /// since `since`.
    fn run_or_park(&mut self, command: Command, since: Instant) {
        let folders = self.folders(&command);
        let mut unreached = folders.into_iter().filter(|dir| !self.reach.reach(dir));
        if let Some(dir) = unreached.next() {
            self.parked.push(Parked { dir, since, command });
            return;
        }
        match command {
            Command::Resolve { prefix, results, command, reply } => {
                let _ = reply.send(self.resolve_any(&prefix, &results, command));
            }
            Command::Get { get, reply } => {
                let _ = reply.send(self.get_by_hand(*get));
            }
            Command::Play { play, reply } => {
                let hold = play.title.as_ref().map(|title| title.hold);
                // A play that is gone cannot give its source up, so the owner does.
                if reply.send(self.play(*play)).is_err()
                    && let Some(hold) = hold
                {
                    self.release(hold);
                }
            }
            Command::Played { id, reply } => {
                let _ = reply.send(self.played(id));
            }
            Command::Release { hold, reply } => {
                self.release(hold);
                let _ = reply.send(Ok(()));
            }
            Command::Files { id, reply } => {
                let _ = reply.send(self.job_files(id).ok_or_else(|| no_download(&id.to_hex())));
            }
            Command::Cache { layout, from } => {
                let id = layout.id;
                let short = &id.to_hex()[..6];
                persist(&format!("save the file list of {short}"), self.store.cache_layout(id, &layout.bytes, now()));
                if let Some(result) = from {
                    persist(
                        &format!("save which search result {short} came from"),
                        self.store.save_became(result, id, now()),
                    );
                }
            }
            Command::Status { refs, reply } => {
                let _ = reply.send(self.resolve_jobs(&refs, "status").map(|ids| self.views(&ids)));
            }
            Command::Pause { refs, reply } => match self.resolve_jobs(&refs, "pause") {
                Ok(ids) => {
                    if !refs.is_empty() {
                        self.adopt(&ids);
                    }
                    self.pause(ids, reply)
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Command::Resume { refs, all, reply } => {
                let ids = if all { Ok(None) } else { self.resolve_jobs(&refs, "resume").map(Some) };
                let resumed = ids.and_then(|ids| {
                    if !refs.is_empty() {
                        self.adopt(ids.as_deref().unwrap_or_default());
                    }
                    self.resume(ids.as_deref())
                });
                if let Ok(changes) = &resumed {
                    self.touch_plays(changes);
                }
                let _ = reply.send(resumed);
            }
            Command::Remove { refs, delete, reply } => match self.removals(&refs) {
                Ok(removing) => self.rm(removing, delete, reply),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Command::Verify { reference, repair, reply } => {
                let verified = self.resolve_job(&reference, "verify").and_then(|id| {
                    self.adopt(&[id]);
                    self.verify(id, repair)
                });
                let _ = reply.send(verified);
            }
            Command::Watch { refs, reply } => {
                let watch = self.resolve_jobs(&refs, "wait").map(|ids| (self.views(&ids), self.updates.subscribe()));
                let _ = reply.send(watch);
            }
            Command::StreamFile { id, index, reply } => {
                let _ = reply.send(self.stream_file(id, index));
            }
            Command::Stream { id, index, reply } => {
                let streaming = self.stream_file(id, index).and_then(|file| Ok((file, self.lease(id)?)));
                let _ = reply.send(streaming);
            }
            Command::Library { shelving, reply } => {
                let _ = reply.send(self.library(shelving));
            }
            Command::RecordResults(releases) => self.record_results(releases),
            Command::Shutdown { reply } => self.begin_stop(Stopper::Exit(Some(reply))),
            Command::Restart { reply } => self.begin_stop(Stopper::Update(reply)),
        }
    }

    /// The folders `command` looks in: where a download it makes goes, and where the downloads it
    /// names are.
    fn folders(&self, command: &Command) -> Vec<PathBuf> {
        let dir_of = |id: DownloadId| self.jobs.get(&id).map(|job| job.record.dir.clone());
        let existing = |source: &Source| match source {
            Source::Download(id) => Some(*id),
            Source::Release { layout, .. } => self.jobs.contains_key(&layout.id).then_some(layout.id),
        };
        let chosen = |dir: &Option<PathBuf>| dir.clone().unwrap_or_else(|| self.config.default_dir.clone());
        match command {
            Command::Get { get, .. } => {
                existing(&get.source).and_then(dir_of).into_iter().chain([chosen(&get.dir)]).collect()
            }
            Command::Play { play, .. } => {
                let to = match &play.to {
                    Destination::Folder(dir) => Some(chosen(dir)),
                    Destination::Plays => None,
                };
                existing(&play.source).and_then(dir_of).into_iter().chain(to).collect()
            }
            Command::Remove { refs, delete: true, .. } => {
                refs.iter().filter_map(|r| self.resolve_job(r, "rm").ok()).filter_map(dir_of).collect()
            }
            Command::Verify { reference, .. } => {
                self.resolve_job(reference, "verify").ok().and_then(dir_of).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Notes the folders that answered and runs the commands and the downloads that waited for
    /// them. A command that has waited `SAY_AFTER` for a folder answers that it is waiting.
    fn reached_folders(&mut self, now: Instant) {
        let answered = self.reach.catch_up(now);
        for parked in std::mem::take(&mut self.parked) {
            if self.reach.reached(&parked.dir) {
                self.run_or_park(parked.command, parked.since);
            } else if now.duration_since(parked.since) >= reach::SAY_AFTER {
                refuse(parked.command, reach::waiting(&parked.dir));
            } else {
                self.parked.push(parked);
            }
        }
        if answered {
            self.put_back_all();
            self.schedule();
        }
    }

    fn views(&self, ids: &[DownloadId]) -> Vec<Job> {
        ids.iter().filter_map(|id| self.jobs.get(id)).map(JobEntry::view).collect()
    }

    fn stream_file(&self, id: DownloadId, index: u32) -> Result<StreamFile, Error> {
        let found = self.jobs.get(&id).and_then(|job| {
            let layout = &job.layout;
            let file = layout.files.get(index as usize)?;
            job.wanted()?[index as usize].then(|| StreamFile {
                path: files::path(layout, file, &job.record.dir),
                release: layout.name.clone(),
                inside: file.path.clone(),
                size: file.size,
                complete: matches!(job.record.intent, Intent::Done { .. }),
            })
        });
        found.ok_or_else(|| Error::new(ErrorCode::NotFound, format!("no download has file {index} of {} selected", id)))
    }

    fn publish(&self, id: DownloadId) {
        if let Some(job) = self.jobs.get(&id) {
            let _ = self.updates.send(Update::Job(Box::new(job.view())));
        }
    }

    fn resolve_jobs(&self, refs: &[String], command: &'static str) -> Result<Vec<DownloadId>, Error> {
        if refs.is_empty() {
            let mut all: Vec<&JobEntry> = self.jobs.values().collect();
            all.sort_by_key(|j| (j.record.added_at, j.id()));
            return Ok(all.into_iter().map(JobEntry::id).collect());
        }
        let mut ids = Vec::new();
        for reference in refs {
            let id = self.resolve_job(reference, command)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    fn resolve_job(&self, reference: &str, command: &'static str) -> Result<DownloadId, Error> {
        match Target::parse(reference, Path::new("/"))? {
            Target::Prefix(prefix) => {
                let prefix: HexPrefix = prefix.parse()?;
                if prefix.is_result() {
                    return self.became_download(&prefix)?.ok_or_else(|| no_download(&format!("u{}", prefix.as_str())));
                }
                let jobs: Vec<(DownloadId, &str)> =
                    self.jobs.values().map(|j| (j.id(), j.record.name.as_str())).collect();
                match resolve::prefix(&prefix, &jobs, &[], Scope::Downloads, command)? {
                    Found::Job(id) => Ok(id),
                    Found::Release(_) => unreachable!("downloads scope finds only downloads"),
                }
            }
            Target::NzbUrl(_) | Target::NzbFile(_) => Err(Error::new(
                ErrorCode::InvalidRef,
                format!("{command} takes a download ID, not an .nzb file or URL"),
            )
            .with_hint("run newsreel status to list download IDs")
            .with_next("newsreel status")),
            Target::Title(video) => Err(newsreel_core::target::not_a_release(&video, command)),
        }
    }

    /// Adds `release` to the engine.
    fn add(&self, release: AddDownload) {
        let id = release.id;
        self.engine.send(id, move |e| e.add(release));
    }

    /// The one download the search results a prefix names became.
    fn became_download(&self, prefix: &HexPrefix) -> Result<Option<DownloadId>, Error> {
        let mut jobs = self.store.became(prefix.as_str())?.into_iter().filter(|id| self.jobs.contains_key(id));
        let Some(id) = jobs.next() else { return Ok(None) };
        Ok(jobs.all(|other| other == id).then_some(id))
    }

    fn resolve_any(&self, prefix: &HexPrefix, results: &[Release], command: &'static str) -> Result<Resolved, Error> {
        let jobs: Vec<(DownloadId, &str)> = self.jobs.values().map(|j| (j.id(), j.record.name.as_str())).collect();
        let found = match resolve::prefix(prefix, &jobs, results, Scope::DownloadsAndResults, command) {
            Err(e) if e.code == ErrorCode::NotFound => match self.became_download(prefix)? {
                Some(id) => return Ok(Resolved::Job(id)),
                None => return Err(e),
            },
            found => found?,
        };
        match found {
            Found::Job(id) => Ok(Resolved::Job(id)),
            Found::Release(release) => {
                let id = self.store.became(&release.id.to_hex())?.first().copied();
                if let Some(id) = id.filter(|id| self.jobs.contains_key(id)) {
                    return Ok(Resolved::Job(id));
                }
                let cached = match id {
                    Some(id) => self.cached(id)?,
                    None => None,
                };
                Ok(Resolved::Release { release: Box::new(release.clone()), cached })
            }
        }
    }

    /// The file list kept from an earlier fetch, if it is there and still parses.
    fn cached(&self, id: DownloadId) -> Result<Option<Arc<Layout>>, Error> {
        let Some(bytes) = self.store.layout(id)? else { return Ok(None) };
        Ok((self.parse)(&bytes).ok().map(Arc::new))
    }

    fn record_results(&mut self, saved: Vec<(Release, Option<SavedLink>)>) {
        persist("save the search results", self.store.record_results(&saved, now()));
    }

    /// What the running downloads other than `except` still need on the disk `device`. One in a
    /// folder that does not answer counts once it does.
    fn others_remaining(&self, device: u64, except: DownloadId) -> u64 {
        self.jobs
            .values()
            .filter(|j| j.id() != except && j.record.intent == Intent::Running && !self.leaving(j.id()))
            .filter_map(|job| {
                let (space, allocated) = self.reach.ask(&job.record.dir, measure(job)).ok()?;
                (space.ok()?.device == device).then(|| space_needed(job, allocated))
            })
            .sum()
    }

    /// What `layout`'s `selection` takes in `dir`, looked up there.
    fn plan(&self, layout: &Arc<Layout>, selection: &Selection, dir: &Path) -> Result<Plan, Error> {
        let (layout, selection, at) = (layout.clone(), selection.clone(), dir.to_path_buf());
        self.reach.ask(dir, move |disk| admission::plan(&layout, &selection, &at, disk))?
    }

    /// The disk `dir` is on, unless it does not answer.
    fn space(&self, dir: &Path) -> Option<Space> {
        let at = dir.to_path_buf();
        self.reach.ask(dir, move |disk| disk.space(&at)).ok()?.ok()
    }

    /// `get` as a person runs it, which makes a download under test theirs.
    fn get_by_hand(&mut self, get: GetCommand) -> Result<GetReply, Error> {
        let dry_run = get.dry_run;
        let reply = self.get(get, None, Home::Folder)?;
        if !dry_run {
            self.adopt(&[reply.job.id]);
            self.keep_play(reply.job.id)?;
        }
        Ok(reply)
    }

    /// Makes the downloads a person named in a command theirs. The plays testing one leave it as
    /// the person has it, whatever it was before. A command for every download adopts none.
    fn adopt(&mut self, ids: &[DownloadId]) {
        for &id in ids {
            self.holds.adopt(id);
        }
    }

    /// `title` links a download this creates to the title it is for, and `home` says where its files live.
    fn get(&mut self, get: GetCommand, title: Option<Linked>, home: Home) -> Result<GetReply, Error> {
        let GetCommand { source, dir, files, max_size, setup_max_size, dry_run } = get;
        if !files.is_empty() {
            crate::selection::compile(&files)?;
        }
        let selection = if files.is_empty() { Selection::All } else { Selection::Patterns(files) };
        let layout = match source {
            Source::Download(id) if self.jobs.contains_key(&id) => {
                return self.get_existing(id, selection, max_size, dry_run);
            }
            Source::Download(id) => return Err(no_download(&id.to_hex())),
            Source::Release { layout, .. } if self.jobs.contains_key(&layout.id) => {
                return self.get_existing(layout.id, selection, max_size, dry_run);
            }
            Source::Release { layout, .. } => Arc::new(layout),
        };
        let id = layout.id;
        let dir = dir.unwrap_or_else(|| self.config.default_dir.clone());
        if home == Home::Folder {
            let looked = dir.clone();
            self.tending.outside_plays(&dir, self.reach.ask(&dir, move |disk| disk.lineage(&looked))?)?;
        }
        let cap = Cap::new(max_size, setup_max_size);
        let plan = self.plan(&layout, &selection, &dir)?;
        admission::check_size(plan.selected_size, cap)?;
        if !dry_run {
            admission::check_conflicts(&plan)?;
            self.check_claims(id, &layout, &dir, &plan.wanted, None)?;
        }
        // A new play makes room in the plays folder only once nothing but the disk refuses it.
        // The plays the engine did not hold are gone by then, so the disk is measured again.
        let space = match home {
            Home::Plays(_) if !dry_run => {
                self.make_room_for(id, plan.selected_size);
                self.space(&dir).unwrap_or(plan.space)
            }
            _ => plan.space,
        };
        admission::check_disk(self.reclaimable(space), plan.needed(0), self.others_remaining(space.device, id))?;
        let record = JobRecord {
            id,
            name: layout.name.clone(),
            dir,
            selection,
            cap,
            added_at: now(),
            intent: Intent::Running,
            verified: 0,
            piece_map: None,
            files_verified: None,
            title,
            home,
        };
        if dry_run {
            let job = JobEntry::new(record, layout, Observed::Absent).view();
            return Ok(GetReply {
                outcome: GetOutcome::Planned,
                job,
                plan: Some(plan.public(cap)),
                previous_selection: None,
            });
        }
        self.store.insert(&record, &layout.bytes, record.added_at)?;
        self.tombstones.retain(|t| t.id != id);
        self.jobs.insert(id, JobEntry::new(record, layout, Observed::Absent));
        self.schedule();
        self.publish(id);
        let job = &self.jobs[&id];
        let outcome = if job.observed.in_engine() { GetOutcome::Started } else { GetOutcome::Queued };
        Ok(GetReply { outcome, job: job.view(), plan: Some(plan.public(cap)), previous_selection: None })
    }

    fn get_existing(
        &mut self,
        id: DownloadId,
        selection: Selection,
        max_size: Option<u64>,
        dry_run: bool,
    ) -> Result<GetReply, Error> {
        let cap = max_size.map_or(self.jobs[&id].record.cap, |bytes| Cap { bytes, from_setup: false });
        let job = &self.jobs[&id];
        let recheck =
            matches!(job.record.intent, Intent::Done { .. }) && job.verify.is_none() && self.lost_files(job)?;
        let unchanged = selection == Selection::All || selection == job.record.selection;
        if unchanged && cap == job.record.cap && !recheck {
            let plan = self.current_plan(job);
            return Ok(GetReply { outcome: GetOutcome::Existing, job: job.view(), plan, previous_selection: None });
        }
        let previous = (!unchanged).then(|| job.record.selection.clone());
        let selection = if unchanged { job.record.selection.clone() } else { selection };
        let layout = job.layout.clone();
        let plan = self.plan(&layout, &selection, &job.record.dir)?;
        admission::check_size(plan.selected_size, cap)?;
        if !dry_run {
            admission::check_conflicts(&plan)?;
            self.check_claims(id, &layout, &job.record.dir, &plan.wanted, None)?;
        }
        let needed = plan.needed(if recheck { 0 } else { job.verified() });
        // A running download gets the files at once, so only room the disk has now counts.
        admission::check_disk(plan.space, needed, self.others_remaining(plan.space.device, id))?;
        if dry_run {
            let mut preview = job.clone();
            preview.record.selection = selection;
            let job = preview.view();
            return Ok(GetReply {
                outcome: GetOutcome::Planned,
                job,
                plan: Some(plan.public(cap)),
                previous_selection: previous,
            });
        }
        let new_cap = cap != job.record.cap;
        if previous.is_some() {
            self.store.set_selection(id, &selection)?;
        }
        if new_cap {
            self.store.set_cap(id, cap)?;
        }
        if recheck {
            // The resume data still claims the lost files' pieces, so without dropping it the
            // engine would skip them.
            self.store.record_check(id, None, 0, None, &vec![0; layout.files.len()])?;
        }
        let reopen = recheck || (previous.is_some() && matches!(job.record.intent, Intent::Done { .. }));
        if reopen {
            self.store.set_intent(id, &Intent::Running)?;
        }
        let job = self.jobs.get_mut(&id).expect("found above");
        job.record.selection = selection;
        job.record.cap = cap;
        if recheck {
            job.record.verified = 0;
            job.record.piece_map = None;
            job.record.files_verified = Some(vec![0; layout.files.len()]);
            job.stats.verified = 0;
            job.stats.piece_map = None;
        }
        if reopen {
            job.record.intent = Intent::Running;
        }
        if previous.is_some() && job.observed.in_engine() {
            let wanted = plan.wanted.clone();
            self.engine.send(id, move |e| e.select(id, &wanted));
        }
        self.schedule();
        self.publish(id);
        let job = &self.jobs[&id];
        let outcome = match (&previous, recheck) {
            (Some(_), _) => GetOutcome::Updated,
            (None, true) if job.observed.in_engine() => GetOutcome::Started,
            (None, true) => GetOutcome::Queued,
            (None, false) => GetOutcome::Existing,
        };
        Ok(GetReply { outcome, job: job.view(), plan: Some(plan.public(cap)), previous_selection: previous })
    }

    /// Whether a selected file of the download is gone from disk or shorter than the release says.
    fn lost_files(&self, job: &JobEntry) -> Result<bool, Error> {
        let layout = job.layout.clone();
        let Ok(wanted) = files::wanted(&layout, &job.record.selection) else { return Ok(false) };
        let dir = job.record.dir.clone();
        self.reach.ask(&job.record.dir, move |disk| {
            layout
                .files
                .iter()
                .zip(wanted)
                .filter(|(_, wanted)| *wanted)
                .any(|(file, _)| disk.allocated_file_bytes(&files::path(&layout, file, &dir)) != Some(file.size))
        })
    }

    fn play(&mut self, play: PlayCommand) -> Result<(Job, Arc<Layout>), Error> {
        let PlayCommand { source, to, files, setup_max_size, title, finish } = play;
        let (id, layout) = match &source {
            Source::Download(id) => match self.jobs.get(id) {
                Some(job) => (*id, job.layout.clone()),
                None => return Err(no_download(&id.to_hex())),
            },
            Source::Release { layout, .. } => match self.jobs.get(&layout.id) {
                Some(job) => (layout.id, job.layout.clone()),
                None => (layout.id, Arc::new(layout.clone())),
            },
        };
        let job = self.jobs.get(&id);
        let before = match job.map(|job| &job.record.intent) {
            None => Before::Absent,
            Some(Intent::Paused) => Before::Paused,
            Some(Intent::Failed { error }) => Before::Failed(error.clone()),
            Some(Intent::Running | Intent::Done { .. }) => Before::Kept,
        };
        let short = id.to_hex()[..6].to_string();
        let see_files = |e: Error| {
            e.with_hint(format!("run newsreel files {short} first")).with_next(format!("newsreel files {short}"))
        };
        let count = layout.files.len();
        if let Some(missing) = files.iter().find(|&&i| i as usize >= count) {
            let message = format!("{short} has {count} files, so it has no file {missing}");
            return Err(see_files(Error::new(ErrorCode::Usage, message)));
        }
        if files.is_empty() {
            return Err(see_files(Error::new(ErrorCode::Usage, "play needs at least one file")));
        }
        let (mut patterns, wanted) = match job {
            None => (Vec::new(), vec![false; count]),
            Some(job) => match &job.record.selection {
                Selection::All => (Vec::new(), vec![true; count]),
                Selection::Patterns(chosen) => (chosen.clone(), job.wanted().unwrap_or_else(|| vec![false; count])),
            },
        };
        for &i in &files {
            let pattern = globset::escape(&newsreel_core::text::sanitize(&layout.files[i as usize].path));
            if !wanted[i as usize] && !patterns.contains(&pattern) {
                patterns.push(pattern);
            }
        }
        let absent = matches!(before, Before::Absent);
        let (dir, home) = if absent { self.destination(id, to)? } else { (None, Home::Folder) };
        let free = match &title {
            Some(_) if absent => {
                let into = dir.as_deref().unwrap_or(&self.config.default_dir);
                let footprint = files::footprint(&layout, into);
                self.reach.ask(into, move |disk| admission::check_free(disk, &footprint, id)).and_then(|free| free)
            }
            _ => Ok(()),
        };
        let made = title.as_ref().map(|play| Linked { link: play.link.clone(), played: false }).filter(|_| absent);
        let get =
            GetCommand { source, dir: dir.clone(), files: patterns, max_size: None, setup_max_size, dry_run: false };
        let id = match free.and_then(|()| self.get(get, made, home)) {
            Ok(reply) => reply.job.id,
            Err(error) => {
                self.unmade(dir.as_deref(), home);
                return Err(error);
            }
        };
        self.pressed_play(id, finish)?;
        match (&title, &self.jobs[&id].record.title) {
            // A download played by hand is being watched, so no title play puts it back.
            (None, _) => self.played(id)?,
            (Some(play), None) => {
                let link = play.link.clone();
                self.store.link(id, &link, now())?;
                self.jobs.get_mut(&id).expect("the play has it").record.title = Some(Linked { link, played: false });
            }
            (Some(_), Some(_)) => {}
        }
        let job = &self.jobs[&id];
        if matches!(job.record.intent, Intent::Paused | Intent::Failed { .. }) {
            self.resume(Some(&[id]))?;
        }
        if let Some(play) = title {
            self.holds.test(id, play.hold, before);
        }
        if finish == Finish::Stream {
            self.streamed.insert(id);
            if self.jobs[&id].observed.in_engine() {
                self.engine.send(id, move |e| e.stream(id));
            }
        }
        self.schedule();
        self.publish(id);
        Ok((self.jobs[&id].view(), layout))
    }

    /// Marks a download as having played. Every test of it is over, and it stays as it is. The
    /// plays keep their holds until that is on record, so one whose record fails can still give
    /// the download up.
    fn played(&mut self, id: DownloadId) -> Result<(), Error> {
        let job = self.jobs.get_mut(&id).ok_or_else(|| no_download(&id.to_hex()))?;
        if let Some(linked) = job.record.title.as_mut().filter(|linked| !linked.played) {
            self.store.played(id)?;
            linked.played = true;
        }
        self.holds.end(id);
        Ok(())
    }

    /// Ends a title play's test of a download that did not play. While another play still tests
    /// the download, nothing changes. The last play to give up puts the download back.
    fn release(&mut self, hold: Hold) {
        self.holds.release(hold);
        self.put_back_all();
        self.schedule();
    }

    /// Tries again whatever is still owed to the downloads no play tests.
    fn put_back_all(&mut self) {
        for id in self.holds.ids() {
            self.put_back(id);
        }
    }

    /// Does what `id` is owed once no play tests it. What fails or has to wait for the engine
    /// stays owed, and is tried again when the engine lets go of the download, at the next
    /// release, and when the engine stops.
    fn put_back(&mut self, id: DownloadId) {
        let owed = |owner: &Self| owner.holds.owed(id, owner.jobs.get(&id), |job| owner.on_disk(job));
        match owed(self) {
            None | Some(PutBack::Nothing | PutBack::Wait) => {}
            Some(PutBack::Pause) => self.pause(vec![id], oneshot::channel().0),
            Some(PutBack::Fail(error)) => self.fail(id, error),
            Some(PutBack::Remove) => {
                if let Err(error) = self.remove(vec![Removing::Download(id)], false) {
                    log(&format!("{id} stays, for it could not be removed: {}", error.message));
                }
            }
        }
        if matches!(owed(self), Some(PutBack::Nothing)) {
            self.holds.end(id);
        }
    }

    /// What `job` has on disk: bytes the engine verified, or anything at all where the download
    /// writes, whoever put it there. Nothing is known to be there only once the engine has
    /// stopped writing.
    fn on_disk(&self, job: &JobEntry) -> OnDisk {
        if job.verified() > 0 {
            return OnDisk::Something;
        }
        let settled = match job.observed {
            Observed::InEngine(phase) => phase == Phase::Paused,
            Observed::Absent => !self.draining.contains(&job.id()),
        };
        if !settled {
            return OnDisk::Unsettled;
        }
        let footprint = files::footprint(&job.layout, &job.record.dir);
        match self.reach.ask(&job.record.dir, move |disk| admission::taken(disk, &footprint).is_some()) {
            Ok(false) => OnDisk::Nothing,
            Ok(true) => OnDisk::Something,
            // What is in a folder that does not answer is not known yet.
            Err(_) => OnDisk::Unsettled,
        }
    }

    fn current_plan(&self, job: &JobEntry) -> Option<newsreel_core::protocol::DownloadPlan> {
        let plan = self.plan(&job.layout, &job.record.selection, &job.record.dir).ok()?;
        Some(plan.public(job.record.cap))
    }

    fn job_files(&self, id: DownloadId) -> Option<FilesInfo> {
        let job = self.jobs.get(&id)?;
        let layout = job.layout.clone();
        let wanted = job.wanted().unwrap_or_else(|| vec![false; layout.files.len()]);
        let verified = match (&job.record.intent, job.observed) {
            (Intent::Done { .. }, _) => {
                FileBytes::Known(layout.files.iter().zip(&wanted).map(|(f, w)| if *w { f.size } else { 0 }).collect())
            }
            (_, Observed::InEngine(_)) => {
                FileBytes::Engine { saved: job.record.files_verified.clone().unwrap_or_default() }
            }
            _ => FileBytes::Known(job.record.files_verified.clone().unwrap_or_default()),
        };
        let download = DownloadFiles { wanted, verified };
        Some(FilesInfo { id, layout, source: FileListSource::Download, download: Some(download) })
    }

    fn pause(&mut self, ids: Vec<DownloadId>, reply: Reply<Vec<Change>>) {
        let mut changes = Vec::new();
        let mut waiting = BTreeSet::new();
        for id in ids {
            let job = &self.jobs[&id];
            let result = match &job.record.intent {
                Intent::Paused => ChangeResult::AlreadyPaused,
                Intent::Done { .. } => ChangeResult::AlreadyDone,
                Intent::Failed { .. } => ChangeResult::AlreadyStopped,
                Intent::Running => {
                    if let Err(e) = self.store.set_intent(id, &Intent::Paused) {
                        let _ = reply.send(Err(e));
                        return;
                    }
                    let job = self.jobs.get_mut(&id).expect("resolved above");
                    job.record.intent = Intent::Paused;
                    match job.observed {
                        Observed::InEngine(Phase::Paused | Phase::Pausing) => {}
                        Observed::InEngine(_) => {
                            job.observed = Observed::InEngine(Phase::Pausing);
                            self.engine.send(id, move |e| e.pause(id));
                        }
                        Observed::Absent => {}
                    }
                    self.publish(id);
                    ChangeResult::Paused
                }
            };
            if self.jobs[&id].observed == Observed::InEngine(Phase::Pausing) {
                waiting.insert(id);
            }
            changes.push(self.change(id, result));
        }
        self.schedule();
        if waiting.is_empty() {
            let _ = reply.send(Ok(changes));
        } else {
            let deadline = Instant::now() + self.config.writes_stop_within;
            self.pauses.push(PendingPause { waiting, changes, deadline, reply });
        }
    }

    fn change(&self, id: DownloadId, result: ChangeResult) -> Change {
        Change::new(id.to_hex(), Some(self.jobs[&id].record.name.clone()), result)
    }

    /// Resumes `ids`, or with `None` every paused or stopped download that fits under its cap.
    fn resume(&mut self, ids: Option<&[DownloadId]>) -> Result<Vec<Change>, Error> {
        let resumable = |j: &JobEntry| matches!(j.record.intent, Intent::Paused | Intent::Failed { .. });
        let ids: Vec<DownloadId> = match ids {
            None => self.jobs.values().filter(|j| resumable(j)).map(JobEntry::id).collect(),
            Some(ids) => ids.to_vec(),
        };
        let mut changes = Vec::new();
        for id in ids {
            let job = &self.jobs[&id];
            let result = match (&job.record.intent, job.observed) {
                (Intent::Done { .. }, _) => ChangeResult::AlreadyDone,
                (Intent::Running, _) => ChangeResult::AlreadyRunning,
                (Intent::Paused | Intent::Failed { .. }, _) => {
                    self.store.set_intent(id, &Intent::Running)?;
                    self.jobs.get_mut(&id).expect("resolved above").record.intent = Intent::Running;
                    ChangeResult::Resumed
                }
            };
            changes.push(self.change(id, result));
        }
        self.schedule();
        for change in &changes {
            if let Ok(id) = change.id.parse() {
                self.publish(id);
            }
        }
        Ok(changes)
    }

    /// Takes downloads off the list. What of them to delete is put on record with each removal,
    /// and deleted once the engine lets go of it: a play's folder always, and the files of a
    /// download in a folder the person chose only with `delete`.
    /// What `refs` name for `rm`: a download, or one removed lately, which answers that it was.
    fn removals(&self, refs: &[String]) -> Result<Vec<Removing>, Error> {
        let mut removing = Vec::new();
        for reference in refs {
            match self.resolve_job(reference, "rm") {
                Ok(id) => removing.push(Removing::Download(id)),
                Err(e) if e.code == ErrorCode::NotFound => match self.tombstone(reference) {
                    Some(tomb) => removing.push(Removing::Gone(tomb.clone())),
                    None => return Err(e),
                },
                Err(e) => return Err(e),
            }
        }
        Ok(removing)
    }

    fn remove(&mut self, removing: Vec<Removing>, delete: bool) -> Result<Vec<Change>, Error> {
        let now = now();
        let mut changes = Vec::new();
        for target in removing {
            match target {
                Removing::Gone(tomb) => changes.push(Change {
                    path: tomb.path,
                    ..Change::new(tomb.id.to_hex(), tomb.name, ChangeResult::AlreadyRemoved)
                }),
                Removing::Download(id) if !self.jobs.contains_key(&id) => {}
                Removing::Download(id) => {
                    let job = &self.jobs[&id];
                    let view = job.view();
                    let name = Some(job.record.name.clone());
                    let tomb = Tombstone { id, name: name.clone(), path: Some(view.path.clone()), removed_at: now };
                    let doom = self.doom(job, delete);
                    self.tending.remove(&mut self.store, &tomb, doom, &self.reach)?;
                    let job = self.jobs.remove(&id).expect("resolved above");
                    self.streamed.remove(&id);
                    self.holds.end(id);
                    if job.observed.in_engine() {
                        self.engine.send(id, move |e| e.remove(id));
                        self.draining.insert(id);
                    }
                    if self.draining.contains(&id) {
                        self.tending.letting_go(id, Self::writes(&job));
                    }
                    self.pauses.iter_mut().for_each(|p| {
                        p.waiting.remove(&id);
                    });
                    if let Some(stopping) = &mut self.stopping {
                        stopping.waiting.remove(&id);
                    }
                    self.tombstones.retain(|t| t.id != id);
                    self.tombstones.push(tomb);
                    let _ = self.updates.send(Update::Removed(id));
                    changes.push(Change {
                        path: Some(view.path),
                        ..Change::new(id.to_hex(), Some(job.record.name), ChangeResult::Removed)
                    });
                }
            }
        }
        self.finish_pauses();
        self.schedule();
        Ok(changes)
    }

    fn tombstone(&self, reference: &str) -> Option<&Tombstone> {
        match Target::parse(reference, Path::new("/")).ok()? {
            Target::Prefix(p) => {
                let prefix: HexPrefix = p.parse().ok()?;
                let mut matching = self.tombstones.iter().filter(|t| t.id.has_prefix(&prefix));
                let first = matching.next()?;
                matching.next().is_none().then_some(first)
            }
            Target::NzbUrl(_) | Target::NzbFile(_) | Target::Title(_) => None,
        }
    }

    fn verify(&mut self, id: DownloadId, repair: bool) -> Result<Vec<Change>, Error> {
        let job = &self.jobs[&id];
        let short = &id.to_hex()[..6];
        let layout = job.layout.clone();
        if job.verify.is_some() || job.observed == Observed::InEngine(Phase::Checking) {
            return Ok(vec![self.change(id, ChangeResult::Checking)]);
        }
        if matches!(job.observed, Observed::InEngine(Phase::Pausing | Phase::Finishing { .. })) {
            return Err(Error::new(ErrorCode::JobConflict, format!("{short} is busy saving its state"))
                .with_hint(format!("try newsreel verify {short} again in a few seconds")));
        }
        let links = self.reach.ask(&job.record.dir, links(job))?;
        self.check_writes(id, &links, "verify")?;
        let job = &self.jobs[&id];
        let wanted = job.wanted().unwrap_or_else(|| vec![true; layout.files.len()]);
        let in_engine = job.observed.in_engine();
        let dir = job.record.dir.clone();
        if !in_engine {
            let resume_data = self.store.resume(id)?;
            let paused = AddDownload {
                id,
                manifest: layout.bytes.clone(),
                save_path: dir,
                wanted: Some(wanted),
                resume_data,
                paused: true,
            };
            self.add(paused);
        }
        self.engine.send(id, move |e| e.recheck(id));
        let job = self.jobs.get_mut(&id).expect("resolved above");
        job.verify = Some(if repair { Verify::Repair } else { Verify::Report });
        job.observed = Observed::InEngine(Phase::Checking);
        self.publish(id);
        Ok(vec![self.change(id, ChangeResult::Checking)])
    }

    fn engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Activity { id, activity } => {
                let Some(job) = self.jobs.get_mut(&id) else { return };
                if matches!(job.observed, Observed::InEngine(Phase::Checking | Phase::Downloading))
                    && job.verify.is_none()
                {
                    job.observed = Observed::InEngine(match activity {
                        Activity::Checking => Phase::Checking,
                        Activity::Downloading => Phase::Downloading,
                    });
                    self.publish(id);
                }
            }
            EngineEvent::Stats { id, stats } => {
                if let Some(job) = self.jobs.get_mut(&id) {
                    let now = Instant::now();
                    job.sample(stats, now);
                    if job.unsourced_for().is_some_and(|d| d >= self.config.stall_after) {
                        self.stop_unsourced(id);
                    } else {
                        self.publish(id);
                    }
                }
            }
            EngineEvent::Finished { id } => {
                let Some(job) = self.jobs.get_mut(&id) else { return };
                if job.record.intent == Intent::Running
                    && job.verify.is_none()
                    && matches!(job.observed, Observed::InEngine(Phase::Checking | Phase::Downloading))
                {
                    job.observed = Observed::InEngine(Phase::Finishing { writes_stopped: false });
                    self.engine.send(id, move |e| e.pause(id));
                }
            }
            EngineEvent::Paused { id } => self.paused(id),
            EngineEvent::ResumeData { id, data, verified, piece_map, files } => {
                self.resume_data(id, data, verified, piece_map, files)
            }
            EngineEvent::Checked { id, damaged_pieces, verified, piece_map, files } => {
                self.checked(id, damaged_pieces, verified, piece_map, files)
            }
            EngineEvent::Failed { id, error } => {
                if self.jobs.get(&id).is_some_and(|j| j.observed.in_engine()) {
                    self.fail(id, error);
                }
            }
            EngineEvent::Removed { id } => {
                self.draining.remove(&id);
                self.tending.let_go(id);
                self.leave();
                self.put_back(id);
                self.finish_deletions();
            }
        }
        self.finish_stop_if_done();
        self.schedule();
    }

    fn fail(&mut self, id: DownloadId, error: Error) {
        if let Some(stopping) = &mut self.stopping
            && stopping.waiting.contains(&id)
        {
            stopping.unsaved = true;
        }
        let Some(job) = self.jobs.get_mut(&id) else { return };
        job.verify = None;
        job.forget_sources();
        let saving = matches!(job.record.intent, Intent::Failed { .. })
            && matches!(job.observed, Observed::InEngine(Phase::Pausing | Phase::Paused));
        // The engine counts a piece once it arrives intact, even when its disk write fails.
        let disk = matches!(error.code, ErrorCode::DiskFull | ErrorCode::StorageError);
        if !saving {
            let intent = Intent::Failed { error };
            persist(&format!("save that {} stopped", &id.to_hex()[..6]), self.store.set_intent(id, &intent));
            job.record.intent = intent;
        }
        let save = !saving && !disk;
        job.observed = match job.observed {
            Observed::InEngine(Phase::Paused | Phase::Finishing { writes_stopped: true }) if save => {
                self.engine.send(id, move |e| e.save_resume_data(id));
                Observed::InEngine(Phase::Paused)
            }
            Observed::InEngine(Phase::Pausing | Phase::Finishing { writes_stopped: false }) if save => {
                Observed::InEngine(Phase::Pausing)
            }
            Observed::InEngine(Phase::Downloading) if save => {
                self.engine.send(id, move |e| e.pause(id));
                Observed::InEngine(Phase::Pausing)
            }
            Observed::InEngine(_) => {
                self.engine.send(id, move |e| e.remove(id));
                self.draining.insert(id);
                if let Some(stopping) = &mut self.stopping {
                    stopping.waiting.remove(&id);
                }
                for pause in &mut self.pauses {
                    pause.waiting.remove(&id);
                }
                Observed::Absent
            }
            Observed::Absent => Observed::Absent,
        };
        self.publish(id);
    }

    fn stop_unsourced(&mut self, id: DownloadId) {
        let job = &self.jobs[&id];
        let pieces = job.stats.missing.map_or(0, |m| m.pieces) as usize;
        let verified = job.verified();
        let missing = job.selected_size().saturating_sub(verified);
        let size = |bytes| newsreel_core::units::format_size(bytes);
        let error = Error::new(
            ErrorCode::NoSource,
            format!(
                "no server has sent the remaining {} ({}) for {}",
                newsreel_core::units::count(pieces, "piece"),
                size(missing),
                newsreel_core::units::elapsed(self.config.stall_after),
            ),
        )
        .with_hint(format!(
            "run newsreel resume {} to try again; the {} already verified is kept",
            &id.to_hex()[..6],
            size(verified)
        ))
        .with_next(format!("newsreel resume {}", &id.to_hex()[..6]));
        self.fail(id, error);
    }

    fn paused(&mut self, id: DownloadId) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        match job.observed {
            Observed::InEngine(Phase::Finishing { writes_stopped: false }) => {
                job.observed = Observed::InEngine(Phase::Finishing { writes_stopped: true });
            }
            Observed::InEngine(Phase::Finishing { writes_stopped: true }) => return,
            Observed::InEngine(Phase::Checking) if job.verify.is_some() => return,
            Observed::InEngine(_) => job.observed = Observed::InEngine(Phase::Paused),
            Observed::Absent => return,
        }
        self.engine.send(id, move |e| e.save_resume_data(id));
        for pause in &mut self.pauses {
            pause.waiting.remove(&id);
        }
        self.finish_pauses();
        self.publish(id);
        self.put_back(id);
    }

    fn finish_pauses(&mut self) {
        let now = Instant::now();
        let (done, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.pauses).into_iter().partition(|p| p.waiting.is_empty() || p.deadline <= now);
        self.pauses = rest;
        for pause in done {
            let reply =
                if pause.waiting.is_empty() { Ok(pause.changes) } else { Err(self.still_writing(&pause.waiting)) };
            let _ = pause.reply.send(reply);
        }
    }

    fn still_writing(&self, ids: &BTreeSet<DownloadId>) -> Error {
        let shorts: Vec<String> = ids.iter().map(|id| id.to_hex()[..6].to_string()).collect();
        let within = newsreel_core::units::elapsed(self.config.writes_stop_within);
        Error::new(ErrorCode::JobConflict, format!("writes to {} did not stop within {within}", shorts.join(", ")))
            .with_hint(format!("run newsreel pause {} again; it returns once writes stop", shorts.join(" ")))
            .with_next(format!("newsreel pause {}", shorts.join(" ")))
    }

    fn resume_data(
        &mut self,
        id: DownloadId,
        data: Vec<u8>,
        verified: Option<u64>,
        piece_map: Option<PieceMap>,
        files: Option<Vec<u64>>,
    ) {
        let job = self.jobs.get(&id);
        let last = job.is_none_or(|j| {
            matches!(j.observed, Observed::InEngine(Phase::Paused | Phase::Finishing { writes_stopped: true }))
        });
        if last && let Some(stopping) = &mut self.stopping {
            stopping.waiting.remove(&id);
        }
        let Some(job) = job else { return };
        let verified = verified.unwrap_or_else(|| job.verified());
        let piece_map = piece_map.or_else(|| job.piece_map());
        let files = files.or_else(|| job.record.files_verified.clone());
        let finishing = job.observed == Observed::InEngine(Phase::Finishing { writes_stopped: true });
        if finishing && job.selected_size() == verified {
            let finished_at = now();
            if let Err(error) = self.store.finish(id, finished_at, &data, verified) {
                if let Some(stopping) = &mut self.stopping {
                    stopping.unsaved = true;
                }
                self.fail(id, error);
                return;
            }
            let job = self.jobs.get_mut(&id).expect("checked above");
            job.record.intent = Intent::Done { finished_at };
            job.record.verified = verified;
            job.observed = Observed::Absent;
            self.engine.send(id, move |e| e.remove(id));
            self.draining.insert(id);
            self.publish(id);
            return;
        }
        let saving = self.store.save_resume(id, &data, verified, piece_map, files.as_deref(), now());
        let saved = persist(&format!("save the progress of {}", &id.to_hex()[..6]), saving);
        if !saved && let Some(stopping) = &mut self.stopping {
            stopping.unsaved = true;
        }
        let job = self.jobs.get_mut(&id).expect("checked above");
        if saved {
            job.record.verified = verified;
            job.record.piece_map = piece_map;
            job.record.files_verified = files;
        }
        if finishing {
            job.observed = Observed::InEngine(Phase::Paused);
        } else if job.held_for_save() {
            job.observed = Observed::Absent;
            job.forget_sources();
            self.engine.send(id, move |e| e.remove(id));
            self.draining.insert(id);
            self.publish(id);
        }
    }

    fn checked(&mut self, id: DownloadId, damaged: u32, verified: u64, piece_map: Option<PieceMap>, files: Vec<u64>) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let Some(mode) = job.verify.take() else { return };
        job.record.verified = verified;
        job.stats.verified = verified;
        job.record.piece_map = piece_map;
        job.stats.piece_map = piece_map;
        job.record.files_verified = Some(files);
        self.engine.send(id, move |e| e.save_resume_data(id));
        let short = id.to_hex()[..6].to_string();
        let intent = job.record.intent.clone();
        let files = job.record.files_verified.clone().unwrap_or_default();
        let record = |store: &mut Store, intent: Option<&Intent>| {
            persist(&format!("save the check of {short}"), store.record_check(id, intent, verified, piece_map, &files));
        };
        match (intent, damaged, mode) {
            (Intent::Running, n, _) => {
                if n > 0 {
                    record(&mut self.store, None);
                }
                job.observed = Observed::InEngine(Phase::Downloading);
                self.engine.send(id, move |e| e.resume(id));
            }
            (Intent::Paused, n, _) => {
                if n > 0 {
                    record(&mut self.store, None);
                }
                job.observed = Observed::InEngine(Phase::Paused);
            }
            (Intent::Done { .. } | Intent::Failed { .. }, 0, _) => {
                let finished_at = match job.record.intent {
                    Intent::Done { finished_at } => finished_at,
                    _ => now(),
                };
                let done = Intent::Done { finished_at };
                persist(&format!("save that {short} is done"), self.store.set_intent(id, &done));
                let job = self.jobs.get_mut(&id).expect("checked above");
                job.record.intent = done;
                job.observed = Observed::Absent;
                self.engine.send(id, move |e| e.remove(id));
                self.draining.insert(id);
            }
            (Intent::Done { .. } | Intent::Failed { .. }, _, Verify::Repair) => {
                record(&mut self.store, Some(&Intent::Running));
                job.record.intent = Intent::Running;
                job.observed = Observed::InEngine(Phase::Downloading);
                self.engine.send(id, move |e| e.resume(id));
            }
            (Intent::Failed { .. }, _, Verify::Report) => {
                record(&mut self.store, None);
                job.observed = Observed::InEngine(Phase::Paused);
            }
            (Intent::Done { .. }, n, Verify::Report) => {
                let error = Error::new(
                    ErrorCode::VerificationFailed,
                    format!("{} in the selected files", newsreel_core::units::count(n as usize, "damaged piece")),
                )
                .with_hint(format!("run newsreel verify {short} --repair to download them again"))
                .with_next(format!("newsreel verify {short} --repair"));
                let failed = Intent::Failed { error };
                record(&mut self.store, Some(&failed));
                job.record.intent = failed;
                job.observed = Observed::InEngine(Phase::Paused);
            }
        }
        self.publish(id);
    }

    fn schedule(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        let ready = self.jobs.values().filter(|j| !self.draining.contains(&j.id()) && !self.leaving(j.id()));
        for id in book::to_start(ready, self.config.max_active, &self.streamed) {
            self.start(id);
        }
    }

    fn start(&mut self, id: DownloadId) {
        let job = &self.jobs[&id];
        let (measured, linked) = (measure(job), links(job));
        // A download in a folder that does not answer starts once it does.
        let Ok(((space, allocated), links)) =
            self.reach.ask(&job.record.dir, move |disk| (measured(disk), linked(disk)))
        else {
            return;
        };
        self.jobs.get_mut(&id).expect("scheduled").forget_sources();
        if let Err(error) = self.check_writes(id, &links, "resume") {
            self.fail(id, error);
            return;
        }
        let job = &self.jobs[&id];
        if job.observed == Observed::InEngine(Phase::Paused) {
            self.engine.send(id, move |e| e.resume(id));
            self.jobs.get_mut(&id).expect("scheduled").observed = Observed::InEngine(Phase::Downloading);
            self.publish(id);
            return;
        }
        if let Ok(space) = space
            && let Err(error) =
                admission::check_disk(space, space_needed(job, allocated), self.others_remaining(space.device, id))
        {
            // Plays leaving the plays folder make room: it is checked again once they are gone.
            if !self.reclaiming(space) {
                self.fail(id, error);
            }
            return;
        }
        let resume_data = match self.store.resume(id) {
            Ok(data) => data,
            Err(error) => return self.fail(id, error),
        };
        let job = &self.jobs[&id];
        let running = AddDownload {
            id,
            manifest: job.layout.bytes.clone(),
            save_path: job.record.dir.clone(),
            wanted: job.wanted(),
            resume_data,
            paused: false,
        };
        self.add(running);
        if self.streamed.contains(&id) {
            self.engine.send(id, move |e| e.stream(id));
        }
        self.jobs.get_mut(&id).expect("scheduled").observed = Observed::InEngine(Phase::Checking);
        self.publish(id);
    }

    /// Refuses to write `id` through any of the `links` on the way to its files, or into files
    /// another download writes.
    fn check_writes(&self, id: DownloadId, links: &[PathBuf], command: &str) -> Result<(), Error> {
        let job = &self.jobs[&id];
        let Some(wanted) = job.wanted() else { return Ok(()) };
        let layout = &job.layout;
        let (dir, retry) = (&job.record.dir, format!("newsreel {command} {}", &id.to_hex()[..6]));
        admission::check_links(links, dir, Some(&retry))?;
        self.check_claims(id, layout, dir, &wanted, Some(&retry))
    }

    fn check_claims(
        &self,
        id: DownloadId,
        layout: &Layout,
        dir: &Path,
        wanted: &[bool],
        retry: Option<&str>,
    ) -> Result<(), Error> {
        let paths: BTreeMap<String, PathBuf> =
            files::targets(layout, dir, wanted).into_iter().map(|p| (claim_key(&p), p)).collect();
        for job in self.jobs.values().filter(|j| j.id() != id && !matches!(j.record.intent, Intent::Failed { .. })) {
            let Some(their_wanted) = job.wanted() else { continue };
            let theirs = &job.layout;
            let taken: Vec<PathBuf> = files::targets(theirs, &job.record.dir, &their_wanted)
                .iter()
                .filter_map(|p| paths.get(&claim_key(p)).cloned())
                .collect();
            if !taken.is_empty() {
                let destination = files::root(dir, &layout.name);
                return Err(admission::claimed(&destination, job.id(), &theirs.name, &taken, retry));
            }
        }
        Ok(())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.reached_folders(now);
        self.finish_pauses();
        self.finish_stop_if_done();
        self.save_progress(now);
        self.finish_deletions();
        self.leave();
        self.tend_plays(now);
        self.prioritize(now);
    }

    fn save_progress(&mut self, now: Instant) {
        if self.stopping.is_some() || now.duration_since(self.resume_saved_at) < self.config.save_resume_every {
            return;
        }
        self.resume_saved_at = now;
        for job in self.jobs.values() {
            if job.record.intent == Intent::Running && job.observed == Observed::InEngine(Phase::Downloading) {
                let id = job.id();
                self.engine.send(id, move |e| e.save_resume_data_if_progressed(id));
            }
        }
    }

    fn begin_stop(&mut self, by: Stopper) {
        if let Some(stopping) = &mut self.stopping {
            match by {
                Stopper::Exit(reply) => stopping.replies.extend(reply),
                Stopper::Update(reply) => {
                    let _ = reply.send(Err(Error::new(ErrorCode::DaemonUnavailable, "the engine is already stopping")));
                }
            }
            return;
        }
        self.put_back_all();
        let mut waiting = BTreeSet::new();
        for job in self.jobs.values_mut() {
            let id = job.record.id;
            match job.observed {
                Observed::InEngine(Phase::Paused) => {
                    self.engine.send(id, move |e| e.save_resume_data(id));
                    waiting.insert(id);
                }
                Observed::InEngine(Phase::Pausing | Phase::Finishing { .. }) => {
                    waiting.insert(id);
                }
                Observed::InEngine(_) => {
                    job.observed = Observed::InEngine(Phase::Pausing);
                    self.engine.send(id, move |e| e.pause(id));
                    waiting.insert(id);
                }
                Observed::Absent => {}
            }
        }
        let (replies, restart) = match by {
            Stopper::Exit(reply) => (reply.into_iter().collect(), None),
            Stopper::Update(reply) => (Vec::new(), Some(reply)),
        };
        self.stopping = Some(Stopping {
            waiting,
            deadline: Instant::now() + self.config.writes_stop_within,
            replies,
            restart,
            unsaved: false,
        });
        self.finish_stop_if_done();
    }

    fn finish_stop_if_done(&mut self) {
        let Some(stopping) = &self.stopping else { return };
        if !stopping.waiting.is_empty() && Instant::now() < stopping.deadline {
            return;
        }
        let stopping = self.stopping.take().expect("checked above");
        let saved = stopping.waiting.is_empty() && !stopping.unsaved;
        let ended = match stopping.restart {
            Some(restart) if stopping.replies.is_empty() && !saved => {
                let error =
                    Error::new(ErrorCode::StorageError, "the engine could not save the progress of every download");
                let _ = restart.send(Err(error));
                self.schedule();
                return;
            }
            Some(restart) if stopping.replies.is_empty() => {
                let _ = restart.send(Ok(()));
                Ended::Restart
            }
            Some(restart) => {
                let _ = restart.send(Err(Error::new(ErrorCode::DaemonUnavailable, "the engine is stopping")));
                Ended::Stopped
            }
            None => Ended::Stopped,
        };
        for reply in stopping.replies {
            let _ = reply.send(());
        }
        self.ended = Some(ended);
    }
}

enum Stopper {
    /// A person, a signal, or the idle timeout. The engine stops whatever the last saves did.
    Exit(Option<oneshot::Sender<()>>),
    /// An update, which restarts the engine only once every download's progress is saved.
    /// Otherwise the engine carries on and the update tries again later.
    Update(Reply<()>),
}

/// Looks up what `job`'s selected files take up on disk already.
fn allocated(job: &JobEntry) -> impl FnOnce(&dyn Disk) -> u64 + Send + 'static {
    let (dir, layout, wanted) = (job.record.dir.clone(), job.layout.clone(), job.wanted());
    move |disk| wanted.map_or(0, |wanted| admission::allocated_bytes(&layout, &dir, &wanted, disk))
}

/// Looks up the disk `job` downloads to, and what its selected files take up on it already.
fn measure(job: &JobEntry) -> impl FnOnce(&dyn Disk) -> (std::io::Result<Space>, u64) + Send + 'static {
    let (dir, allocated) = (job.record.dir.clone(), allocated(job));
    move |disk| (disk.space(&dir), allocated(disk))
}

/// Looks up the links on the way to `job`'s selected files, which the engine would write through.
fn links(job: &JobEntry) -> impl FnOnce(&dyn Disk) -> Vec<PathBuf> + Send + 'static {
    let (dir, layout, wanted) = (job.record.dir.clone(), job.layout.clone(), job.wanted());
    move |disk| wanted.map_or_else(Vec::new, |wanted| admission::links(&layout, &dir, &wanted, disk))
}

/// What `job` still needs on its disk, with `allocated` bytes of its selected files there already.
fn space_needed(job: &JobEntry, allocated: u64) -> u64 {
    match job.wanted() {
        Some(_) => job.selected_size().saturating_sub(allocated.max(job.verified())),
        None => job.remaining(),
    }
}

/// Answers a command that did not run with `error`.
fn refuse(command: Command, error: Error) {
    match command {
        Command::Get { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        Command::Play { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        Command::Remove { reply, .. } | Command::Verify { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        _ => {}
    }
}

// Default macOS filesystems ignore case.
fn claim_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}
