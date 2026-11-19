//! One play of a title: find its sources, try them one at a time best first, move past each that
//! will not start, clean up after it, and play the first that works.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::playback::stream::{self, Playable};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use newsreel_core::catalog::Episode;
use newsreel_core::filetype;
use newsreel_core::text::sanitize;
use newsreel_core::title::Span;
use newsreel_core::{
    AttemptEvent, Check, DownloadId, Error, ErrorCode, HexPrefix, Job, JobStatus, Opens, Player, Plays, Quality,
    Release, ResultId, Skip, SourceReport, TitleLink, Tried, VideoRef,
};
use tokio::sync::oneshot;

use super::super::BoxFuture;
use super::super::ctx::{Ctx, OwnerHandle};
use super::super::downloads::{self, Answer};
use super::super::layout::Layout;
use super::super::owner::plays::Destination;
use super::super::owner::{Command, Finish, Hold, PlayCommand, Resolved as Found, Source, TitlePlay};
use super::super::search_port::Progress;
use super::Resolved;
use crate::log::log;

const MOST_TRIED: usize = 6;
/// Sources after the one under test whose NZBs are fetched early, so stalled indexers in a row
/// cost one wait.
const LOOK_AHEAD: usize = 2;
/// How long a source whose NZB was asked for early still waits for it, however long ago that was.
const ANSWER_WITHIN: Duration = Duration::from_millis(100);
/// How often a trial looks for the first bytes of its video.
const LOOK_EVERY: Duration = Duration::from_millis(100);
/// How long a title's search runs before the best of its sources so far may go first. The quick
/// indexers have answered by then.
const FIRST_AFTER: Duration = Duration::from_millis(2500);

/// What a caller asks a title play for.
pub struct PlayTitle {
    pub video: VideoRef,
    /// Play only this release, with no fallback.
    pub pin: Option<ResultId>,
    /// The download that stalled. It is never tried, and once another source plays it is paused.
    pub replace: Option<DownloadId>,
    /// What the title plays in, which its sources are ranked for.
    pub player: Player,
    /// What the screen plays, which a source's file must suit.
    pub plays: Plays,
    /// Where its download goes. A title played into the plays folder downloads in playing order
    /// to be watched; one played into a folder downloads as `get` downloads, to be kept.
    pub to: Destination,
}

pub enum Outcome {
    Played(Box<Played>),
    Failed { error: Error, tried: Vec<Tried> },
}

/// The source a title plays from, and the sources tried before it.
pub struct Played {
    /// The film's name and year, or the series' name and the episode.
    pub name: String,
    pub video: VideoRef,
    pub release: Release,
    pub job: DownloadId,
    /// The index of the video in the download.
    pub file: u32,
    /// Seconds into the video where the person stopped, unless they finished it.
    pub resume_at: Option<f64>,
    /// The sources that did not play, in the order they were tried.
    pub tried: Vec<Tried>,
}

impl Outcome {
    /// The play's last step, which says how it ended.
    fn step(&self) -> AttemptEvent {
        match self {
            Outcome::Played(played) => AttemptEvent::Playing {
                video: played.video.clone(),
                release: played.release.clone(),
                job: played.job,
                file: played.file,
                resume_at: played.resume_at,
            },
            Outcome::Failed { error, tried } => AttemptEvent::Failed { error: error.clone(), tried: tried.clone() },
        }
    }
}

/// Reports `outcome` as the play's last step, and answers it.
fn ended(report: &mut impl FnMut(AttemptEvent), outcome: Outcome) -> Outcome {
    report(outcome.step());
    outcome
}

/// Plays `ask`, reports each step, the last being `playing` or `failed`, and answers how it ended.
/// A download here that holds the video plays first, before any search. Otherwise sources are
/// tried once the search so far holds one that could lead, while slower indexes go on answering,
/// and each next source is the best of what has been found by then. Dropping the future stops the
/// play and gives up the source under test.
pub async fn run(ctx: &Ctx, ask: PlayTitle, mut report: impl FnMut(AttemptEvent) + Send) -> Outcome {
    let end = Instant::now() + ctx.titles.timing.budget;
    let title = match ctx.titles.resolve(&ask.video, newsreel_core::time::now()).await {
        Ok(title) => title,
        Err(error) => return ended(&mut report, Outcome::Failed { error, tried: Vec::new() }),
    };
    report(AttemptEvent::Resolved {
        video: title.video.clone(),
        title: title.details.card.clone(),
        episode: title.episode.clone(),
    });
    let jobs = match ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await {
        Ok(jobs) => jobs,
        Err(error) => return ended(&mut report, Outcome::Failed { error, tried: Vec::new() }),
    };
    // The search result the stalled download was made from: never tried again.
    let replaced = ask.replace.and_then(|old| jobs.iter().find(|j| j.id == old)).map(super::held_as);
    if let Some(release) = replaced {
        ctx.titles.failed(release, Skip::Stalled);
    }
    let held: HashMap<ResultId, DownloadId> = jobs.iter().map(|job| (super::held_as(job), job.id)).collect();
    let mut walk = Walk { ctx, ask: &ask, title: &title, end, held, tried: Vec::new(), asked: HashMap::new() };
    let healthy: Vec<Job> = jobs.iter().filter(|j| !matches!(j.status, JobStatus::Error { .. })).cloned().collect();
    let ffmpeg = ask.plays.remux && ctx.media.ffmpeg.converts();
    let held = ctx.titles.line(&healthy, &title.video, Vec::new(), ask.player, ffmpeg);
    let reused = |r: &Release| ask.player.reuses(Quality::parse(&r.name), ffmpeg);
    if let Some(release) = held.into_iter().find(|r| r.failed.is_none() && ask.may_try(r, replaced) && reused(r))
        && let Some(outcome) = walk.try_source(release, 1, &mut report).await
    {
        return outcome;
    }

    let query = match ctx.titles.query(&title, (ctx.preferences)(), ask.player).await {
        Ok(query) => query,
        Err(error) => return ended(&mut report, Outcome::Failed { error, tried: walk.tried }),
    };
    let mut search = Searching {
        search: downloads::Searching::start(ctx, query),
        started: Instant::now(),
        releases: Vec::new(),
        sources: Vec::new(),
        error: None,
    };
    let line_up = |search: &Searching, tried: &[Tried]| {
        next_up(
            &ask,
            ctx.titles.line(&jobs, &title.video, search.releases.clone(), ask.player, ffmpeg),
            tried,
            replaced,
        )
    };
    loop {
        let ran = search.started.elapsed();
        let first = line_up(&search, &walk.tried).into_iter().next();
        if may_start(first.as_ref(), ran, search.over()) {
            break;
        }
        let first_after = tokio::time::Instant::from_std(search.started + FIRST_AFTER);
        tokio::select! {
            () = search.next() => {}
            () = tokio::time::sleep_until(first_after), if ran < FIRST_AFTER => {}
        }
    }
    if search.releases.is_empty()
        && let Some(error) = search.error.take()
    {
        return ended(&mut report, Outcome::Failed { error, tried: walk.tried });
    }
    // The sources as the title page lists them: a release posted twice under one name is one.
    let found = ctx.titles.line(&jobs, &title.video, search.releases.clone(), ask.player, ffmpeg).len();
    report(AttemptEvent::Searched { found: found as u32, sources: search.sources.clone() });
    let mut fetching: FuturesUnordered<BoxFuture<'_, ()>> = FuturesUnordered::new();
    while Instant::now() < end {
        let candidates = line_up(&search, &walk.tried);
        let Some(release) = candidates.first().cloned() else {
            if search.over() {
                break;
            }
            search.next().await;
            continue;
        };
        let of = (walk.tried.len() + candidates.len()) as u32;
        for next in walk.look_ahead(&candidates[1..]) {
            fetching.push(fetch_early(ctx, next, ctx.titles.timing.file_list));
        }
        let trial = walk.try_source(release, of, &mut report);
        tokio::pin!(trial);
        let over = loop {
            tokio::select! {
                over = &mut trial => break over,
                () = search.next() => {}
                Some(()) = fetching.next(), if !fetching.is_empty() => {}
            }
        };
        if let Some(outcome) = over {
            return outcome;
        }
    }
    let error = failure(&title.name(), &walk.tried, ask.pin.is_some(), ask.replace.is_some());
    ended(&mut report, Outcome::Failed { error, tried: walk.tried })
}

/// Whether a title play may stop waiting for its search and try `first`, the best of its sources
/// so far, `ran` into the search: once the quick indexers have answered, or the search is `over`.
fn may_start(first: Option<&Release>, ran: Duration, over: bool) -> bool {
    over || (first.is_some() && ran >= FIRST_AFTER)
}

/// A title's search as it runs, and the best of what it has found so far.
struct Searching<'a> {
    search: downloads::Searching<'a>,
    started: Instant,
    /// Ranked for playing here.
    releases: Vec<Release>,
    sources: Vec<SourceReport>,
    error: Option<Error>,
}

impl Searching<'_> {
    fn over(&self) -> bool {
        self.search.over()
    }

    /// Waits for the next source to answer, and keeps what the search has found by then. Once
    /// the search is over it never returns.
    async fn next(&mut self) {
        match self.search.next().await {
            Answer::SoFar(Progress { result, .. }) | Answer::Done(Ok(result)) => {
                self.releases = result.releases;
                self.sources = result.sources;
            }
            Answer::Done(Err(error)) => self.error = Some(error),
        }
    }
}

/// Where the person stopped `video`, unless they finished it. A place that cannot be read plays
/// from the beginning, and the log says why.
fn resume_at(ctx: &Ctx, video: &VideoRef) -> Option<f64> {
    ctx.titles.watching.resume_at(video).unwrap_or_else(|error| {
        log(&format!("{video} plays from the beginning, for where it stopped cannot be read: {error}"));
        None
    })
}

/// A title play's way through its sources.
struct Walk<'a> {
    ctx: &'a Ctx,
    ask: &'a PlayTitle,
    title: &'a Resolved,
    end: Instant,
    /// The downloads on this machine, by the search result each stands for.
    held: HashMap<ResultId, DownloadId>,
    /// The sources that did not play, in the order they were tried.
    tried: Vec<Tried>,
    /// When the play first asked for each source's NZB.
    asked: HashMap<ResultId, Instant>,
}

impl Walk<'_> {
    /// The first of `next` whose NZBs to fetch while another source is under test: those not
    /// asked for yet and not here already. Each counts as asked for from now.
    fn look_ahead(&mut self, next: &[Release]) -> Vec<Release> {
        let ahead: Vec<Release> = next
            .iter()
            .filter(|r| !self.asked.contains_key(&r.id) && !self.held.contains_key(&r.id))
            .take(LOOK_AHEAD)
            .cloned()
            .collect();
        let now = Instant::now();
        for release in &ahead {
            self.asked.insert(release.id, now);
        }
        ahead
    }

    /// Tries `release` as the next of `of` sources, and answers how the play ended once it is over:
    /// it plays, or the engine is going away. A source that does not play joins `tried`.
    async fn try_source(
        &mut self,
        release: Release,
        of: u32,
        report: &mut (impl FnMut(AttemptEvent) + Send),
    ) -> Option<Outcome> {
        let ctx = self.ctx;
        report(AttemptEvent::Trying { rank: self.tried.len() as u32 + 1, of, release: release.clone() });
        let asked = *self.asked.entry(release.id).or_insert_with(Instant::now);
        let mut trial = Trial { owner: &ctx.owner, hold: None };
        match trial.run(self, &release, asked, report).await {
            Ok((job, file)) => {
                let id = job.id;
                if let Err(error) = ctx.owner.ask(|reply| Command::Played { id, reply }).await {
                    return Some(ended(report, Outcome::Failed { error, tried: std::mem::take(&mut self.tried) }));
                }
                trial.hold = None;
                let video = self.title.video.clone();
                let resume_at = resume_at(ctx, &video);
                let tried = std::mem::take(&mut self.tried);
                let played = Played { name: self.title.name(), video, release, job: id, file, resume_at, tried };
                let outcome = ended(report, Outcome::Played(Box::new(played)));
                if let Some(old) = self.ask.replace.filter(|old| *old != id) {
                    let _ = ctx.owner.ask(|reply| Command::Pause { refs: vec![old.to_hex()], reply }).await;
                }
                Some(outcome)
            }
            Err(reason) => {
                let reason = *reason;
                trial.give_up().await;
                ctx.titles.failed(release.id, reason.clone());
                report(AttemptEvent::Skipped { release: release.id, reason: reason.clone() });
                self.tried.push(Tried { release: release.id, reason });
                None
            }
        }
    }
}

impl PlayTitle {
    /// Whether this play may try `release`: never the download it replaces, only the pinned
    /// release when one is, and never one whose name says a codec the screen does not decode, held
    /// here or not. `replaced` is the search result the replaced download was made from. A trial
    /// checks again once it has read the release's NZB.
    fn may_try(&self, release: &Release, replaced: Option<ResultId>) -> bool {
        replaced.is_none_or(|old| release.id != old)
            && self.pin.is_none_or(|pin| release.id == pin)
            && self.plays.decodes(Quality::parse(&release.name))
    }
}

/// The sources `ask` tries next, in the order of its `line`: those it may try and has not
/// `tried`, as many as it has tries left.
fn next_up(ask: &PlayTitle, line: Vec<Release>, tried: &[Tried], replaced: Option<ResultId>) -> Vec<Release> {
    let tried_ids: HashSet<ResultId> = tried.iter().map(|t| t.release).collect();
    let next = line.into_iter().filter(|r| !tried_ids.contains(&r.id) && ask.may_try(r, replaced));
    next.take(MOST_TRIED.saturating_sub(tried.len())).collect()
}

/// The source under test. Dropped before it plays, it gives the source up.
struct Trial<'a> {
    owner: &'a OwnerHandle,
    /// Its hold on the download, from before it asks for one until that download plays.
    hold: Option<Hold>,
}

impl Trial<'_> {
    /// Checks `release` step by step for `walk` and answers the download and the file that play.
    /// Its NZB has been on its way since `asked`.
    async fn run(
        &mut self,
        walk: &Walk<'_>,
        release: &Release,
        asked: Instant,
        report: &mut (impl FnMut(AttemptEvent) + Send),
    ) -> Result<(Job, u32), Box<Skip>> {
        let Walk { ctx, ask, title, end, .. } = *walk;
        let timing = ctx.titles.timing;
        report(AttemptEvent::Check { check: Check::FileList });
        let deadline = end.min(asked + timing.file_list).max(Instant::now() + ANSWER_WITHIN);
        let (source, layout) = files(ctx, release, walk.held.get(&release.id).copied(), asked, deadline).await?;
        // A search result names no download until its NZB is read.
        if ask.replace.is_some_and(|stalled| download_of(&source) == stalled) {
            return Err(Skip::Stalled.into());
        }
        let remux = ctx.media.ffmpeg.converts();
        let playable = video_of(&layout, title.video.episode, ask.plays, remux)?;
        // ffmpeg comes while the download starts, when the screen cannot read this file itself.
        if !ask.plays.opens(&layout.files[playable.video as usize].path, false) {
            let _ = ctx.media.ffmpeg.fetch();
        }

        report(AttemptEvent::Check { check: Check::Start });
        let span = release.span.unwrap_or_else(|| title.video.episode.map_or(Span::Film, Span::Episode));
        let link = TitleLink { title: title.video.title.clone(), span, release: Some(release.id) };
        let hold = Hold::fresh();
        // Held before the owner is asked, so a play that goes away at any point after gives it up.
        self.hold = Some(hold);
        let finish = match ask.to {
            Destination::Plays => Finish::Stream,
            Destination::Folder(_) => Finish::Keep,
        };
        let play = PlayCommand {
            source,
            to: ask.to.clone(),
            files: [playable.video].into_iter().chain(playable.subtitles.iter().copied()).collect(),
            setup_max_size: (ctx.preferences)().max_size,
            title: Some(TitlePlay { link, hold }),
            finish,
        };
        let (job, _) =
            ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await.map_err(
                |error| match error.code {
                    ErrorCode::TooLarge | ErrorCode::DiskFull => Skip::TooLarge { error },
                    _ => Skip::Failed { error },
                },
            )?;
        downloads::fetch_end(ctx, job.id, playable.video, layout.files[playable.video as usize].size);

        report(AttemptEvent::Check { check: Check::FirstBytes });
        let begun = Instant::now();
        let within = timing.first_bytes.min(end.saturating_duration_since(begun));
        let mut look = tokio::time::interval(LOOK_EVERY);
        loop {
            look.tick().await;
            let id = job.id;
            let now = match ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await {
                Ok(mut jobs) if !jobs.is_empty() => jobs.remove(0),
                Ok(_) => {
                    let error = Error::new(ErrorCode::NotFound, "the download is gone");
                    return Err(Skip::Failed { error }.into());
                }
                Err(error) => return Err(Skip::Failed { error }.into()),
            };
            let bytes = ctx
                .engine
                .ask(move |e| e.file_progress(id))
                .await
                .ok()
                .and_then(|f| f.get(playable.video as usize).copied());
            if let Some(outcome) = first_bytes(&now.status, bytes.unwrap_or(0), begun.elapsed(), within) {
                return outcome.map(|()| (now, playable.video)).map_err(Box::new);
            }
        }
    }

    /// Gives the source up, and waits until the owner has started to put its download back.
    async fn give_up(&mut self) {
        if let Some(hold) = self.hold.take() {
            let _ = self.owner.ask(|reply| Command::Release { hold, reply }).await;
        }
    }
}

impl Drop for Trial<'_> {
    fn drop(&mut self) {
        if let Some(hold) = self.hold.take() {
            self.owner.tell(Command::Release { hold, reply: oneshot::channel().0 });
        }
    }
}

/// The source of `release` and its file list: `held`, the download here it stands for, the
/// download its search result became, or its NZB, fetched and read by `deadline`.
async fn files(
    ctx: &Ctx,
    release: &Release,
    held: Option<DownloadId>,
    asked: Instant,
    deadline: Instant,
) -> Result<(Source, Arc<Layout>), Box<Skip>> {
    let failed = |error| Box::new(Skip::Failed { error });
    let found = match held {
        Some(id) => Found::Job(id),
        None => {
            let prefix: HexPrefix = release.id.to_ref().parse().map_err(failed)?;
            let results = vec![release.clone()];
            ctx.owner.ask(|reply| Command::Resolve { prefix, results, command: "play", reply }).await.map_err(failed)?
        }
    };
    let source = match found {
        Found::Job(id) => Source::Download(id),
        Found::Release { release, cached } => {
            tokio::time::timeout_at(deadline.into(), downloads::release_source(ctx, *release, cached))
                .await
                .map_err(|_| Box::new(Skip::NoFileList { waited_s: seconds(asked.elapsed()) }))?
                .map_err(failed)?
        }
    };
    match source {
        Source::Download(id) => {
            let info = downloads::download_files(ctx, id).await.map_err(failed)?;
            Ok((Source::Download(id), info.layout))
        }
        Source::Release { layout, origin } => {
            let shared = Arc::new(layout.clone());
            Ok((Source::Release { layout, origin }, shared))
        }
    }
}

/// Fetches and reads `release`'s NZB ahead of its trial, for at most `within`. The engine keeps
/// what arrives, so the trial finds it there.
fn fetch_early(ctx: &Ctx, release: Release, within: Duration) -> BoxFuture<'_, ()> {
    Box::pin(async move {
        let _ = tokio::time::timeout(within, downloads::release_source(ctx, release, None)).await;
    })
}

/// The download `source` is or would be: an NZB names its download.
fn download_of(source: &Source) -> DownloadId {
    match source {
        Source::Download(id) => *id,
        Source::Release { layout, .. } => layout.id,
    }
}

fn seconds(elapsed: Duration) -> u32 {
    elapsed.as_secs_f64().round() as u32
}

/// The file of the film, or of `episode` in a pack, and its subtitles, or why none plays on a
/// screen that `plays` them. A release that holds only sample clips holds no video.
fn video_of(layout: &Layout, episode: Option<Episode>, plays: Plays, remux: bool) -> Result<Playable, Box<Skip>> {
    let listing = layout.listing();
    let playable = match episode {
        Some(episode) => stream::episode(&listing, &sanitize(&layout.name), episode),
        None => stream::pick(&listing, plays),
    };
    let playable = playable.filter(|p| !stream::is_sample(&listing[p.video as usize].path)).ok_or(Skip::NoVideo)?;
    let path = &listing[playable.video as usize].path;
    if plays.opens(path, remux) {
        Ok(playable)
    } else {
        let container = filetype::extension(path);
        let skip = match plays.opens {
            Opens::Only(_) => Skip::NotOnTv { container },
            Opens::Browser | Opens::Any => Skip::Unplayable { container },
        };
        Err(skip.into())
    }
}

/// Whether a download under test has started: the video has bytes, or the download is done. A
/// download that fails, or that gets nothing `within` its start, will not do.
fn first_bytes(status: &JobStatus, video_bytes: u64, waited: Duration, within: Duration) -> Option<Result<(), Skip>> {
    match status {
        JobStatus::Done { .. } => Some(Ok(())),
        _ if video_bytes > 0 => Some(Ok(())),
        JobStatus::Error { error } => Some(Err(Skip::Failed { error: error.clone() })),
        _ if waited >= within => Some(Err(Skip::NoData { waited_s: seconds(waited) })),
        _ => None,
    }
}

/// Why no source played `name`.
fn failure(name: &str, tried: &[Tried], pinned: bool, replacing: bool) -> Error {
    let every = |skip: fn(&Skip) -> bool| tried.iter().all(|t| skip(&t.reason));
    if tried.is_empty() {
        return Error::new(ErrorCode::NoSource, format!("no source of {name} was found"))
            .with_hint("search Usenet for it by name");
    }
    if every(|s| matches!(s, Skip::Unplayable { .. })) {
        return Error::new(ErrorCode::NoSource, format!("no source of {name} plays in the browser without ffmpeg"))
            .with_hint(format!("{}, then play it again", crate::ffmpeg::INSTALL));
    }
    if every(|s| matches!(s, Skip::NotOnTv { .. })) {
        return Error::new(ErrorCode::NoSource, format!("no source of {name} comes in a container the TV plays"))
            .with_hint("play it here, or choose a source in MKV or MP4");
    }
    if every(|s| matches!(s, Skip::TooLarge { .. }))
        && let Some(Skip::TooLarge { error }) = tried.first().map(|t| &t.reason)
    {
        return error.clone();
    }
    let (message, hint) = match (pinned, replacing) {
        (true, _) => (format!("this source of {name} does not play now"), "let newsreel choose a source"),
        (false, true) => {
            (format!("no other source of {name} plays now"), "keep waiting on this one, or try again later")
        }
        (false, false) => (format!("no source of {name} plays now"), "try again later, or choose a source"),
    };
    Error::new(ErrorCode::NoSource, message).with_hint(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::daemon::fake;

    fn release(n: u8) -> Release {
        Release {
            id: ResultId::from_bytes([n; 20]),
            name: format!("release {n}"),
            size: None,
            grabs: None,
            published: None,
            category: None,
            flags: BTreeSet::new(),
            sources: Vec::new(),
            nzb_url: None,
            span: None,
            failed: None,
        }
    }

    fn numbers(releases: Vec<Release>) -> Vec<u8> {
        releases.iter().map(|r| r.name.trim_start_matches("release ").parse::<u8>().unwrap()).collect()
    }

    #[test]
    fn a_play_tries_at_most_six_sources_never_the_one_it_replaces_and_with_a_pin_only_that() {
        let ask = |replace: Option<u8>, pin: Option<u8>| PlayTitle {
            video: "tt0063350".parse().unwrap(),
            pin: pin.map(|n| release(n).id),
            replace: replace.map(|n| DownloadId::from_bytes([n; 20])),
            player: Player::External,
            plays: Plays::BROWSER,
            to: Destination::Plays,
        };
        let line: Vec<Release> = (1..=12).map(release).collect();
        type Row<'a> = (Option<u8>, Option<u8>, &'a [u8], &'a [u8]);
        let table: [Row; 5] = [
            (None, None, &[], &[1, 2, 3, 4, 5, 6]),
            (None, None, &[1, 4], &[2, 3, 5, 6]),
            (Some(2), None, &[], &[1, 3, 4, 5, 6, 7]),
            (None, Some(9), &[], &[9]),
            (None, Some(9), &[9], &[]),
        ];
        for (replace, pin, tried, expected) in table {
            let tried: Vec<Tried> =
                tried.iter().map(|n| Tried { release: release(*n).id, reason: Skip::NoVideo }).collect();
            let replaced = replace.map(|n| release(n).id);
            let next = numbers(next_up(&ask(replace, pin), line.clone(), &tried, replaced));
            assert_eq!(next, expected, "replacing {replace:?}, pinned to {pin:?}, {} tried", tried.len());
        }
    }

    #[test]
    fn a_play_starts_before_its_search_ends_once_the_quick_indexers_have_answered() {
        let first = release(1);
        let ms = Duration::from_millis;
        let table = [
            (Some(&first), ms(0), false, false),
            (Some(&first), ms(2499), false, false),
            (Some(&first), ms(2500), false, true),
            (Some(&first), ms(10), true, true),
            (None, ms(7000), false, false),
            (None, ms(10), true, true),
        ];
        for (n, (first, ran, over, expected)) in table.into_iter().enumerate() {
            assert_eq!(may_start(first, ran, over), expected, "row {n}");
        }
    }

    fn release_named(name: &str, files: &[(&str, u64)]) -> Layout {
        let files = files.iter().map(|(path, size)| (path.to_string(), *size)).collect();
        fake::Posting { id: DownloadId::from_bytes([7; 20]), name: name.into(), files }.layout().unwrap()
    }

    #[test]
    fn the_video_is_the_film_or_the_episode_never_a_sample_and_a_browser_without_ffmpeg_opens_only_its_containers() {
        let film = release_named("Night", &[("Night.mp4", 900), ("Night.en.srt", 9), ("Sample.mp4", 99)]);
        let pack = release_named(
            "Harbor.Lights.S01.1080p",
            &[("Harbor.Lights.S01E01.mkv", 900), ("Harbor.Lights.S01E02.mp4", 900), ("Harbor.Lights.S01E02.srt", 9)],
        );
        let disc = release_named("Night DVD", &[("NIGHT.iso", 900), ("readme.txt", 9)]);
        let samples = release_named("Night", &[("Sample/Night.sample.mp4", 90), ("sample-night.mkv", 80)]);
        let episode = |season, episode| Some(Episode { season, episode });
        let read = |m: &Layout, e, remux| video_of(m, e, Plays::BROWSER, remux).map(|p| (p.video, p.subtitles));
        let table = [
            (read(&film, None, false), Ok((0, vec![1]))),
            (read(&pack, episode(1, 2), false), Ok((1, vec![2]))),
            (read(&pack, episode(1, 1), true), Ok((0, vec![2]))),
            (read(&pack, episode(1, 1), false), Err(Box::new(Skip::Unplayable { container: "mkv".into() }))),
            (read(&pack, episode(1, 3), true), Err(Box::new(Skip::NoVideo))),
            (read(&disc, None, true), Err(Box::new(Skip::NoVideo))),
            (read(&samples, None, true), Err(Box::new(Skip::NoVideo))),
        ];
        for (n, (got, expected)) in table.into_iter().enumerate() {
            assert_eq!(got, expected, "row {n}");
        }
        let paths = ["a.mp4", "a.M4V", "a.mov", "a.webm", "a.mkv", "a.avi", "mp4"];
        let open = |plays: Plays, remux| paths.map(|p| plays.opens(p, remux));
        assert_eq!(open(Plays::BROWSER, false), [true, true, true, true, false, false, false]);
        assert_eq!(open(Plays::BROWSER, true), [true; 7]);
        assert_eq!(open(Plays::ANY, false), [true; 7]);
    }

    #[test]
    fn a_download_under_test_starts_with_its_first_bytes_and_not_later_than_its_deadline() {
        let within = Duration::from_secs(30);
        let error = Error::new(ErrorCode::StorageError, "cannot write");
        let downloading = JobStatus::Downloading;
        let table = [
            (JobStatus::Done { finished_at: 1 }, 0, 0, Some(Ok(()))),
            (downloading.clone(), 1, 0, Some(Ok(()))),
            (downloading.clone(), 0, 29, None),
            (JobStatus::Checking, 0, 29, None),
            (downloading.clone(), 0, 30, Some(Err(Skip::NoData { waited_s: 30 }))),
            (JobStatus::Error { error: error.clone() }, 0, 1, Some(Err(Skip::Failed { error }))),
        ];
        for (status, bytes, waited, expected) in table {
            assert_eq!(first_bytes(&status, bytes, Duration::from_secs(waited), within), expected, "{status:?}");
        }
    }

    #[test]
    fn a_failed_play_says_why_no_source_played() {
        let tried = |reason: Skip| Tried { release: release(1).id, reason };
        let too_large = Error::new(ErrorCode::DiskFull, "not enough space");
        let read = |tried: &[Tried], pinned, replacing| {
            let e = failure("Night of the Living Dead (1968)", tried, pinned, replacing);
            (e.code, e.message)
        };
        let no_source = |message: &str| (ErrorCode::NoSource, message.to_string());
        assert_eq!(read(&[], false, false), no_source("no source of Night of the Living Dead (1968) was found"));
        assert_eq!(
            read(&[tried(Skip::Unplayable { container: "mkv".into() })], false, false),
            no_source("no source of Night of the Living Dead (1968) plays in the browser without ffmpeg")
        );
        assert_eq!(
            read(&[tried(Skip::TooLarge { error: too_large.clone() })], false, false),
            (ErrorCode::DiskFull, "not enough space".into())
        );
        let mixed = [tried(Skip::NoVideo), tried(Skip::NoData { waited_s: 30 })];
        assert_eq!(read(&mixed, false, false), no_source("no source of Night of the Living Dead (1968) plays now"));
        assert_eq!(
            read(&mixed[..1], true, false),
            no_source("this source of Night of the Living Dead (1968) does not play now")
        );
        assert_eq!(
            read(&mixed, false, true),
            no_source("no other source of Night of the Living Dead (1968) plays now")
        );
    }
}
