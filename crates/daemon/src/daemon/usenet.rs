//! The Usenet engine. A job is a resolved release ([`Manifest`]); its pieces are the parts of
//! articles that land in its files. Workers fetch pieces in file order, and the pieces under a
//! byte range a player asks for first, then continue from there, as streaming needs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use newsreel_core::{DownloadId, Error, ErrorCode, PieceMap, Slice};
use usenet::manifest::Piece;
use usenet::{Manifest, Nzb, Pool, ResolveError, article};

use super::BoxFuture;
use super::engine_port::{self, Activity, AddDownload, EngineEvent, EnginePort, Missing, Stats};
use super::journal::Journal;
use super::layout::{Layout, LayoutFile};
use super::nzb_port::NzbPort;
use crate::log::log;

const STATS_EVERY: Duration = Duration::from_secs(1);
/// After a transient failure, how long a worker waits before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(2);
/// Pieces past the last requested range fetched before returning to file order.
const READAHEAD: usize = 64;
/// The longest the engine thread waits for events, which is also how long a command can wait.
const POLL_WAIT: Duration = Duration::from_millis(10);
/// How often a worker held back by a rate limit looks again, so lifting the limit frees it soon.
const PACE_CHECK: Duration = Duration::from_millis(200);

/// Reads a manifest: the files a download writes, and where each article lands in them.
pub fn parse(bytes: &[u8]) -> Result<Layout, Error> {
    let manifest = Manifest::from_bytes(bytes).map_err(|e| Error::new(ErrorCode::InvalidRelease, e.to_string()))?;
    layout(&manifest, bytes.to_vec())
}

/// The download a manifest names: its release ID, a hash of the NZB's segments.
fn download_id(manifest: &Manifest) -> Result<DownloadId, Error> {
    manifest.release_id.parse().map_err(|_| {
        Error::new(
            ErrorCode::InvalidRelease,
            format!("the manifest names its release by {:?}, not 40 hex characters", manifest.release_id),
        )
        .with_hint("read the NZB again with newsreel get <nzb file or URL>")
    })
}

fn folder_name(title: &str) -> String {
    let name: String = title.chars().map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
    match name.trim() {
        "" | "." | ".." => "release".into(),
        trimmed => trimmed.to_string(),
    }
}

fn layout(manifest: &Manifest, bytes: Vec<u8>) -> Result<Layout, Error> {
    let id = download_id(manifest)?;
    let files: Vec<LayoutFile> =
        manifest.files.iter().map(|f| LayoutFile { path: f.path.clone(), size: f.size }).collect();
    match files.as_slice() {
        [only] if !only.path.contains('/') => Layout::new(id, only.path.clone(), files, true, bytes),
        _ => Layout::new(id, folder_name(&manifest.title), files, false, bytes),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Run {
    Paused,
    Running,
    Finished,
    Stopped,
}

type PieceRef = (usize, usize);

struct Job {
    id: DownloadId,
    files: Vec<OutputFile>,
    state: Mutex<State>,
    journal: Journal,
}

struct OutputFile {
    path: PathBuf,
    size: u64,
    pieces: Vec<Piece>,
    handle: Mutex<Option<File>>,
}

struct State {
    run: Run,
    /// Bumped on every pause, stop, and selection change, so workers from before exit.
    generation: u64,
    wanted: Vec<bool>,
    /// CRC32 of each piece's landed bytes, once it is on disk.
    have: Vec<Vec<Option<u32>>>,
    /// CRC32 an earlier run journaled for a piece not in `have`: read back before fetching.
    journaled: Vec<Vec<Option<u32>>>,
    in_flight: BTreeSet<PieceRef>,
    /// Missing on every server.
    failed: BTreeSet<PieceRef>,
    urgent: VecDeque<PieceRef>,
    /// Where the last requested range ended; readahead continues from here.
    focus: Option<PieceRef>,
    workers: usize,
    landed: u64,
    last_error: Option<String>,
    announced_downloading: bool,
    /// Bytes per second the download may take, when the owner caps it.
    limit: Option<NonZeroU32>,
    /// Under a limit, when the bytes already granted have had their time.
    paced_until: Option<Instant>,
}

impl State {
    /// When a fetch of `bytes` may start under the limit, granting it the time it takes.
    /// `None` without a limit.
    fn pace(&mut self, bytes: u64) -> Option<Instant> {
        let rate = f64::from(self.limit?.get());
        let now = Instant::now();
        let start = self.paced_until.map_or(now, |until| until.max(now));
        self.paced_until = Some(start + Duration::from_secs_f64(bytes as f64 / rate));
        Some(start)
    }

    fn usable(&self, (f, p): PieceRef) -> bool {
        self.wanted.get(f) == Some(&true)
            && self.have[f].get(p) == Some(&None)
            && !self.in_flight.contains(&(f, p))
            && !self.failed.contains(&(f, p))
    }

    fn pick(&mut self, files: &[OutputFile]) -> Option<PieceRef> {
        while let Some(next) = self.urgent.pop_front() {
            if self.usable(next) {
                return Some(next);
            }
        }
        if let Some((f, start)) = self.focus {
            let ahead = (start..files[f].pieces.len()).take(READAHEAD).map(|p| (f, p)).find(|r| self.usable(*r));
            if ahead.is_some() {
                return ahead;
            }
        }
        (0..files.len()).flat_map(|f| (0..files[f].pieces.len()).map(move |p| (f, p))).find(|r| self.usable(*r))
    }

    fn remaining(&self) -> (u32, u32) {
        let mut pieces = 0;
        for (f, have) in self.have.iter().enumerate() {
            if self.wanted[f] {
                pieces += have.iter().filter(|h| h.is_none()).count() as u32;
            }
        }
        (pieces, self.failed.iter().filter(|(f, _)| self.wanted[*f]).count() as u32)
    }
}

impl Job {
    fn verified(&self, state: &State) -> (u64, Vec<u64>) {
        let files: Vec<u64> = self
            .files
            .iter()
            .zip(&state.have)
            .map(|(f, have)| f.pieces.iter().zip(have).filter(|(_, h)| h.is_some()).map(|(p, _)| p.length).sum())
            .collect();
        let verified = files.iter().zip(&state.wanted).filter(|(_, w)| **w).map(|(b, _)| b).sum();
        (verified, files)
    }

    fn piece_map(&self, state: &State) -> Option<PieceMap> {
        let all: Vec<(bool, bool)> = state
            .have
            .iter()
            .enumerate()
            .flat_map(|(f, have)| have.iter().map(move |h| (state.wanted[f], h.is_some())))
            .collect();
        if all.is_empty() {
            return None;
        }
        Some(PieceMap::new(std::array::from_fn(|s| {
            let begin = s * all.len() / PieceMap::SLICES;
            let end = (begin + 1).max((s + 1) * all.len() / PieceMap::SLICES).min(all.len());
            let run = &all[begin.min(all.len() - 1)..end];
            let wanted = run.iter().filter(|(w, _)| *w).count() as u32;
            Slice::of(run.iter().filter(|(w, h)| *w && *h).count() as u32, wanted)
        })))
    }

    /// `open` is how many connections the pool holds, which a download's requests share.
    fn stats(&self, state: &State, down_rate: u64, open: u32) -> Stats {
        let (verified, _) = self.verified(state);
        let (pieces, unavailable) = state.remaining();
        Stats {
            verified,
            piece_map: self.piece_map(state),
            missing: Some(Missing { pieces, unavailable }),
            down_rate,
            // Each connection carries several requests at once, so requests overcount them.
            connections: (state.in_flight.len() as u32).min(open),
        }
    }

    /// Pieces of `file` under `[offset, offset + length)`.
    fn covering(&self, file: usize, offset: u64, length: u64) -> std::ops::Range<usize> {
        let pieces = &self.files[file].pieces;
        let end = offset.saturating_add(length.max(1));
        let first = pieces.partition_point(|p| p.offset + p.length <= offset);
        let last = pieces.partition_point(|p| p.offset < end);
        first..last.max(first)
    }

    fn write(&self, (f, p): PieceRef, bytes: &[u8]) -> std::io::Result<()> {
        let file = &self.files[f];
        let mut handle = file.handle.lock().unwrap();
        if handle.is_none() {
            if let Some(dir) = file.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let opened = OpenOptions::new().create(true).write(true).read(true).truncate(false).open(&file.path)?;
            if opened.metadata()?.len() != file.size {
                opened.set_len(file.size)?;
            }
            *handle = Some(opened);
        }
        let opened = handle.as_ref().expect("opened above");
        write_at(opened, bytes, file.pieces[p].offset)
    }

    /// The CRC of a piece's bytes on disk. None when they are not there: the file is missing
    /// or too short. An error when the file cannot be read, which says nothing of the piece.
    fn read_back(&self, (f, p): PieceRef) -> Result<Option<u32>, Error> {
        use std::io::ErrorKind::{NotFound, UnexpectedEof};
        let file = &self.files[f];
        let piece = &file.pieces[p];
        let unreadable = |e: std::io::Error| {
            Error::new(ErrorCode::StorageError, format!("cannot read {}: {e}", file.path.display()))
                .with_hint("check the file's permissions, then run newsreel verify again")
        };
        let opened = match File::open(&file.path) {
            Ok(opened) => opened,
            Err(e) if e.kind() == NotFound => return Ok(None),
            Err(e) => return Err(unreadable(e)),
        };
        let mut bytes = vec![0; piece.length as usize];
        match read_at(&opened, &mut bytes, piece.offset) {
            Ok(()) => Ok(Some(crc32fast::hash(&bytes))),
            Err(e) if e.kind() == UnexpectedEof => Ok(None),
            Err(e) => Err(unreadable(e)),
        }
    }
}

#[cfg(unix)]
fn write_at(file: &File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, offset)
}

#[cfg(unix)]
fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, bytes, offset)
}

/// Windows writes and reads at an offset with no "all" form, so these loop until done.
#[cfg(windows)]
fn write_at(file: &File, mut bytes: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match std::os::windows::fs::FileExt::seek_write(file, bytes, offset)? {
            0 => return Err(std::io::ErrorKind::WriteZero.into()),
            n => {
                bytes = &bytes[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, bytes, offset)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => {
                bytes = &mut std::mem::take(&mut bytes)[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

pub struct Usenet {
    runtime: tokio::runtime::Handle,
    pool: Pool,
    /// Where each release's [`Journal`] is kept.
    journals: PathBuf,
    jobs: BTreeMap<DownloadId, Arc<Job>>,
    sender: mpsc::Sender<EngineEvent>,
    events: mpsc::Receiver<EngineEvent>,
    queued: Vec<EngineEvent>,
    stats_at: Instant,
    landed_at_last_stats: BTreeMap<DownloadId, u64>,
}

impl Usenet {
    pub fn new(pool: Pool, runtime: tokio::runtime::Handle, journals: PathBuf) -> Self {
        let (sender, events) = mpsc::channel();
        super::journal::prune(&journals, super::journal::KEEP);
        Self {
            runtime,
            pool,
            journals,
            jobs: BTreeMap::new(),
            sender,
            events,
            queued: Vec::new(),
            stats_at: Instant::now(),
            landed_at_last_stats: BTreeMap::new(),
        }
    }

    fn job(&self, id: DownloadId) -> Result<Arc<Job>, Error> {
        self.jobs.get(&id).cloned().ok_or_else(|| Error::internal(format!("the engine has no download {id}")))
    }

    fn start_workers(&self, job: &Arc<Job>) {
        let mut state = job.state.lock().unwrap();
        if state.run != Run::Running {
            return;
        }
        let generation = state.generation;
        let wanted = self.pool.capacity().max(1);
        while state.workers < wanted {
            state.workers += 1;
            let (job, pool, events) = (job.clone(), self.pool.clone(), self.sender.clone());
            self.runtime.spawn(worker(job, pool, events, generation));
        }
    }

    fn stats(&mut self) {
        let elapsed = self.stats_at.elapsed().as_secs_f64().max(0.001);
        let open = self.pool.stats().connections;
        self.stats_at = Instant::now();
        for (id, job) in &self.jobs {
            let state = job.state.lock().unwrap();
            if state.run != Run::Running {
                continue;
            }
            let before = self.landed_at_last_stats.insert(*id, state.landed).unwrap_or(state.landed);
            let rate = ((state.landed - before) as f64 / elapsed) as u64;
            self.queued.push(EngineEvent::Stats { id: *id, stats: job.stats(&state, rate, open) });
        }
    }
}

/// Fetches pieces until none is left for this worker, then reports a finish or a failure
/// if it was the last worker out.
async fn worker(job: Arc<Job>, pool: Pool, events: mpsc::Sender<EngineEvent>, generation: u64) {
    loop {
        let (next, journaled) = {
            let mut state = job.state.lock().unwrap();
            let next =
                if state.generation == generation && state.run == Run::Running { state.pick(&job.files) } else { None };
            match next {
                Some(r) => {
                    state.in_flight.insert(r);
                    if !state.announced_downloading {
                        state.announced_downloading = true;
                        let _ = events.send(EngineEvent::Activity { id: job.id, activity: Activity::Downloading });
                    }
                    (r, state.journaled[r.0][r.1].take())
                }
                None => {
                    // Workers of an earlier generation were already uncounted when it changed.
                    if state.generation == generation {
                        state.workers -= 1;
                        if state.workers == 0 && state.run == Run::Running {
                            finish(&job, &mut state, &events);
                        }
                    }
                    return;
                }
            }
        };
        if let Some(crc) = journaled {
            let reader = job.clone();
            let found = tokio::task::spawn_blocking(move || reader.read_back(next)).await;
            if matches!(found, Ok(Ok(Some(c))) if c == crc) {
                let mut state = job.state.lock().unwrap();
                state.in_flight.remove(&next);
                if state.generation == generation {
                    state.have[next.0][next.1] = Some(crc);
                }
                continue;
            }
        }
        let (f, p) = next;
        let piece = &job.files[f].pieces[p];
        let start = job.state.lock().unwrap().pace(piece.length);
        if let Some(start) = start {
            // Look again now and then: a lifted limit or a pause frees the worker at once.
            loop {
                let now = Instant::now();
                let held = {
                    let state = job.state.lock().unwrap();
                    state.limit.is_some() && state.generation == generation && now < start
                };
                if !held {
                    break;
                }
                tokio::time::sleep((start - now).min(PACE_CHECK)).await;
            }
        }
        let fetched = article::fetch(&pool, &piece.ids).await;
        let landed = match fetched {
            Ok(part) => {
                let (from, to) = (piece.skip as usize, (piece.skip + piece.length) as usize);
                match part.data.get(from..to).filter(|_| part.begin == piece.begin) {
                    Some(bytes) => Ok(bytes.to_vec()),
                    None => Err(Some(format!(
                        "article <{}> holds bytes {}..{} of its file, not the {} bytes from {} the NZB places there",
                        piece.ids[0],
                        part.begin,
                        part.end(),
                        piece.length,
                        piece.begin + piece.skip
                    ))),
                }
            }
            Err(e) if e.is_missing() => Err(None),
            Err(e) => {
                {
                    let mut state = job.state.lock().unwrap();
                    state.in_flight.remove(&next);
                    state.last_error = Some(e.to_string());
                }
                tokio::time::sleep(RETRY_AFTER).await;
                continue;
            }
        };
        let mut state = job.state.lock().unwrap();
        state.in_flight.remove(&next);
        if state.generation != generation {
            continue;
        }
        match landed {
            Ok(bytes) => {
                if let Err(e) = job.write(next, &bytes) {
                    state.run = Run::Stopped;
                    state.generation += 1;
                    let path = job.files[f].path.display();
                    let error = Error::new(ErrorCode::StorageError, format!("cannot write {path}: {e}"))
                        .with_hint("check free space and permissions in the downloads folder");
                    let _ = events.send(EngineEvent::Failed { id: job.id, error });
                    continue;
                }
                let crc = crc32fast::hash(&bytes);
                state.have[f][p] = Some(crc);
                state.landed += bytes.len() as u64;
                job.journal.record(next, crc);
            }
            Err(corrupt) => {
                if let Some(why) = corrupt {
                    state.last_error = Some(why);
                }
                state.failed.insert(next);
            }
        }
    }
}

fn finish(job: &Job, state: &mut State, events: &mpsc::Sender<EngineEvent>) {
    let (pieces, unavailable) = state.remaining();
    if pieces == 0 {
        state.run = Run::Finished;
        let _ = events.send(EngineEvent::Stats { id: job.id, stats: job.stats(state, 0, 0) });
        let _ = events.send(EngineEvent::Finished { id: job.id });
    } else if unavailable == pieces {
        state.run = Run::Stopped;
        let error = Error::new(
            ErrorCode::NoSource,
            format!("{unavailable} articles are missing on every server, so the download cannot finish"),
        )
        .with_hint("the release has expired or been taken down; search again and pick another release");
        let _ = events.send(EngineEvent::Stats { id: job.id, stats: job.stats(state, 0, 0) });
        let _ = events.send(EngineEvent::Failed { id: job.id, error });
    }
}

/// Resume data: per piece, a flag byte and the CRC32 of its landed bytes.
fn encode_resume(state: &State) -> Vec<u8> {
    let mut out = b"TU1".to_vec();
    for have in state.have.iter().flatten() {
        out.push(u8::from(have.is_some()));
        out.extend(have.unwrap_or(0).to_le_bytes());
    }
    out
}

fn decode_resume(bytes: &[u8], have: &mut [Vec<Option<u32>>]) -> bool {
    let Some(body) = bytes.strip_prefix(b"TU1") else { return false };
    let total: usize = have.iter().map(Vec::len).sum();
    if body.len() != total * 5 {
        return false;
    }
    for (slot, record) in have.iter_mut().flatten().zip(body.chunks(5)) {
        *slot = (record[0] == 1).then(|| u32::from_le_bytes([record[1], record[2], record[3], record[4]]));
    }
    true
}

impl EnginePort for Usenet {
    fn add(&mut self, release: AddDownload) -> Result<(), Error> {
        let AddDownload { id, manifest: bytes, save_path, wanted, resume_data, paused } = release;
        if self.jobs.contains_key(&id) {
            return Err(Error::internal(format!("the engine already has download {id}")));
        }
        let manifest =
            Manifest::from_bytes(&bytes).map_err(|e| Error::new(ErrorCode::InvalidRelease, e.to_string()))?;
        let meta = layout(&manifest, Vec::new())?;
        let files: Vec<OutputFile> = manifest
            .files
            .into_iter()
            .zip(&meta.files)
            .map(|(f, m)| OutputFile {
                path: save_path.join(super::files::relative(&meta, m)),
                size: f.size,
                pieces: f.pieces,
                handle: Mutex::new(None),
            })
            .collect();
        let mut have: Vec<Vec<Option<u32>>> = files.iter().map(|f| vec![None; f.pieces.len()]).collect();
        if let Some(data) = resume_data
            && !decode_resume(&data, &mut have)
        {
            log(&format!("{id} has unreadable resume data, so it downloads again"));
        }
        let journal = Journal::new(&self.journals, id);
        let mut journaled = journal.read(&files.iter().map(|f| f.pieces.len()).collect::<Vec<_>>());
        for (slot, known) in journaled.iter_mut().flatten().zip(have.iter().flatten()) {
            if known.is_some() {
                *slot = None;
            }
        }
        let wanted = wanted.unwrap_or_else(|| vec![true; files.len()]);
        let state = State {
            run: if paused { Run::Paused } else { Run::Running },
            generation: 0,
            wanted,
            have,
            journaled,
            in_flight: BTreeSet::new(),
            failed: BTreeSet::new(),
            urgent: VecDeque::new(),
            focus: None,
            workers: 0,
            landed: 0,
            last_error: None,
            announced_downloading: false,
            limit: None,
            paced_until: None,
        };
        let job = Arc::new(Job { id, files, state: Mutex::new(state), journal });
        self.queued.push(EngineEvent::Activity { id, activity: Activity::Checking });
        self.jobs.insert(id, job.clone());
        self.start_workers(&job);
        Ok(())
    }

    fn select(&mut self, id: DownloadId, wanted: &[bool]) -> Result<(), Error> {
        let job = self.job(id)?;
        {
            let mut state = job.state.lock().unwrap();
            state.wanted = wanted.to_vec();
            state.generation += 1;
            state.workers = 0;
            state.announced_downloading = false;
            if state.run == Run::Finished {
                state.run = Run::Running;
            }
        }
        self.start_workers(&job);
        Ok(())
    }

    fn pause(&mut self, id: DownloadId) -> Result<(), Error> {
        let job = self.job(id)?;
        let mut state = job.state.lock().unwrap();
        if state.run == Run::Running {
            state.run = Run::Paused;
            state.generation += 1;
            state.workers = 0;
            state.announced_downloading = false;
        }
        self.queued.push(EngineEvent::Stats { id, stats: job.stats(&state, 0, 0) });
        self.queued.push(EngineEvent::Paused { id });
        Ok(())
    }

    fn resume(&mut self, id: DownloadId) -> Result<(), Error> {
        let job = self.job(id)?;
        {
            let mut state = job.state.lock().unwrap();
            if matches!(state.run, Run::Paused | Run::Stopped) {
                state.run = Run::Running;
                state.generation += 1;
                state.workers = 0;
                state.failed.clear();
            }
        }
        self.start_workers(&job);
        Ok(())
    }

    fn recheck(&mut self, id: DownloadId) -> Result<(), Error> {
        let job = self.job(id)?;
        self.queued.push(EngineEvent::Activity { id, activity: Activity::Checking });
        let events = self.sender.clone();
        self.runtime.spawn_blocking(move || {
            let landed: Vec<(PieceRef, u32)> = {
                let state = job.state.lock().unwrap();
                state
                    .have
                    .iter()
                    .enumerate()
                    .flat_map(|(f, have)| have.iter().enumerate().filter_map(move |(p, h)| Some(((f, p), (*h)?))))
                    .collect()
            };
            let mut damaged: Vec<PieceRef> = Vec::new();
            for (piece, crc) in landed {
                match job.read_back(piece) {
                    Ok(found) if found == Some(crc) => {}
                    Ok(_) => damaged.push(piece),
                    // A check that cannot read takes nothing back.
                    Err(error) => {
                        let _ = events.send(EngineEvent::Failed { id, error });
                        return;
                    }
                }
            }
            let mut state = job.state.lock().unwrap();
            for (f, p) in &damaged {
                state.have[*f][*p] = None;
            }
            let (verified, files) = job.verified(&state);
            // Damaged means every selected piece not on disk intact,
            // including ones that never arrived or failed an earlier check.
            let (missing, _) = state.remaining();
            let _ = events.send(EngineEvent::Checked {
                id,
                damaged_pieces: missing,
                verified,
                piece_map: job.piece_map(&state),
                files,
            });
        });
        Ok(())
    }

    fn save_resume_data(&mut self, id: DownloadId) -> Result<(), Error> {
        let job = self.job(id)?;
        let state = job.state.lock().unwrap();
        let (verified, files) = job.verified(&state);
        self.queued.push(EngineEvent::ResumeData {
            id,
            data: encode_resume(&state),
            verified: Some(verified),
            piece_map: job.piece_map(&state),
            files: Some(files),
        });
        Ok(())
    }

    fn save_resume_data_if_progressed(&mut self, id: DownloadId) -> Result<(), Error> {
        self.save_resume_data(id)
    }

    fn remove(&mut self, id: DownloadId) -> Result<(), Error> {
        if let Some(job) = self.jobs.remove(&id) {
            let mut state = job.state.lock().unwrap();
            state.run = Run::Stopped;
            state.generation += 1;
        }
        self.landed_at_last_stats.remove(&id);
        self.queued.push(EngineEvent::Removed { id });
        Ok(())
    }

    fn file_progress(&mut self, id: DownloadId) -> Result<Vec<u64>, Error> {
        let job = self.job(id)?;
        let state = job.state.lock().unwrap();
        Ok(job.verified(&state).1)
    }

    fn want_range(&mut self, id: DownloadId, file: u32, offset: u64, length: u64) -> Result<u64, Error> {
        let job = self.job(id)?;
        let f = file as usize;
        let Some(size) = job.files.get(f).map(|f| f.size) else {
            return Err(Error::internal(format!("download {id} has no file {file}")));
        };
        let length = length.min(size.saturating_sub(offset));
        let range = job.covering(f, offset, length);
        let mut state = job.state.lock().unwrap();
        // Readahead follows the reader, whether or not these bytes are already here.
        state.focus = Some((f, range.end));
        let missing: Vec<PieceRef> = range.clone().map(|p| (f, p)).filter(|r| state.have[r.0][r.1].is_none()).collect();
        let Some(&(_, first)) = missing.first() else {
            return Ok(length);
        };
        let arrived = job.files[f].pieces[first].offset.saturating_sub(offset).min(length);
        for r in missing.iter().rev() {
            state.urgent.push_front(*r);
        }
        if state.run == Run::Finished {
            state.run = Run::Running;
        }
        drop(state);
        self.start_workers(&job);
        Ok(arrived)
    }

    fn written(&mut self, id: DownloadId, file: u32, offset: u64, length: u64, runs: u32) -> Result<Vec<Slice>, Error> {
        let job = self.job(id)?;
        let f = file as usize;
        let size = job.files.get(f).map(|f| f.size).unwrap_or(0);
        if length == 0 || offset >= size {
            return Err(Error::internal(format!("byte range {offset}+{length} is outside file {file} of {id}")));
        }
        let span = length.min(size - offset);
        let state = job.state.lock().unwrap();
        Ok((0..u64::from(runs))
            .map(|r| {
                let begin = offset + r * span / u64::from(runs);
                let end = (begin + 1).max(offset + (r + 1) * span / u64::from(runs));
                let pieces = job.covering(f, begin, end - begin);
                let have = pieces.clone().filter(|p| state.have[f][*p].is_some()).count();
                Slice::of(have as u32, pieces.len() as u32)
            })
            .collect())
    }

    fn stream(&mut self, id: DownloadId) -> Result<(), Error> {
        // Pieces already arrive in file order.
        self.job(id).map(|_| ())
    }

    fn limit(&mut self, id: DownloadId, rate: Option<NonZeroU32>) -> Result<(), Error> {
        let job = self.job(id)?;
        let mut state = job.state.lock().unwrap();
        state.limit = rate;
        state.paced_until = None;
        Ok(())
    }
}

impl engine_port::Engine for Usenet {
    fn port(&mut self) -> &mut dyn EnginePort {
        self
    }

    fn poll(&mut self, max_wait: Duration) -> Vec<EngineEvent> {
        if self.queued.is_empty()
            && let Ok(event) = self.events.recv_timeout(max_wait.min(POLL_WAIT))
        {
            self.queued.push(event);
        }
        self.queued.extend(self.events.try_iter());
        if self.stats_at.elapsed() >= STATS_EVERY {
            self.stats();
        }
        std::mem::take(&mut self.queued)
    }
}

/// Resolves NZBs over the pool's servers.
pub struct Resolver {
    pool: Pool,
}

impl Resolver {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl NzbPort for Resolver {
    fn resolve(&self, nzb: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            let started = Instant::now();
            let nzb = Nzb::parse(&nzb).map_err(|e| {
                Error::new(ErrorCode::InvalidRelease, e.to_string())
                    .with_hint("pass an .nzb file or URL, or an ID from newsreel search")
            })?;
            let manifest = usenet::resolve(&nzb, &self.pool).await.map_err(resolve_error)?;
            log(&format!(
                "resolved {} in {:.1}s: {} files",
                manifest.title,
                started.elapsed().as_secs_f32(),
                manifest.files.len()
            ));
            Ok(manifest.to_bytes())
        })
    }
}

fn resolve_error(e: ResolveError) -> Error {
    let search_again = "search again and pick another release";
    match &e {
        ResolveError::Article { error, .. } if error.is_missing() => {
            Error::new(ErrorCode::NoSource, e.to_string()).with_hint(search_again)
        }
        ResolveError::Article { .. } => Error::new(ErrorCode::NetworkUnavailable, e.to_string())
            .with_hint("check the usenet servers in the newsreel config and the network connection"),
        ResolveError::Compressed { .. } => Error::new(ErrorCode::InvalidRelease, e.to_string())
            .with_hint(format!("streaming needs stored (uncompressed) archives; {search_again}")),
        _ => Error::new(ErrorCode::InvalidRelease, e.to_string()).with_hint(search_again),
    }
}

#[cfg(test)]
mod tests {
    use usenet::manifest::{OutputFile as ManifestFile, Packing};

    use super::*;

    fn piece(offset: u64, length: u64) -> Piece {
        Piece { ids: vec![format!("{offset}@x")], offset, length, begin: offset, skip: 0 }
    }

    fn manifest(files: &[(&str, &[u64])]) -> Manifest {
        let files = files
            .iter()
            .map(|(path, lengths)| {
                let mut at = 0;
                let pieces = lengths
                    .iter()
                    .map(|l| {
                        at += l;
                        piece(at - l, *l)
                    })
                    .collect();
                ManifestFile { path: path.to_string(), size: at, pieces }
            })
            .collect();
        Manifest { release_id: "ab".repeat(20), title: "Some Release".into(), packing: Packing::Direct, files }
    }

    fn job(files: &[(&str, &[u64])]) -> Job {
        let m = manifest(files);
        let have: Vec<Vec<Option<u32>>> = m.files.iter().map(|f| vec![None; f.pieces.len()]).collect();
        let state = State {
            run: Run::Running,
            generation: 0,
            wanted: vec![true; m.files.len()],
            journaled: have.clone(),
            have,
            in_flight: BTreeSet::new(),
            failed: BTreeSet::new(),
            urgent: VecDeque::new(),
            focus: None,
            workers: 0,
            landed: 0,
            last_error: None,
            announced_downloading: false,
            limit: None,
            paced_until: None,
        };
        let files = m
            .files
            .into_iter()
            .map(|f| OutputFile {
                path: PathBuf::from(f.path),
                size: f.size,
                pieces: f.pieces,
                handle: Mutex::new(None),
            })
            .collect();
        let id = DownloadId::from_bytes([1; 20]);
        Job { id, files, state: Mutex::new(state), journal: Journal::new(&std::env::temp_dir(), id) }
    }

    #[test]
    fn one_file_is_saved_as_itself_and_several_in_a_folder() {
        let one = parse(&manifest(&[("Movie.mkv", &[10])]).to_bytes()).unwrap();
        assert_eq!((one.name.as_str(), one.single_file), ("Movie.mkv", true));
        assert_eq!(one.id, DownloadId::from_bytes([0xab; 20]));
        let nested = parse(&manifest(&[("Set/Movie.mkv", &[10])]).to_bytes()).unwrap();
        assert_eq!((nested.name.as_str(), nested.single_file), ("Some Release", false));
        let two = parse(&manifest(&[("a.mkv", &[10]), ("b.nfo", &[2])]).to_bytes()).unwrap();
        assert_eq!((two.name.as_str(), two.files.len()), ("Some Release", 2));
    }

    #[test]
    fn requested_ranges_come_first_then_readahead_then_file_order() {
        let job = job(&[("a.mkv", &[10; 100])]);
        let mut state = job.state.lock().unwrap();
        assert_eq!(state.pick(&job.files), Some((0, 0)), "file order by default");
        let range = job.covering(0, 505, 20);
        assert_eq!(range, 50..53, "bytes 505..525 lie in pieces 50, 51, and 52");
        state.urgent.extend(range.clone().map(|p| (0, p)));
        state.focus = Some((0, range.end));
        let picks: Vec<_> = (0..5)
            .map(|_| {
                let r = state.pick(&job.files).unwrap();
                state.in_flight.insert(r);
                r.1
            })
            .collect();
        assert_eq!(picks, [50, 51, 52, 53, 54]);
    }

    #[test]
    fn pieces_missing_everywhere_are_not_picked_again() {
        let job = job(&[("a.mkv", &[10, 10])]);
        let mut state = job.state.lock().unwrap();
        state.failed.insert((0, 0));
        assert_eq!(state.pick(&job.files), Some((0, 1)));
        state.have[0][1] = Some(7);
        assert_eq!(state.pick(&job.files), None);
        assert_eq!(state.remaining(), (1, 1), "one piece left, and it is unavailable");
    }

    #[test]
    fn resume_data_round_trips_and_rejects_another_layout() {
        let job = job(&[("a.mkv", &[10, 10, 5])]);
        let mut state = job.state.lock().unwrap();
        state.have[0][1] = Some(0xdead_beef);
        let data = encode_resume(&state);
        let mut restored = vec![vec![None; 3]];
        assert!(decode_resume(&data, &mut restored));
        assert_eq!(restored, [vec![None, Some(0xdead_beef), None]]);
        assert!(!decode_resume(&data, &mut [vec![None; 2]]));
        assert!(!decode_resume(b"junk", &mut restored));
    }
}
