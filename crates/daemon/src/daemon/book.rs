use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use torrential_core::{DownloadId, Error, Job, JobStatus, PieceMap, Place, Progress, Selection, TitleLink};

use super::admission::Cap;
use super::engine_port::Stats;
use super::files;
use super::layout::Layout;

const ETA_AFTER: Duration = Duration::from_secs(10);

#[derive(Clone, PartialEq, Debug)]
pub enum Intent {
    Running,
    Paused,
    Done { finished_at: i64 },
    Failed { error: Error },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Observed {
    Absent,
    InEngine(Phase),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Checking,
    Downloading,
    Pausing,
    Paused,
    Finishing { writes_stopped: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verify {
    Report,
    Repair,
}

pub fn status(intent: &Intent, observed: Observed) -> JobStatus {
    match (intent, observed) {
        (_, Observed::InEngine(Phase::Checking)) => JobStatus::Checking,
        (Intent::Done { finished_at }, _) => JobStatus::Done { finished_at: *finished_at },
        (Intent::Failed { error }, _) => JobStatus::Error { error: error.clone() },
        (Intent::Paused, _) => JobStatus::Paused,
        (Intent::Running, Observed::Absent | Observed::InEngine(Phase::Paused)) => JobStatus::Queued,
        (Intent::Running, Observed::InEngine(Phase::Downloading | Phase::Pausing | Phase::Finishing { .. })) => {
            JobStatus::Downloading
        }
    }
}

impl Observed {
    pub fn occupies_slot(self) -> bool {
        matches!(self, Observed::InEngine(Phase::Checking | Phase::Downloading | Phase::Finishing { .. }))
    }

    pub fn in_engine(self) -> bool {
        matches!(self, Observed::InEngine(_))
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct JobRecord {
    pub id: DownloadId,
    pub name: String,
    pub dir: PathBuf,
    pub selection: Selection,
    pub cap: Cap,
    pub added_at: i64,
    pub intent: Intent,
    pub verified: u64,
    pub piece_map: Option<PieceMap>,
    pub files_verified: Option<Vec<u64>>,
    pub title: Option<Linked>,
    pub home: Home,
}

/// Where a download's files live, which decides who may delete them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Home {
    /// The downloads folder or a folder the person chose. Its files go only when the person asks.
    Folder,
    /// A folder of its own inside torrential's plays folder.
    Plays(Play),
}

/// A download in torrential's plays folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Play {
    /// When someone last pressed play on it or streamed it.
    pub watched_at: SystemTime,
    /// The person asked for it by hand with `get`, so it never leaves the plays folder or stops
    /// by itself.
    pub by_hand: bool,
    /// The folder torrential made for it, which is the only one deleted for it.
    pub folder: Identity,
}

/// The title a download was made for, and whether it ever reached playing.
#[derive(Clone, PartialEq, Debug)]
pub struct Linked {
    pub link: TitleLink,
    pub played: bool,
}

#[derive(Clone, Debug)]
pub struct JobEntry {
    pub record: JobRecord,
    pub layout: Arc<Layout>,
    pub observed: Observed,
    pub stats: Stats,
    receiving_since: Option<Instant>,
    rate_settled: bool,
    unsourced: Duration,
    unsourced_at: Option<Instant>,
    pub verify: Option<Verify>,
}

impl JobEntry {
    pub fn new(record: JobRecord, layout: Arc<Layout>, observed: Observed) -> Self {
        Self {
            record,
            layout,
            observed,
            stats: Stats::default(),
            receiving_since: None,
            rate_settled: false,
            unsourced: Duration::ZERO,
            unsourced_at: None,
            verify: None,
        }
    }

    pub fn sample(&mut self, stats: Stats, at: Instant) {
        self.receiving_since = if stats.down_rate > 0 { self.receiving_since.or(Some(at)) } else { None };
        self.rate_settled = self.receiving_since.is_some_and(|since| at - since >= ETA_AFTER);
        let downloading = self.observed == Observed::InEngine(Phase::Downloading);
        let unsourced = stats.missing.is_some_and(|m| m.pieces > 0 && m.unavailable == m.pieces);
        match (downloading, stats.connections > 0, unsourced) {
            (false, ..) | (true, true, false) => self.forget_sources(),
            (true, false, _) => self.unsourced_at = None,
            (true, true, true) => {
                self.unsourced += self.unsourced_at.map_or(Duration::ZERO, |last| at - last);
                self.unsourced_at = Some(at);
            }
        }
        self.stats = stats;
    }

    pub fn unsourced_for(&self) -> Option<Duration> {
        self.unsourced_at.map(|_| self.unsourced)
    }

    pub fn forget_sources(&mut self) {
        self.unsourced = Duration::ZERO;
        self.unsourced_at = None;
    }

    pub fn held_for_save(&self) -> bool {
        matches!(self.record.intent, Intent::Failed { .. }) && self.observed == Observed::InEngine(Phase::Paused)
    }

    pub fn id(&self) -> DownloadId {
        self.record.id
    }

    pub fn wanted(&self) -> Option<Vec<bool>> {
        files::wanted(&self.layout, &self.record.selection).ok()
    }

    fn path(&self) -> PathBuf {
        let targets = self.wanted().map(|wanted| files::targets(&self.layout, &self.record.dir, &wanted));
        match targets.as_deref() {
            Some([only]) => only.clone(),
            _ => files::root(&self.record.dir, &self.record.name),
        }
    }

    /// Bytes in the selected files. Zero when the selection does not compile, which `get`
    /// refuses before a download exists.
    pub fn selected_size(&self) -> u64 {
        let wanted = self.wanted().unwrap_or_default();
        self.layout.files.iter().zip(wanted).filter(|(_, w)| *w).map(|(f, _)| f.size).sum()
    }

    pub fn verified(&self) -> u64 {
        match (&self.record.intent, self.observed) {
            (Intent::Done { .. }, _) => self.selected_size(),
            (_, Observed::InEngine(_)) => self.stats.verified.max(self.record.verified),
            _ => self.record.verified,
        }
    }

    pub fn piece_map(&self) -> Option<PieceMap> {
        match (&self.record.intent, self.observed) {
            (_, Observed::InEngine(_)) => self.stats.piece_map.or(self.record.piece_map),
            (Intent::Done { .. }, _) => None,
            _ => self.record.piece_map,
        }
    }

    pub fn remaining(&self) -> u64 {
        match self.record.intent {
            Intent::Running => self.selected_size().saturating_sub(self.verified()),
            _ => 0,
        }
    }

    pub fn view(&self) -> Job {
        let status = status(&self.record.intent, self.observed);
        let transferring = status == JobStatus::Downloading;
        let size = self.selected_size();
        let verified = self.verified();
        let s = if transferring { self.stats } else { Stats::default() };
        let eta = (s.down_rate > 0 && self.rate_settled && self.unsourced_at.is_none())
            .then(|| size.saturating_sub(verified).div_ceil(s.down_rate));
        Job {
            id: self.record.id,
            name: torrential_core::text::sanitize(&self.record.name),
            dir: self.record.dir.clone(),
            path: self.path(),
            status,
            progress: Progress {
                size,
                verified,
                piece_map: self.piece_map(),
                down_rate: s.down_rate,
                connections: s.connections,
                eta,
            },
            selection: self.record.selection.clone(),
            added_at: self.record.added_at,
            title: self.record.title.as_ref().map(|linked| linked.link.clone()),
            place: match self.record.home {
                Home::Folder => Place::Folder,
                Home::Plays(_) => Place::Plays,
            },
        }
    }
}

/// Streamed jobs start at once and take no slot.
pub fn to_start<'a>(
    jobs: impl Iterator<Item = &'a JobEntry>,
    max_active: usize,
    streamed: &BTreeSet<DownloadId>,
) -> Vec<DownloadId> {
    let (mut now, mut waiting) = (Vec::new(), Vec::new());
    let mut active = 0;
    for job in jobs {
        let streamed = streamed.contains(&job.id());
        if job.observed.occupies_slot() {
            active += usize::from(!streamed);
        } else if job.record.intent == Intent::Running
            && matches!(job.observed, Observed::Absent | Observed::InEngine(Phase::Paused))
        {
            if streamed {
                now.push(job.id());
            } else {
                waiting.push((job.record.added_at, job.id()));
            }
        }
    }
    waiting.sort();
    now.extend(waiting.into_iter().take(max_active.saturating_sub(active)).map(|(_, id)| id));
    now
}

pub use crate::folder::Identity;

const STEM: usize = 16;

/// The name of a play's folder: the first 16 hex digits of its download's ID, a dash, and a
/// number. Always one part of a path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlayName(String);

impl PlayName {
    /// The `n`th name for a play of `id`.
    pub fn new(id: DownloadId, n: u32) -> Self {
        Self(format!("{}-{n}", &id.to_hex()[..STEM]))
    }

    pub fn parse(name: &str) -> Option<Self> {
        let (stem, n) = name.split_once('-')?;
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        let ok = stem.len() == STEM
            && stem.bytes().all(hex)
            && (1..=10).contains(&n.len())
            && n.bytes().all(|b| b.is_ascii_digit());
        ok.then(|| Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::engine_port::Missing;
    use crate::daemon::layout::LayoutFile;
    use torrential_core::ErrorCode;

    fn entry(n: u8, added_at: i64, intent: Intent, observed: Observed) -> JobEntry {
        let id = DownloadId::from_bytes([n; 20]);
        let files = vec![LayoutFile { path: "film.mp4".into(), size: 600 << 20 }];
        let layout = Layout::new(id, "film.mp4".into(), files, true, Vec::new()).unwrap();
        let record = JobRecord {
            id,
            name: "film.mp4".into(),
            dir: PathBuf::from("/d"),
            selection: Selection::All,
            cap: Cap { bytes: 1, from_setup: false },
            added_at,
            intent,
            verified: 0,
            piece_map: None,
            files_verified: None,
            title: None,
            home: Home::Folder,
        };
        JobEntry::new(record, Arc::new(layout), observed)
    }

    #[test]
    fn an_eta_waits_until_the_rate_has_held_for_ten_seconds() {
        let mut job = entry(1, 0, Intent::Running, Observed::InEngine(Phase::Downloading));
        let start = Instant::now();
        let mut etas = Vec::new();
        for (second, down_rate) in [(0, 1 << 20), (9, 2 << 20), (10, 10 << 20), (11, 0), (12, 10 << 20), (22, 10 << 20)]
        {
            job.sample(Stats { down_rate, ..Stats::default() }, start + Duration::from_secs(second));
            etas.push(job.view().progress.eta);
        }
        assert_eq!(etas, [None, None, Some(60), None, None, Some(60)]);
        let missing = Some(Missing { pieces: 2, unavailable: 2 });
        let stats = Stats { down_rate: 10 << 20, missing, connections: 2, ..Stats::default() };
        job.sample(stats, start + Duration::from_secs(23));
        assert_eq!(job.view().progress.eta, None, "bytes arrive, but no source has the pieces still missing");
    }

    #[test]
    fn the_path_is_the_file_when_one_file_is_picked() {
        let mut job = entry(1, 0, Intent::Running, Observed::Absent);
        let files =
            vec![LayoutFile { path: "bbb.mp4".into(), size: 1 }, LayoutFile { path: "bbb.mov".into(), size: 1 }];
        let layout = Layout::new(job.id(), "Big Buck Bunny".into(), files, false, Vec::new()).unwrap();
        job.layout = Arc::new(layout);
        job.record.name = "Big Buck Bunny".into();
        assert_eq!(job.view().path, PathBuf::from("/d/Big Buck Bunny"));
        job.record.selection = Selection::Patterns(vec!["*.mp4".into()]);
        assert_eq!(job.view().path, PathBuf::from("/d/Big Buck Bunny/bbb.mp4"));
    }

    #[test]
    fn only_connected_time_without_a_source_for_any_missing_piece_counts() {
        let mut job = entry(1, 0, Intent::Running, Observed::InEngine(Phase::Downloading));
        let start = Instant::now();
        let sample = |job: &mut JobEntry, second, connections, unavailable| {
            let missing = Some(Missing { pieces: 2, unavailable });
            job.sample(Stats { connections, missing, ..Stats::default() }, start + Duration::from_secs(second));
            job.unsourced_for().map(|d| d.as_secs())
        };
        assert_eq!(sample(&mut job, 0, 0, 2), None, "nothing connected");
        assert_eq!(sample(&mut job, 1, 2, 1), None, "a server has one of the pieces");
        assert_eq!(sample(&mut job, 2, 2, 2), Some(0));
        assert_eq!(sample(&mut job, 5, 2, 2), Some(3));
        assert_eq!(sample(&mut job, 6, 0, 2), None, "nothing connected to ask");
        assert_eq!(sample(&mut job, 900, 2, 2), Some(3), "the outage and the first sample after it earn nothing");
        assert_eq!(sample(&mut job, 901, 2, 2), Some(4));
        assert_eq!(sample(&mut job, 902, 2, 1), None, "a source turned up");
        assert_eq!(sample(&mut job, 903, 2, 2), Some(0), "and the count starts over");
        job.observed = Observed::InEngine(Phase::Pausing);
        assert_eq!(sample(&mut job, 904, 2, 2), None, "only downloading counts");
    }

    #[test]
    fn starts_the_oldest_waiting_jobs_into_free_slots() {
        let jobs = [
            entry(1, 10, Intent::Running, Observed::InEngine(Phase::Downloading)),
            entry(2, 30, Intent::Running, Observed::Absent),
            entry(3, 20, Intent::Running, Observed::Absent),
            entry(4, 5, Intent::Paused, Observed::Absent),
            entry(6, 40, Intent::Running, Observed::InEngine(Phase::Paused)),
        ];
        let none = BTreeSet::new();
        let ids: Vec<u8> = to_start(jobs.iter(), 3, &none).iter().map(|id| id.as_bytes()[0]).collect();
        assert_eq!(ids, [3, 2]);
        let ids: Vec<u8> = to_start(jobs.iter(), 4, &none).iter().map(|id| id.as_bytes()[0]).collect();
        assert_eq!(ids, [3, 2, 6]);
    }

    #[test]
    fn the_piece_map_is_the_engines_then_the_last_saves_and_ends_when_done() {
        let map = |c: &str| Some(c.repeat(64).parse::<PieceMap>().unwrap());
        let mut job = entry(1, 0, Intent::Running, Observed::InEngine(Phase::Downloading));
        job.record.piece_map = map("2");
        assert_eq!(job.view().progress.piece_map, map("2"), "before the engine's first sample");
        job.stats.piece_map = map("5");
        assert_eq!(job.view().progress.piece_map, map("5"));
        job.record.intent = Intent::Failed { error: Error::new(ErrorCode::DiskFull, "disk full") };
        job.observed = Observed::Absent;
        assert_eq!(job.view().progress.piece_map, map("2"), "out of the engine, the last save's");
        job.record.intent = Intent::Done { finished_at: 7 };
        assert_eq!(job.view().progress.piece_map, None);
    }
}
