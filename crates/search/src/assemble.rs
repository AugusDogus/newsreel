use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use newsreel_core::error::Details;
use newsreel_core::title::{Namesakes, Wanted};
use newsreel_core::{
    Arch, Error, ErrorCode, Flag, Language, Profile, Quality, Release, Resolution, ResultId, SearchQuery, SearchResult,
    SourceOutcome, SourceReport, TitleId,
};
use sha1::{Digest, Sha1};

use crate::classify;
use crate::observation::Observation;
use crate::rank::{self, FAKE_MBPS};
use crate::sources::{Failure, SourceMeta};
use crate::terms::{Relevance, Terms};
use crate::trash;

#[derive(Clone)]
pub(crate) struct Run {
    pub source: Arc<SourceMeta>,
    pub answer: Answer,
    pub elapsed_ms: u32,
}

#[derive(Clone)]
pub(crate) enum Answer {
    Fresh(Arc<Vec<Observation>>),
    Cached(Arc<Vec<Observation>>),
    Failed(Failure),
}

pub(crate) fn vet(terms: &Terms, rows: Vec<Observation>) -> Result<Vec<Observation>, Failure> {
    let total = rows.len();
    let kept: Vec<Observation> = rows.into_iter().filter(|o| terms.is_relevant(o.name())).collect();
    if (total - kept.len()) * 2 > total { Err(Failure::Irrelevant) } else { Ok(kept) }
}

/// A release and the indexer whose link it carries, which saves the link without its key.
pub(crate) type Linked = (Release, String);

pub(crate) fn assemble(
    query: &SearchQuery,
    profile: Profile,
    terms: &Terms,
    runs: &[Run],
    elapsed_ms: u32,
) -> Result<(SearchResult, Vec<Linked>), Error> {
    let (result, linked) = merged(query, profile, terms, runs, elapsed_ms);
    if !result.sources.iter().any(|r| r.outcome.is_ok()) {
        return Err(all_failed(result.sources));
    }
    Ok((result, linked))
}

pub(crate) fn merged(
    query: &SearchQuery,
    profile: Profile,
    terms: &Terms,
    runs: &[Run],
    elapsed_ms: u32,
) -> (SearchResult, Vec<Linked>) {
    let mut runs: Vec<&Run> = runs.iter().collect();
    runs.sort_by(|a, b| (a.source.authority, &a.source.name).cmp(&(b.source.authority, &b.source.name)));
    let sources: Vec<SourceReport> = runs.chunk_by(|a, b| a.source.name == b.source.name).map(report).collect();
    let sightings = runs.iter().flat_map(|run| {
        let rows: &[Observation] = match &run.answer {
            Answer::Fresh(rows) | Answer::Cached(rows) => rows,
            Answer::Failed(_) => &[],
        };
        rows.iter().map(|row| (&*run.source, row))
    });
    let (mut kept, hidden_adult) = filter(merge(sightings, terms), query);
    rank::rank(&mut kept, query.sort, &profile);
    let total = u32::try_from(kept.len()).unwrap_or(u32::MAX);
    kept.truncate(query.limit.into());
    let releases = kept.iter().map(Candidate::shown).collect();
    let query = SearchQuery { profile, ..query.clone() };
    let result = SearchResult { query, releases, total, hidden_adult, sources, elapsed_ms };
    (result, kept.into_iter().map(|c| (c.release, c.source)).collect())
}

/// One source's report over its runs, one for each of a title's queries.
fn report(runs: &[&Run]) -> SourceReport {
    let count = |rows: &Vec<Observation>| u32::try_from(rows.len()).unwrap_or(u32::MAX);
    let outcome = |run: &Run| match &run.answer {
        Answer::Fresh(rows) => SourceOutcome::Ok { results: count(rows) },
        Answer::Cached(rows) => SourceOutcome::Cached { results: count(rows) },
        Answer::Failed(failure) => failure.outcome(),
    };
    let first = runs[0];
    let outcome = runs[1..].iter().fold(outcome(first), |sum, run| joined(sum, outcome(run)));
    let elapsed_ms = runs.iter().map(|r| r.elapsed_ms).max().unwrap_or(first.elapsed_ms);
    SourceReport { source: first.source.name.clone(), outcome, elapsed_ms }
}

/// Two runs of one source as one outcome. Their rows add up, fresh wins over cached, and an
/// answer wins over a failure. Of two failures, the first says why.
fn joined(first: SourceOutcome, then: SourceOutcome) -> SourceOutcome {
    match (first, then) {
        (SourceOutcome::Cached { results: a }, SourceOutcome::Cached { results: b }) => {
            SourceOutcome::Cached { results: a.saturating_add(b) }
        }
        (
            SourceOutcome::Ok { results: a } | SourceOutcome::Cached { results: a },
            SourceOutcome::Ok { results: b } | SourceOutcome::Cached { results: b },
        ) => SourceOutcome::Ok { results: a.saturating_add(b) },
        (failed, answered) if !failed.is_ok() && answered.is_ok() => answered,
        (first, _) => first,
    }
}

fn all_failed(sources: Vec<SourceReport>) -> Error {
    let names: Vec<&str> = sources.iter().map(|r| r.source.as_str()).collect();
    Error::new(ErrorCode::ProvidersUnavailable, format!("all sources failed ({})", names.join(", ")))
        .with_hint("check the network connection, or retry with a longer --timeout")
        .with_details(Details::Sources(sources))
}

pub(crate) struct Candidate {
    pub release: Release,
    /// The indexer whose link the release carries.
    source: String,
    pub(crate) relevance: Relevance,
    pub(crate) extra_words: u16,
    /// A trailer, a clip, or another extra, by a word the query does not hold.
    extra: bool,
    pub(crate) quality: Quality,
    pub(crate) resolution: Option<Resolution>,
    pub(crate) arches: Vec<Arch>,
    pub(crate) languages: Vec<Language>,
    /// How high TRaSH Guides ranks the group, when the release is of the query's title.
    pub(crate) tier: u8,
    /// Set only in a title's search, for a release of the wanted title.
    pub(crate) title: Option<TitleFit>,
    /// The title the indexers tagged the release with.
    imdb: Option<TitleId>,
}

/// How a release of the wanted title plays here. Its span is in `release.span`.
pub(crate) struct TitleFit {
    /// Other titles share the name and this release names no year.
    pub(crate) undated: bool,
    /// Mbit/s per film or episode. `None` when the size or the runtime is unknown.
    pub(crate) mbps: Option<f64>,
    /// Words in the name beyond the title, its year, and tags, as an alternate version has.
    pub(crate) other_words: usize,
}

impl Candidate {
    /// The release as output shows it: NZB links carry the user's API key, so they stay inside.
    fn shown(&self) -> Release {
        Release { nzb_url: None, ..self.release.clone() }
    }
}

type Sighting<'a> = (&'a SourceMeta, &'a Observation);

/// The ID of the search result an NZB link names.
pub(crate) fn result_id(nzb_url: &str) -> ResultId {
    ResultId::from_bytes(Sha1::digest(nzb_url.as_bytes()).into())
}

fn merge<'a>(sightings: impl Iterator<Item = Sighting<'a>>, terms: &Terms) -> Vec<Candidate> {
    let mut groups: HashMap<ResultId, Vec<Sighting<'a>>> = HashMap::new();
    for sighting in sightings {
        groups.entry(result_id(&sighting.1.nzb_url)).or_default().push(sighting);
    }
    groups.into_iter().map(|(id, group)| combine(id, &group, terms)).collect()
}

fn combine(id: ResultId, group: &[Sighting<'_>], terms: &Terms) -> Candidate {
    fn first<T: Ord + Clone>(group: &[Sighting<'_>], field: impl Fn(&Observation) -> Option<T>) -> Option<T> {
        group.iter().filter_map(|(s, o)| field(o).map(|v| (s.authority, v))).min().map(|(_, v)| v)
    }
    let name = group
        .iter()
        .map(|(s, o)| {
            // Indexers cut long titles and mark the cut with an ellipsis, which
            // makes the cut name longer than the whole one.
            let name = o.name();
            let whole = name.strip_suffix("...").or_else(|| name.strip_suffix('…')).map(str::trim_end);
            (std::cmp::Reverse(whole.unwrap_or(name).chars().count()), whole.is_some(), s.authority, name)
        })
        .min()
        .map(|(.., name)| name.to_string())
        .unwrap_or_default();
    let category = first(group, |o| o.category);
    let mut flags: BTreeSet<Flag> = group.iter().flat_map(|(_, o)| o.flags.iter().copied()).collect();
    if group.iter().any(|(_, o)| classify::is_cam(o.name(), category)) {
        flags.insert(Flag::Cam);
    }
    if group.iter().any(|(_, o)| classify::is_adult(o.name())) {
        flags.insert(Flag::Adult);
    }
    let standing = trash::standing(&name);
    if standing.is_avoid() {
        flags.insert(Flag::Avoid);
    }
    // A ranked group's release of another title is no better a match for it.
    let tier = if terms.is_title_of(&name) { standing.tier_rank() } else { 0 };
    let mut authorities: Vec<(u16, &str)> = group.iter().map(|(s, _)| (s.authority, s.name.as_str())).collect();
    authorities.sort();
    authorities.dedup();
    let (_, source, nzb_url) = group
        .iter()
        .map(|(s, o)| (s.authority, &s.name, &o.nzb_url))
        .min()
        .expect("a group holds at least one sighting");
    let relevance = group.iter().map(|(_, o)| terms.relevance(o.name())).max().unwrap_or(terms.relevance(""));
    let extra_words = terms.extra_words(&name);
    let quality = Quality::parse(&name);
    let size = first(group, |o| o.size);
    let extra = classify::is_extra(&name, terms);
    let resolution = quality.resolution.filter(|r| !extra && !rank::is_too_small(size, *r));
    let arches = Arch::named_in(&name);
    let languages = Language::named_in(&name, terms.words());
    Candidate {
        source: source.clone(),
        release: Release {
            id,
            name,
            size,
            grabs: group.iter().filter_map(|(_, o)| o.grabs).max(),
            published: first(group, |o| o.published),
            category,
            flags,
            sources: authorities.into_iter().map(|(_, name)| name.to_string()).collect(),
            nzb_url: Some(nzb_url.clone()),
            span: None,
            failed: None,
        },
        relevance,
        extra_words,
        extra,
        quality,
        resolution,
        arches,
        languages,
        title: None,
        tier,
        imdb: first(group, |o| o.imdb.clone()),
    }
}

fn filter(candidates: Vec<Candidate>, q: &SearchQuery) -> (Vec<Candidate>, u32) {
    let mut hidden_adult = 0;
    let kept = candidates
        .into_iter()
        .filter(|c| {
            let r = &c.release;
            q.category.is_none_or(|want| r.category == Some(want))
                && q.min_grabs.is_none_or(|min| r.grabs.is_some_and(|g| g >= min))
                && q.max_size.is_none_or(|max| r.size.is_some_and(|s| s <= max))
        })
        .filter_map(|c| match &q.wanted {
            Some(wanted) => fitted(c, wanted),
            None => Some(c),
        })
        .filter(|c| {
            let hide = !q.adult && c.release.flags.contains(&Flag::Adult);
            hidden_adult += u32::from(hide);
            !hide
        })
        .collect();
    (kept, hidden_adult)
}

/// The candidate as a release of the wanted title, or `None` when it is another title or an
/// extra such as its trailer, or when its bitrate says it is a fake.
fn fitted(mut c: Candidate, wanted: &Wanted) -> Option<Candidate> {
    if c.extra {
        return None;
    }
    // A tag is the indexer's word on which title a release is, so another title's is not this one.
    let reading = match &c.imdb {
        Some(id) if *id == wanted.id => wanted.fit_tagged(&c.release.name)?,
        Some(_) => return None,
        None => wanted.fit(&c.release.name)?,
    };
    let bitrate = rank::bitrate(c.release.size, reading.span, wanted);
    if bitrate.is_some_and(|mbps| mbps < FAKE_MBPS) {
        return None;
    }
    c.release.span = Some(reading.span);
    // The fit already says it is the wanted title, so a ranked group counts whatever the name.
    c.tier = trash::standing(&c.release.name).tier_rank();
    c.title = Some(TitleFit {
        undated: wanted.namesakes == Namesakes::Leading && reading.year.is_none(),
        mbps: bitrate,
        other_words: wanted.other_words(&c.release.name),
    });
    Some(c)
}

#[cfg(test)]
mod tests;
