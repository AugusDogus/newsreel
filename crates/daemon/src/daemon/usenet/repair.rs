//! Rebuilding pieces no server sends intact from the release's PAR2 files, as SABnzbd does.
//!
//! A PAR2 set protects the files as they were posted: for a release in RAR volumes, the volumes,
//! not the video stored in them. So a damaged piece of the video is a byte range of one volume,
//! which falls in some of the set's slices. Rebuilding those slices reads every other slice of the
//! set: the video's bytes from disk as they land, and the volumes' bytes that are not on disk (RAR
//! headers, and any stored file the download does not keep), fetched from their articles and held
//! in memory. The rebuilt slices are then cut back into the video's bytes and written.
//!
//! A repair runs as one task per download, started when its first piece is set aside. It fetches
//! the set's index and only as many recovery blocks as the damaged slices need, feeds every other
//! slice as it lands, and rebuilds once the rest of the set is in. Its fetches go at urgent
//! priority while a reader waits on the damaged bytes, and beside the download otherwise. When it cannot work (no PAR2
//! set covers the bytes, too few blocks, blocks that cannot solve these slices), the pieces stay
//! set aside with the reason, and the download fails as it did before repair existed: at once for
//! a reader waiting on them, which lets a title play move to its next source, and at the end for
//! a plain download.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use newsreel_core::RepairStage;
use usenet::article::ArticleError;
use usenet::nntp::Priority;
use usenet::nzb::NzbFile;
use usenet::par2::{Feed, Par2Files, RecoverySet, Repair, RepairError, Scanner};
use usenet::yenc::Part;
use usenet::{Nzb, article};

use super::{Job, PieceRef, RETRY_AFTER, Run, State, finish, read_at, short};
use crate::log::log;

/// The most bytes of the set's files a repair holds in memory: the parts of the posted files that
/// are not the files on disk, such as RAR headers or a stored sample the download does not keep.
const HELD_MOST: u64 = 256 << 20;
/// How often a repair looks for slices that have landed, besides being woken when a piece lands.
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// Articles a repair fetches at once: a set's volumes' headers in one round, about 25 for a film
/// in 15 volumes, since on a link where each connection is slow every round takes seconds.
const FETCH_AT_ONCE: usize = 32;
/// Slices fed in one go off the async threads.
const FEED_AT_ONCE: usize = 64;

/// Where a download's repair stands.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) enum Repairing {
    /// Nothing is being repaired.
    #[default]
    Idle,
    /// These pieces, set aside in `failed`, are being rebuilt.
    Active { pieces: BTreeSet<PieceRef>, stage: RepairStage },
    /// The release's PAR2 files cannot rebuild the pieces set aside, and why.
    Impossible(String),
}

impl Repairing {
    /// The piece is being rebuilt, so a reader waiting on it waits.
    pub(super) fn covers(&self, piece: PieceRef) -> bool {
        matches!(self, Self::Active { pieces, .. } if pieces.contains(&piece))
    }

    pub(super) fn impossible(&self) -> Option<&str> {
        match self {
            Self::Impossible(why) => Some(why),
            _ => None,
        }
    }

    fn set_stage(&mut self, to: RepairStage) {
        if let Self::Active { stage, .. } = self {
            *stage = to;
        }
    }
}

/// Puts a piece set aside up for repair. Returns the epoch of a repair to start when none runs.
pub(super) fn lost(state: &mut State, piece: PieceRef) -> Option<u64> {
    match &mut state.repair {
        Repairing::Idle => {
            state.repair = Repairing::Active { pieces: BTreeSet::from([piece]), stage: RepairStage::Preparing };
            Some(state.repair_epoch)
        }
        Repairing::Active { pieces, .. } => {
            pieces.insert(piece);
            None
        }
        Repairing::Impossible(_) => None,
    }
}

/// Why a repair ended without rebuilding.
enum Stop {
    /// The download was resumed, removed, or stopped, which starts over or ends its repair.
    Cancelled,
    /// The release's PAR2 files cannot rebuild the pieces.
    Cannot(String),
}

impl From<RepairError> for Stop {
    fn from(e: RepairError) -> Self {
        Self::Cannot(match e {
            RepairError::NotEnoughRecovery { damaged, blocks } => format!(
                "the damage spans {} of the PAR2 set, and the release's PAR2 files hold {}",
                units(damaged, "slice"),
                units(blocks, "recovery block")
            ),
            RepairError::Dependent { damaged } => {
                format!("the release's recovery blocks cannot rebuild these {damaged} slices together")
            }
            e => e.to_string(),
        })
    }
}

/// Repairs the download's pieces set aside in `epoch`, until none is left or it cannot.
pub(super) async fn run(job: Arc<Job>, epoch: u64) {
    let outcome = repair(&job, epoch).await;
    let mut state = job.state.lock().unwrap();
    if state.repair_epoch != epoch {
        return;
    }
    match outcome {
        Ok(()) | Err(Stop::Cancelled) => {}
        Err(Stop::Cannot(why)) => {
            log(&format!("{} cannot repair its damaged articles from the release's PAR2 files: {why}", short(job.id)));
            state.repair = Repairing::Impossible(why);
            if state.workers == 0 && state.run == Run::Running {
                finish(&job, &mut state);
            }
        }
    }
}

async fn repair(job: &Arc<Job>, epoch: u64) -> Result<(), Stop> {
    let recovery = job.recovery.clone().ok_or_else(|| Stop::Cannot("the release posts no PAR2 files".into()))?;
    let (nzb, par2) = (&recovery.nzb, &recovery.par2);
    let started = std::time::Instant::now();
    let index = fetch_whole(job, epoch, &nzb.files[par2.index]).await?;
    let set = RecoverySet::parse(&index.concat())
        .map_err(|e| Stop::Cannot(format!("its PAR2 index {} does not read: {e}", nzb.files[par2.index].filename)))?;
    let mut scanner = Scanner::new(&set);
    index.iter().for_each(|data| scanner.push(data));
    let mut fetched = BTreeSet::from([par2.index]);
    let mut bad: BTreeSet<u32> = BTreeSet::new();
    loop {
        let plan = Plan::new(job, epoch, nzb, par2, &set).await?;
        let lost: BTreeSet<PieceRef> = match &guard(job, epoch)?.repair {
            Repairing::Active { pieces, .. } => pieces.clone(),
            _ => return Err(Stop::Cancelled),
        };
        let mut damaged = plan.damaged(job, &lost)?;
        damaged.extend(&bad);
        let rebuild = loop {
            while scanner.blocks().len() < damaged.len() {
                let next = par2
                    .choose(damaged.len() as u32, set.slice_size())
                    .into_iter()
                    .find(|v| !fetched.contains(v))
                    .ok_or(RepairError::NotEnoughRecovery { damaged: damaged.len(), blocks: scanner.blocks().len() })?;
                fetched.insert(next);
                fetch_whole(job, epoch, &nzb.files[next]).await?.iter().for_each(|data| scanner.push(data));
            }
            let blocks = scanner.blocks().to_vec();
            match Repair::new(&set, &damaged.iter().copied().collect::<Vec<_>>(), blocks) {
                Ok(rebuild) => break rebuild,
                // A spare block solves the rare set of slices the blocks held cannot.
                Err(RepairError::Dependent { .. }) => {
                    let next = par2
                        .choose(damaged.len() as u32 + 1, set.slice_size())
                        .into_iter()
                        .find(|v| !fetched.contains(v));
                    let next = next.ok_or(RepairError::Dependent { damaged: damaged.len() })?;
                    fetched.insert(next);
                    fetch_whole(job, epoch, &nzb.files[next]).await?.iter().for_each(|data| scanner.push(data));
                }
                Err(e) => return Err(e.into()),
            }
        };
        log(&format!(
            "{} repairs {} from its PAR2 files: {} slices of {} bytes, {} recovery blocks fetched in {:.1}s",
            short(job.id),
            units(lost.len(), "damaged article"),
            damaged.len(),
            set.slice_size(),
            rebuild.blocks(),
            started.elapsed().as_secs_f32()
        ));
        guard(job, epoch)?.repair.set_stage(RepairStage::Waiting);
        let rebuild = match feed(job, epoch, &plan, rebuild, &lost, &mut bad).await? {
            (rebuild, Fed::All) => rebuild,
            // More slices are damaged than the blocks held rebuild, more pieces were set aside,
            // or the selection changed: plan again.
            (_, Fed::Again) => continue,
        };
        // A paused download writes nothing, the repair's bytes included.
        while guard(job, epoch)?.run != Run::Running {
            tokio::time::sleep(LOOK_EVERY).await;
        }
        guard(job, epoch)?.repair.set_stage(RepairStage::Rebuilding);
        let rebuilding = std::time::Instant::now();
        let written = {
            let (job, plan) = (job.clone(), plan.clone());
            tokio::task::spawn_blocking(move || -> Result<BTreeSet<PieceRef>, Stop> {
                let rebuilt = rebuild.finish()?;
                plan.write(&job, &rebuilt).map_err(Stop::Cannot)
            })
            .await
            .map_err(|e| Stop::Cannot(format!("the repair stopped: {e}")))??
        };
        let reread = {
            let (job, written) = (job.clone(), written.clone());
            tokio::task::spawn_blocking(move || written.iter().map(|&p| (p, job.read_back(p))).collect::<Vec<_>>())
                .await
                .map_err(|e| Stop::Cannot(format!("the repair stopped: {e}")))?
        };
        let mut state = guard(job, epoch)?;
        for (piece, crc) in reread {
            match crc {
                Ok(Some(crc)) => {
                    state.have[piece.0][piece.1] = Some(crc);
                    state.failed.remove(&piece);
                    job.journal.record(piece, crc);
                }
                Ok(None) => {
                    return Err(Stop::Cannot(format!(
                        "{} was not on disk after the repair wrote it",
                        job.place(piece)
                    )));
                }
                Err(e) => return Err(Stop::Cannot(e.message)),
            }
        }
        log(&format!(
            "{} repaired {} in {:.1}s once the rest of the release was in, {:.1}s after it was found",
            short(job.id),
            units(lost.len(), "damaged article"),
            rebuilding.elapsed().as_secs_f32(),
            started.elapsed().as_secs_f32()
        ));
        let left = match &mut state.repair {
            Repairing::Active { pieces, .. } => {
                pieces.retain(|p| !lost.contains(p));
                pieces.len()
            }
            _ => 0,
        };
        if left == 0 {
            state.repair = Repairing::Idle;
            if state.workers == 0 && state.run == Run::Running {
                finish(job, &mut state);
            }
            return Ok(());
        }
        // Pieces set aside while this repair ran get the next one.
        state.repair.set_stage(RepairStage::Preparing);
    }
}

/// The download's state, or `Cancelled` once this repair's epoch has passed.
fn guard(job: &Job, epoch: u64) -> Result<std::sync::MutexGuard<'_, State>, Stop> {
    let state = job.state.lock().unwrap();
    if state.repair_epoch != epoch || state.run == Run::Stopped {
        return Err(Stop::Cancelled);
    }
    Ok(state)
}

fn units(n: usize, noun: &str) -> String {
    newsreel_core::units::count(n, noun)
}

enum Fed {
    /// Every slice not damaged has been fed.
    All,
    /// The repair has to be planned again: a slice turned out damaged and the blocks held cannot
    /// rebuild it too, more pieces were set aside, or the selection changed which bytes are on disk.
    Again,
}

/// Feeds the repair every slice that is not damaged, as the pieces it reads land.
async fn feed(
    job: &Arc<Job>,
    epoch: u64,
    plan: &Plan,
    mut rebuild: Repair,
    lost: &BTreeSet<PieceRef>,
    bad: &mut BTreeSet<u32>,
) -> Result<(Repair, Fed), Stop> {
    loop {
        let ready: Vec<u32> = {
            let state = guard(job, epoch)?;
            let more =
                matches!(&state.repair, Repairing::Active { pieces, .. } if pieces.iter().any(|p| !lost.contains(p)));
            if state.wanted != plan.wanted || more {
                return Ok((rebuild, Fed::Again));
            }
            rebuild.waiting().filter(|&s| plan.ready(s, &state)).take(FEED_AT_ONCE).collect()
        };
        if ready.is_empty() {
            if rebuild.waiting_count() == 0 {
                return Ok((rebuild, Fed::All));
            }
            let _ = tokio::time::timeout(LOOK_EVERY, job.landed.notified()).await;
            continue;
        }
        let (job_, plan_) = (job.clone(), plan.clone());
        let (fed, outcome) = tokio::task::spawn_blocking(move || {
            let outcome = plan_.feed(&job_, &mut rebuild, &ready);
            (rebuild, outcome)
        })
        .await
        .map_err(|e| Stop::Cannot(format!("the repair stopped: {e}")))?;
        rebuild = fed;
        match outcome {
            Ok(damaged) => bad.extend(damaged),
            Err(FeedError::Unsolvable(slice)) => {
                bad.insert(slice);
                return Ok((rebuild, Fed::Again));
            }
            Err(FeedError::Read(why)) => return Err(Stop::Cannot(why)),
        }
    }
}

enum FeedError {
    /// Slice `n` arrived damaged, and the blocks held cannot rebuild it along with the others.
    Unsolvable(u32),
    Read(String),
}

/// Where one run of a set file's bytes comes from.
#[derive(Clone, Debug)]
enum Source {
    /// A piece of a file the download writes, at this offset of that file.
    Piece { piece: PieceRef, offset: u64 },
    /// Fetched and held, as no file on disk has them.
    Held(Arc<[u8]>),
    /// Not to be had: in an article no server sent intact.
    Gone,
}

#[derive(Clone, Debug)]
struct Span {
    /// Where the run starts in its set file.
    at: u64,
    len: u64,
    from: Source,
}

/// How to read every slice of a recovery set from what the download has.
#[derive(Clone)]
struct Plan {
    set: Arc<RecoverySet>,
    /// Each file of the set as runs, in order and without gaps.
    files: Arc<Vec<Vec<Span>>>,
    /// The pieces each slice reads.
    needs: Arc<Vec<Vec<PieceRef>>>,
    /// Where each piece lies in the set: the file and the range.
    placed: Arc<HashMap<PieceRef, (usize, u64, u64)>>,
    /// The posted file each piece's article is part of, for errors.
    posted: Arc<HashMap<PieceRef, String>>,
    /// The download's selection the plan was made for.
    wanted: Vec<bool>,
}

impl Plan {
    /// Maps the set's files to the posted files and the pieces of the download, and fetches the
    /// bytes of the set that no file on disk holds.
    async fn new(job: &Arc<Job>, epoch: u64, nzb: &Nzb, par2: &Par2Files, set: &RecoverySet) -> Result<Self, Stop> {
        let wanted = guard(job, epoch)?.wanted.clone();
        // The set's file each posted file is, by name, or else by its size and first bytes, since
        // posters rename files.
        let mut posted_as: Vec<Option<usize>> = nzb.files.iter().map(|f| set.file_named(&f.filename)).collect();
        // First articles fetched to match files, kept for the bytes they hold that are not on disk.
        let mut first: HashMap<usize, Part> = HashMap::new();
        let named: BTreeSet<usize> = posted_as.iter().flatten().copied().collect();
        if named.len() < set.files().len() {
            let recovery: BTreeSet<usize> = par2.files().collect();
            let unnamed: Vec<usize> = (0..nzb.files.len())
                .filter(|&n| posted_as[n].is_none() && !recovery.contains(&n))
                .filter(|&n| !nzb.files[n].filename.to_ascii_lowercase().ends_with(".par2"))
                .filter(|&n| !nzb.files[n].segments.is_empty())
                .collect();
            let ids = unnamed.iter().map(|&n| nzb.files[n].segments[0].message_ids.clone()).collect();
            for (n, part) in unnamed.into_iter().zip(fetch_all(job, epoch, ids).await?) {
                if let Some(part) = part {
                    posted_as[n] = set.file_starting(part.file_size, &part.data).filter(|k| !named.contains(k));
                    // Its first article holds the volume's first header, which is read below.
                    first.insert(n, part);
                }
            }
        }
        let mut article: HashMap<&str, (usize, usize)> = HashMap::new();
        for (n, file) in nzb.files.iter().enumerate() {
            for (i, segment) in file.segments.iter().enumerate() {
                for id in &segment.message_ids {
                    article.insert(id.as_str(), (n, i));
                }
            }
        }
        let mut runs: Vec<Vec<Span>> = vec![Vec::new(); set.files().len()];
        // Each posted file's articles whose offset a piece gives.
        let mut begins: Vec<HashMap<usize, u64>> = vec![HashMap::new(); nzb.files.len()];
        let mut placed = HashMap::new();
        let mut posted = HashMap::new();
        for (f, file) in job.files.iter().enumerate() {
            for (p, piece) in file.pieces.iter().enumerate() {
                let Some(&(n, i)) = piece.ids.iter().find_map(|id| article.get(id.as_str())) else { continue };
                begins[n].insert(i, piece.begin);
                posted.insert((f, p), nzb.files[n].filename.clone());
                let Some(k) = posted_as[n] else { continue };
                let at = piece.begin + piece.skip;
                placed.insert((f, p), (k, at, piece.length));
                if wanted[f] {
                    runs[k].push(Span {
                        at,
                        len: piece.length,
                        from: Source::Piece { piece: (f, p), offset: piece.offset },
                    });
                }
            }
        }
        // The rest of each set file is fetched from the articles that hold it, all at once.
        let mut held_bytes = 0;
        let mut missing: Vec<Vec<(u64, u64)>> = vec![Vec::new(); set.files().len()];
        let mut wanted_articles: Vec<(usize, Vec<String>)> = Vec::new();
        let mut parts: Vec<(usize, Option<Part>)> = Vec::new();
        for (k, file) in set.files().iter().enumerate() {
            runs[k].sort_by_key(|r| r.at);
            let mut at = 0;
            for run in &runs[k] {
                if run.at < at {
                    return Err(Stop::Cannot(format!("pieces overlap at byte {} of {}", run.at, file.name)));
                }
                if run.at > at {
                    missing[k].push((at, run.at));
                }
                at = run.at + run.len;
            }
            if at < file.size {
                missing[k].push((at, file.size));
            }
            if missing[k].is_empty() {
                continue;
            }
            let Some(n) = posted_as.iter().position(|&posted| posted == Some(k)) else { continue };
            held_bytes += missing[k].iter().map(|(s, e)| e - s).sum::<u64>();
            if held_bytes > HELD_MOST {
                return Err(Stop::Cannot(format!(
                    "rebuilding would hold more than {} of files this download does not keep in memory; select every file of the release and resume it",
                    newsreel_core::units::format_size(HELD_MOST)
                )));
            }
            for i in spanning(&nzb.files[n], &begins[n], file.size, &missing[k]) {
                match if i == 0 { first.remove(&n) } else { None } {
                    Some(part) => parts.push((k, Some(part))),
                    None => wanted_articles.push((k, nzb.files[n].segments[i].message_ids.clone())),
                }
            }
        }
        let (from, ids): (Vec<usize>, Vec<Vec<String>>) = wanted_articles.into_iter().unzip();
        parts.extend(from.into_iter().zip(fetch_all(job, epoch, ids).await?));
        let mut held: Vec<Vec<Span>> = vec![Vec::new(); set.files().len()];
        for (k, part) in parts {
            let Some(part) = part else { continue };
            for &(s, e) in &missing[k] {
                let (x, y) = (s.max(part.begin), e.min(part.end()));
                if x < y {
                    let bytes = &part.data[(x - part.begin) as usize..(y - part.begin) as usize];
                    held[k].push(Span { at: x, len: y - x, from: Source::Held(bytes.into()) });
                }
            }
        }
        // What no article sent is gone.
        for (k, mut held) in held.into_iter().enumerate() {
            held.sort_by_key(|r| r.at);
            for &(s, e) in &missing[k] {
                let mut at = s;
                for run in held.iter().filter(|r| r.at >= s && r.at < e) {
                    if run.at > at {
                        runs[k].push(Span { at, len: run.at - at, from: Source::Gone });
                    }
                    at = at.max(run.at + run.len);
                }
                if at < e {
                    runs[k].push(Span { at, len: e - at, from: Source::Gone });
                }
            }
            runs[k].extend(held);
            runs[k].sort_by_key(|r| r.at);
        }
        let mut needs = vec![Vec::new(); set.slice_count() as usize];
        for (k, file) in runs.iter().enumerate() {
            for run in file {
                if let Source::Piece { piece, .. } = run.from {
                    for s in set.slices_in(k, run.at..run.at + run.len) {
                        needs[s as usize].push(piece);
                    }
                }
            }
        }
        Ok(Self {
            set: Arc::new(set.clone()),
            files: Arc::new(runs),
            needs: Arc::new(needs),
            placed: Arc::new(placed),
            posted: Arc::new(posted),
            wanted,
        })
    }

    /// The slices that cannot be read: those holding a lost piece or bytes no article sent.
    fn damaged(&self, job: &Job, lost: &BTreeSet<PieceRef>) -> Result<BTreeSet<u32>, Stop> {
        let mut damaged = BTreeSet::new();
        for &piece in lost {
            let Some(&(k, at, len)) = self.placed.get(&piece) else {
                let file = self.posted.get(&piece).map_or("a posted file".into(), |name| name.clone());
                return Err(Stop::Cannot(format!(
                    "{} lies in {file}, which the release's PAR2 files do not cover",
                    job.place(piece)
                )));
            };
            damaged.extend(self.set.slices_in(k, at..at + len));
        }
        for (k, file) in self.files.iter().enumerate() {
            for run in file.iter().filter(|r| matches!(r.from, Source::Gone)) {
                damaged.extend(self.set.slices_in(k, run.at..run.at + run.len));
            }
        }
        Ok(damaged)
    }

    /// Every piece the slice reads has landed.
    fn ready(&self, slice: u32, state: &State) -> bool {
        self.needs[slice as usize].iter().all(|&(f, p)| state.have[f][p].is_some())
    }

    /// Feeds slices to the repair. Returns the ones that arrived damaged and joined the ones to
    /// rebuild.
    fn feed(&self, job: &Job, rebuild: &mut Repair, slices: &[u32]) -> Result<Vec<u32>, FeedError> {
        let mut open: HashMap<usize, File> = HashMap::new();
        let mut buf = vec![0u8; self.set.slice_size() as usize];
        let mut damaged = Vec::new();
        for &index in slices {
            let Some(slice) = self.set.slice(index) else { continue };
            let bytes = &mut buf[..slice.length as usize];
            self.read(job, slice.file, slice.offset, bytes, &mut open).map_err(FeedError::Read)?;
            match rebuild.feed(index, bytes) {
                Ok(Feed::Accepted) => {}
                Ok(Feed::Damaged) => damaged.push(index),
                Err(RepairError::NotEnoughRecovery { .. } | RepairError::Dependent { .. }) => {
                    return Err(FeedError::Unsolvable(index));
                }
                Err(e) => return Err(FeedError::Read(e.to_string())),
            }
        }
        Ok(damaged)
    }

    /// Reads bytes of set file `k` from `at`, from the files on disk and the bytes held.
    fn read(
        &self,
        job: &Job,
        k: usize,
        at: u64,
        out: &mut [u8],
        open: &mut HashMap<usize, File>,
    ) -> Result<(), String> {
        let end = at + out.len() as u64;
        let runs = &self.files[k];
        let first = runs.partition_point(|r| r.at + r.len <= at);
        for run in runs[first..].iter().take_while(|r| r.at < end) {
            let (x, y) = (at.max(run.at), end.min(run.at + run.len));
            let into = &mut out[(x - at) as usize..(y - at) as usize];
            match &run.from {
                Source::Piece { piece: (f, _), offset } => {
                    let path = &job.files[*f].path;
                    let file = match open.entry(*f) {
                        std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                        std::collections::hash_map::Entry::Vacant(v) => {
                            v.insert(File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?)
                        }
                    };
                    read_at(file, into, offset + (x - run.at))
                        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                }
                Source::Held(bytes) => into.copy_from_slice(&bytes[(x - run.at) as usize..(y - run.at) as usize]),
                // Only a damaged slice holds these, and damaged slices are not read.
                Source::Gone => into.fill(0),
            }
        }
        Ok(())
    }

    /// Writes the rebuilt slices' bytes that belong to files on disk. Returns the pieces written.
    fn write(&self, job: &Job, rebuilt: &[usenet::par2::Rebuilt]) -> Result<BTreeSet<PieceRef>, String> {
        let mut written = BTreeSet::new();
        for r in rebuilt {
            let (k, start) = (r.slice.file, r.slice.offset);
            let end = start + r.data.len() as u64;
            let runs = &self.files[k];
            let first = runs.partition_point(|run| run.at + run.len <= start);
            for run in runs[first..].iter().take_while(|run| run.at < end) {
                let Source::Piece { piece, offset } = run.from else { continue };
                let (x, y) = (start.max(run.at), end.min(run.at + run.len));
                let bytes = &r.data[(x - start) as usize..(y - start) as usize];
                job.write_range(piece.0, offset + (x - run.at), bytes)
                    .map_err(|e| format!("cannot write {}: {e}", job.files[piece.0].path.display()))?;
                written.insert(piece);
            }
        }
        job.sync().map_err(|e| format!("cannot save the repaired bytes to disk: {e}"))?;
        Ok(written)
    }
}

/// The segments of a posted file that may hold bytes in `missing`. A segment a piece places runs
/// to the next one placed, or to the end of the file; one no piece places is always fetched.
fn spanning(file: &NzbFile, begins: &HashMap<usize, u64>, size: u64, missing: &[(u64, u64)]) -> Vec<usize> {
    let mut out = Vec::new();
    for i in 0..file.segments.len() {
        let Some(&begin) = begins.get(&i) else {
            out.push(i);
            continue;
        };
        let end = (i + 1..file.segments.len()).find_map(|j| begins.get(&j).copied()).unwrap_or(size);
        if missing.iter().any(|&(s, e)| s < end && begin < e) {
            out.push(i);
        }
    }
    out
}

/// One article, or None when no server sends it intact. Waits out servers that cannot be reached.
/// It goes at urgent priority while a reader waits on the repair, past the download's requests,
/// and beside them otherwise, so a repair nobody waits on yet does not slow a seek.
async fn fetch(job: &Job, epoch: u64, ids: &[String]) -> Result<Option<Part>, Stop> {
    loop {
        let blocking = guard(job, epoch)?.repair_stats().is_some_and(|r| r.blocking);
        let pool = if blocking { job.pool.with_priority(Priority::Urgent) } else { job.pool.clone() };
        match article::fetch(&pool, ids).await {
            Ok(part) => return Ok(Some(part)),
            Err(ArticleError::Yenc { .. } | ArticleError::Nntp(usenet::nntp::NntpError::Missing { .. })) => {
                return Ok(None);
            }
            Err(_) => {
                drop(guard(job, epoch)?);
                tokio::time::sleep(RETRY_AFTER).await;
            }
        }
    }
}

/// A posted file's articles' bytes in order, leaving out those no server sends intact.
async fn fetch_whole(job: &Job, epoch: u64, file: &NzbFile) -> Result<Vec<Vec<u8>>, Stop> {
    let ids: Vec<Vec<String>> = file.segments.iter().map(|s| s.message_ids.clone()).collect();
    let parts = fetch_all(job, epoch, ids).await?;
    Ok(parts.into_iter().flatten().map(|p| p.data).collect())
}

/// Articles, in order, as [`fetch`] gives them, a few at once.
async fn fetch_all(job: &Job, epoch: u64, ids: Vec<Vec<String>>) -> Result<Vec<Option<Part>>, Stop> {
    let fetches = ids.into_iter().map(|ids| async move { fetch(job, epoch, &ids).await });
    futures_util::stream::iter(fetches).buffered(FETCH_AT_ONCE).collect::<Vec<_>>().await.into_iter().collect()
}
