//! A video as an HLS film for a TV: a whole playlist from the start, cut at the video's own
//! keyframes, and each segment made when the TV asks for it. One ffmpeg run copies the video and
//! converts the audio only if it must, writing a fragmented MP4 that newsreel cuts into the
//! playlist's segments, since every fragment starts on a keyframe. A run stops reading ahead
//! once it is `AHEAD` segments past what the TV asked for, and a TV that asks for a segment far
//! from the run gets a new run from there. So the TV seeks anywhere with its own remote, starts
//! about as fast as a stream, and the disk holds only the last few segments.

pub mod index;

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use newsreel_core::{DownloadId, Error, ErrorCode};
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::super::ctx::Ctx;
use super::super::http;
use super::super::owner::{Command, StreamFile};
use super::ffmpeg::{self, Encoder, Picture};
use super::{Media, probe};
use crate::log::log;
use index::{boxes, child};

/// How long a segment runs, at least, when the keyframes allow.
pub const TARGET: f64 = 6.0;
/// How far a fragment's start may be from the keyframe the playlist names, as their clocks round
/// differently. A keyframe this close after another starts no segment.
const SLACK: f64 = 0.25;
/// Segments a run makes past the furthest one the TV asked for, before it waits.
const AHEAD: usize = 8;
/// A segment this many past where the run has got to is waited for, not run anew.
const NEAR: usize = 3;
/// Segments kept on disk, nearest the TV's place first.
const KEPT: usize = 16;
/// How long a segment may take to make: the pieces it needs may not have been downloaded yet.
const MADE_WITHIN: Duration = Duration::from_secs(120);
/// The last segment is not shorter than this, or it joins the one before.
const SHORTEST_LAST: f64 = 1.0;
/// How much of what ffmpeg prints about a failure is kept.
const COMPLAINT: usize = 4 * 1024;

pub const PLAYLIST: &str = "application/vnd.apple.mpegurl";
pub const INIT: &str = "init.mp4";
pub const SEGMENT: &str = "m4s";

/// Where each segment starts, in seconds, and how long the video is.
#[derive(Clone, PartialEq, Debug)]
pub struct Plan {
    pub starts: Vec<f64>,
    pub duration: f64,
}

impl Plan {
    /// Segments of at least `TARGET` seconds that start on the keyframes given, which are sorted.
    pub fn on(keyframes: &[f64], duration: f64) -> Option<Plan> {
        if !(duration.is_finite() && duration > 0.0) {
            return None;
        }
        let mut starts = vec![0.0];
        let mut before = 0.0;
        for &key in keyframes {
            let last = *starts.last().expect("one start");
            if key - last >= TARGET && key - before >= 2.0 * SLACK && duration - key >= SHORTEST_LAST {
                starts.push(key);
            }
            before = key;
        }
        Some(Plan { starts, duration })
    }

    /// Segments `TARGET` seconds long, for a video encoded again with a keyframe at each start.
    pub fn every(duration: f64) -> Option<Plan> {
        if !(duration.is_finite() && duration > 0.0) {
            return None;
        }
        let count = ((duration - SHORTEST_LAST) / TARGET).ceil().max(1.0) as usize;
        Some(Plan { starts: (0..count).map(|i| i as f64 * TARGET).collect(), duration })
    }

    /// The segment that holds `time`.
    pub fn segment_of(&self, time: f64) -> usize {
        self.starts.partition_point(|start| *start <= time + SLACK).saturating_sub(1)
    }

    fn end(&self, segment: usize) -> f64 {
        self.starts.get(segment + 1).copied().unwrap_or(self.duration)
    }

    /// The playlist: every segment, so the TV shows the whole film and seeks anywhere in it.
    pub fn playlist(&self) -> String {
        let longest = (0..self.starts.len()).map(|i| self.end(i) - self.starts[i]).fold(0.0, f64::max);
        let mut text = format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"{INIT}\"\n",
            longest.ceil() as u64
        );
        for (i, start) in self.starts.iter().enumerate() {
            text.push_str(&format!("#EXTINF:{:.6},\n{i}.{SEGMENT}\n", self.end(i) - start));
        }
        text.push_str("#EXT-X-ENDLIST\n");
        text
    }
}

/// What a TV may ask of a cast's HLS film, by the name after the grant's folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    Playlist,
    Init,
    Segment(usize),
}

impl Asked {
    pub fn of(name: &str) -> Option<Asked> {
        if name.ends_with(".m3u8") {
            return Some(Asked::Playlist);
        }
        if name == INIT {
            return Some(Asked::Init);
        }
        name.strip_suffix(&format!(".{SEGMENT}"))?.parse().ok().map(Asked::Segment)
    }
}

/// Builds the arguments of a run that starts at a time in seconds.
pub type Arguments = Box<dyn Fn(f64) -> Vec<String> + Send + Sync>;

/// A cast's HLS film: its plan, and the run making its segments. Dropping it stops the run and
/// deletes the segments.
pub struct Film {
    shared: Arc<Shared>,
}

struct Shared {
    plan: Plan,
    program: PathBuf,
    arguments: Arguments,
    dir: PathBuf,
    /// The segment a TV asking for the init segment first is about to ask for.
    first: usize,
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    init: Option<Arc<Vec<u8>>>,
    /// The segments on disk.
    made: BTreeSet<usize>,
    run: Option<Run>,
    /// The furthest segment the TV asked for since the run started.
    wanted: usize,
    next: u64,
}

struct Run {
    id: u64,
    from: usize,
    /// The last segment it finished.
    made: Option<usize>,
    task: JoinHandle<()>,
    /// Why it stopped, once it has, without finishing the video.
    failed: Option<String>,
    over: bool,
}

impl Film {
    /// A film of `plan`, made by `program` with `arguments`, its segments kept in `dir`, which is
    /// made now. A TV that asks for the init segment first starts at `start_at` seconds.
    pub fn new(plan: Plan, program: PathBuf, arguments: Arguments, dir: PathBuf, start_at: f64) -> io::Result<Film> {
        std::fs::create_dir_all(&dir)?;
        let first = plan.segment_of(start_at);
        let shared = Shared { plan, program, arguments, dir, first, state: Mutex::default(), changed: Notify::new() };
        Ok(Film { shared: Arc::new(shared) })
    }

    pub fn plan(&self) -> &Plan {
        &self.shared.plan
    }

    pub fn playlist(&self) -> String {
        self.shared.plan.playlist()
    }

    /// The init segment: the `ftyp` and `moov` every run writes the same.
    pub async fn init(&self) -> Result<Arc<Vec<u8>>, Error> {
        let shared = &self.shared;
        self.until(
            |state| state.init.clone(),
            |state| {
                if state.run.is_none() {
                    shared.start(state, shared.first);
                }
            },
        )
        .await
    }

    /// Segment `segment`, made now unless it is on disk.
    pub async fn segment(&self, segment: usize) -> Result<Vec<u8>, Error> {
        let shared = &self.shared;
        if segment >= shared.plan.starts.len() {
            return Err(Error::new(ErrorCode::NotFound, format!("the film has no segment {segment}")));
        }
        self.until(
            |state| state.made.contains(&segment).then_some(()),
            |state| {
                let run = state.run.as_ref();
                // A run makes each segment once, in order, so one it passed is made anew.
                let reaching = run.is_some_and(|run| {
                    let at = run.made.map_or(run.from, |made| made + 1);
                    !run.over && segment >= at && segment <= at + NEAR
                });
                if reaching {
                    state.wanted = state.wanted.max(segment);
                } else {
                    shared.start(state, segment);
                }
            },
        )
        .await?;
        tokio::fs::read(shared.path(segment)).await.map_err(|e| {
            Error::new(ErrorCode::StorageError, format!("cannot read segment {segment} of the film for the TV: {e}"))
        })
    }

    /// Waits until `ready` says the state holds what is wanted. `ask` runs first, and again
    /// whenever a run ends without it.
    async fn until<T>(&self, ready: impl Fn(&State) -> Option<T>, ask: impl Fn(&mut State)) -> Result<T, Error> {
        let shared = &self.shared;
        let deadline = tokio::time::Instant::now() + MADE_WITHIN;
        let mut asked: Option<u64> = None;
        loop {
            let changed = shared.changed.notified();
            {
                let mut state = shared.state();
                if let Some(found) = ready(&state) {
                    return Ok(found);
                }
                match (&state.run, asked) {
                    (Some(run), Some(id)) if run.id == id && run.over => {
                        let why = run.failed.clone().unwrap_or_else(|| "it ended before the segment".into());
                        return Err(Error::internal(format!("ffmpeg cannot make the film for the TV: {why}")));
                    }
                    (Some(run), Some(id)) if run.id == id => {}
                    _ => {
                        ask(&mut state);
                        asked = state.run.as_ref().map(|run| run.id);
                    }
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(Error::new(ErrorCode::FetchFailed, "the film for the TV is taking too long to make")
                    .with_hint("the download may be slow; the TV asks again"));
            }
        }
    }
}

impl Drop for Film {
    fn drop(&mut self) {
        if let Some(run) = self.shared.state().run.take() {
            run.task.abort();
        }
        let _ = std::fs::remove_dir_all(&self.shared.dir);
    }
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn path(&self, segment: usize) -> PathBuf {
        self.dir.join(format!("{segment}.{SEGMENT}"))
    }

    /// Stops the run, if one goes, and starts one at `segment`.
    fn start(self: &Arc<Self>, state: &mut State, segment: usize) {
        if let Some(run) = state.run.take() {
            run.task.abort();
        }
        state.next += 1;
        let id = state.next;
        state.wanted = segment;
        let task = tokio::spawn(Arc::clone(self).run(id, segment));
        state.run = Some(Run { id, from: segment, made: None, task, failed: None, over: false });
        self.changed.notify_waiters();
    }

    /// One ffmpeg run from `from`, until the video ends or another run takes its place.
    async fn run(self: Arc<Self>, id: u64, from: usize) {
        let start = self.plan.starts[from];
        // Just past the keyframe, which a seek in the input lands on or before.
        let at = if from == 0 { 0.0 } else { start + 0.01 };
        let failed = match self.make(id, from, at).await {
            Ok(()) => None,
            Err(why) => {
                log(&format!("ffmpeg stopped making the film for the TV from {start:.1} s: {why}"));
                Some(why)
            }
        };
        let mut state = self.state();
        if let Some(run) = state.run.as_mut().filter(|run| run.id == id) {
            run.over = true;
            run.failed = failed;
        }
        drop(state);
        self.changed.notify_waiters();
    }

    async fn make(&self, id: u64, from: usize, at: f64) -> Result<(), String> {
        let mut child = tokio::process::Command::new(&self.program)
            .args((self.arguments)(at))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| crate::ffmpeg::cannot_run(&self.program, &e).message)?;
        let mut stdout = child.stdout.take().ok_or("ffmpeg gave no output")?;
        let mut stderr = child.stderr.take().ok_or("ffmpeg gave no errors")?;
        let complaints = tokio::spawn(async move {
            let (mut kept, mut buf) = (Vec::new(), [0; 1024]);
            while let Ok(read @ 1..) = stderr.read(&mut buf).await {
                if kept.len() < COMPLAINT {
                    kept.extend_from_slice(&buf[..read]);
                }
            }
            String::from_utf8_lossy(&kept).into_owned()
        });
        let mut cutter = Cutter::new(from);
        let mut buf = Vec::with_capacity(1 << 20);
        loop {
            self.until_wanted(id).await?;
            buf.reserve(1 << 16);
            let read = stdout.read_buf(&mut buf).await.map_err(|e| format!("cannot read ffmpeg's output: {e}"))?;
            if read == 0 {
                break;
            }
            let used = cutter.take(&buf, &self.plan).map_err(|e| e.to_string())?;
            buf.drain(..used);
            self.keep(id, &mut cutter).await?;
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        let complaint = crate::ffmpeg::complaint(&complaints.await.unwrap_or_default());
        if !status.success() {
            return Err(complaint);
        }
        cutter.finish();
        self.keep(id, &mut cutter).await
    }

    /// Waits while the run is `AHEAD` segments past what the TV asked for. Fails once another
    /// run has taken its place.
    async fn until_wanted(&self, id: u64) -> Result<(), String> {
        loop {
            let changed = self.changed.notified();
            {
                let state = self.state();
                let Some(run) = state.run.as_ref().filter(|run| run.id == id) else {
                    return Err("another run took its place".into());
                };
                if run.made.is_none_or(|made| made < state.wanted + AHEAD) {
                    return Ok(());
                }
            }
            changed.await;
        }
    }

    /// Writes the init segment and the segments `cutter` finished, and deletes the ones furthest
    /// from the TV's place past `KEPT`.
    async fn keep(&self, id: u64, cutter: &mut Cutter) -> Result<(), String> {
        let init = cutter.init.take();
        let finished = std::mem::take(&mut cutter.finished);
        if init.is_none() && finished.is_empty() {
            return Ok(());
        }
        for (segment, bytes) in &finished {
            tokio::fs::write(self.path(*segment), bytes)
                .await
                .map_err(|e| format!("cannot write segment {segment} to {}: {e}", self.dir.display()))?;
        }
        let mut state = self.state();
        if state.run.as_ref().is_none_or(|run| run.id != id) {
            return Err("another run took its place".into());
        }
        if let Some(init) = init {
            state.init.get_or_insert_with(|| Arc::new(init));
        }
        for (segment, _) in &finished {
            state.made.insert(*segment);
            if let Some(run) = state.run.as_mut() {
                run.made = Some(*segment);
            }
        }
        let wanted = state.wanted;
        while state.made.len() > KEPT {
            let far = state.made.iter().copied().max_by_key(|made| made.abs_diff(wanted)).expect("more than KEPT");
            state.made.remove(&far);
            let _ = std::fs::remove_file(self.path(far));
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }
}

/// Cuts a run's fragmented MP4 into the init segment and the playlist's segments.
struct Cutter {
    from: usize,
    init: Option<Vec<u8>>,
    /// The `ftyp` and `moov` read so far, until the `moov` is whole.
    head: Vec<u8>,
    tracks: Option<Tracks>,
    moof: Option<Vec<u8>>,
    current: Option<(usize, Vec<u8>)>,
    finished: Vec<(usize, Vec<u8>)>,
}

/// The video track's ID and timescale, from the `moov`.
#[derive(Clone, Copy, Debug)]
struct Tracks {
    video: u32,
    timescale: u32,
}

impl Cutter {
    fn new(from: usize) -> Self {
        Self { from, init: None, head: Vec::new(), tracks: None, moof: None, current: None, finished: Vec::new() }
    }

    /// Takes the whole top-level boxes at the front of `buf`, and returns how many bytes they
    /// were.
    fn take(&mut self, buf: &[u8], plan: &Plan) -> io::Result<usize> {
        let mut used = 0;
        while let Some((kind, length)) = top(&buf[used..]) {
            let Some(bytes) = buf.get(used..used + length) else { break };
            match &kind {
                b"ftyp" => self.head.extend_from_slice(bytes),
                b"moov" => {
                    self.head.extend_from_slice(bytes);
                    self.tracks = Some(tracks(&bytes[8..]).ok_or_else(|| bad("its moov names no video track"))?);
                    self.init = Some(std::mem::take(&mut self.head));
                }
                b"moof" => self.moof = Some(bytes.to_vec()),
                b"mdat" => {
                    let moof = self.moof.take().ok_or_else(|| bad("an mdat comes before its moof"))?;
                    let tracks = self.tracks.ok_or_else(|| bad("a fragment comes before the moov"))?;
                    let time = fragment_time(&moof[8..], tracks).ok_or_else(|| bad("a fragment has no video time"))?;
                    let segment = plan.segment_of(time);
                    // A seek in the input lands on the keyframe before, a segment early.
                    if segment >= self.from {
                        match &mut self.current {
                            Some((current, data)) if *current == segment => {
                                data.extend_from_slice(&moof);
                                data.extend_from_slice(bytes);
                            }
                            current => {
                                if let Some(done) = current.take() {
                                    self.finished.push(done);
                                }
                                *current = Some((segment, [moof.as_slice(), bytes].concat()));
                            }
                        }
                    }
                }
                _ => {}
            }
            used += length;
        }
        Ok(used)
    }

    /// The run reached the end of the video, so its last segment is whole.
    fn finish(&mut self) {
        if let Some(done) = self.current.take() {
            self.finished.push(done);
        }
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("ffmpeg wrote an MP4 newsreel cannot cut: {what}"))
}

/// The type and whole length of the box at the front of `data`, once its head is there.
fn top(data: &[u8]) -> Option<([u8; 4], usize)> {
    let head = data.get(..8)?;
    let kind = [head[4], head[5], head[6], head[7]];
    let length = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
        1 => usize::try_from(u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)).ok()?,
        length => length as usize,
    };
    (length >= 8).then_some((kind, length))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// The video track of a `moov`'s body.
fn tracks(moov: &[u8]) -> Option<Tracks> {
    boxes(moov).into_iter().filter(|(kind, _)| kind == b"trak").find_map(|(_, trak)| {
        let tkhd = child(trak, b"tkhd")?;
        let video = if tkhd.first() == Some(&1) { be32(tkhd, 20)? } else { be32(tkhd, 12)? };
        let mdia = child(trak, b"mdia")?;
        (child(mdia, b"hdlr")?.get(8..12)? == b"vide").then_some(())?;
        let mdhd = child(mdia, b"mdhd")?;
        let timescale = if mdhd.first() == Some(&1) { be32(mdhd, 20)? } else { be32(mdhd, 12)? };
        (timescale > 0).then_some(Tracks { video, timescale })
    })
}

/// When a fragment's video starts showing, in seconds: its decode time and its first sample's
/// composition offset.
fn fragment_time(moof: &[u8], tracks: Tracks) -> Option<f64> {
    let traf = boxes(moof)
        .into_iter()
        .filter(|(kind, _)| kind == b"traf")
        .map(|(_, traf)| traf)
        .find(|traf| child(traf, b"tfhd").and_then(|tfhd| be32(tfhd, 4)) == Some(tracks.video))?;
    let tfdt = child(traf, b"tfdt")?;
    let decode = if tfdt.first() == Some(&1) {
        u64::from_be_bytes(tfdt.get(4..12)?.try_into().ok()?)
    } else {
        u64::from(be32(tfdt, 4)?)
    };
    let trun = child(traf, b"trun")?;
    let version = *trun.first()?;
    let flags = be32(trun, 0)? & 0x00FF_FFFF;
    let mut at = 8 + if flags & 0x1 != 0 { 4 } else { 0 } + if flags & 0x4 != 0 { 4 } else { 0 };
    let offset = if flags & 0x800 != 0 {
        at += [0x100, 0x200, 0x400].iter().filter(|flag| flags & **flag != 0).count() * 4;
        let raw = be32(trun, at)?;
        if version == 0 { i64::from(raw) } else { i64::from(raw as i32) }
    } else {
        0
    };
    Some((decode as i64 + offset) as f64 / f64::from(tracks.timescale))
}

/// A download's file, read through the engine, which fetches the pieces a read needs first.
struct Engine<'a> {
    ctx: &'a Ctx,
    id: DownloadId,
    index: u32,
    file: StreamFile,
}

impl index::Bytes for Engine<'_> {
    fn size(&self) -> u64 {
        self.file.size
    }

    async fn read(&mut self, start: u64, end: u64) -> io::Result<Vec<u8>> {
        http::read(self.ctx, self.id, self.index, &self.file, start, end).await
    }
}

/// How a cast plays a file through ffmpeg, for a TV that cannot play the file as it is.
pub enum Converted {
    /// An HLS film, cut on the file's keyframes, or on its own for a video encoded again.
    Film(Film),
    /// A fragmented MP4 from a time on, for a file with no keyframe index newsreel reads. A TV
    /// seeks in it only as far as it has read, so the cast starts it again for any seek.
    Stream { program: PathBuf, arguments: Arguments },
}

/// Plans the conversion of file `index` of download `id` for a TV: the video copied, or encoded
/// as H.264 for `Picture::H264` and `Picture::Fit`, and the default audio as AAC unless it is AAC. `dir` holds the
/// film's segments, and a TV starts at `start_at` seconds.
pub async fn convert(
    ctx: &Ctx,
    id: DownloadId,
    index: u32,
    picture: Picture,
    dir: &Path,
    start_at: f64,
) -> Result<Converted, Error> {
    let program = ctx.media.ffmpeg.now()?.ffmpeg;
    let media = probe(ctx, id, index).await?;
    let file = ctx.owner.ask(|reply| Command::StreamFile { id, index, reply }).await?;
    let name = super::name(&file);
    if media.video.is_none() {
        return Err(Error::new(ErrorCode::NoSelection, format!("{name} has no video track")));
    }
    let url = http::url(ctx.http_port, id, index, &name);
    let paced = ctx.media.paces(&program).await;
    let duration = media.duration.unwrap_or(0.0);
    let plan = match picture {
        Picture::Copy => {
            let mut engine = Engine { ctx, id, index, file };
            match index::keyframes(&mut engine, &media.container).await {
                Ok(Some(keyframes)) => Plan::on(&keyframes, duration),
                Ok(None) => None,
                Err(e) => {
                    log(&format!("cannot read the keyframes of {name}, so it streams to the TV: {e}"));
                    None
                }
            }
        }
        Picture::H264 | Picture::Fit(..) => Plan::every(duration),
    };
    let forced = (picture != Picture::Copy).then(|| plan.as_ref().map(|plan| plan.starts.clone())).flatten();
    let arguments = stream_arguments(url, media, picture, paced, forced);
    Ok(match plan {
        Some(plan) => {
            let film = Film::new(plan, program, arguments, dir.to_path_buf(), start_at).map_err(|e| {
                Error::new(
                    ErrorCode::StorageError,
                    format!("cannot make {} for the film's segments: {e}", dir.display()),
                )
            })?;
            Converted::Film(film)
        }
        None => Converted::Stream { program, arguments },
    })
}

fn stream_arguments(
    url: String,
    media: Arc<Media>,
    picture: Picture,
    paced: bool,
    forced: Option<Vec<f64>>,
) -> Arguments {
    Box::new(move |t| {
        let mut args = ffmpeg::stream(&url, &media, t, None, picture, Encoder::of_host(), paced);
        if let Some(times) = &forced {
            ffmpeg::force_keyframes(&mut args, times);
        }
        ffmpeg::never_negative(&mut args);
        args
    })
}

#[cfg(test)]
mod tests;
