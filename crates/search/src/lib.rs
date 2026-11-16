//! Turns a [`SearchQuery`] into a [`SearchResult`]: every selected indexer runs at
//! once under the query's deadline, rows merge by NZB link, and one ranking orders
//! them, so a human and an agent see the same list.
//!
//! The engine process owns one [`Search`] and one [`ResultCache`]. Search never
//! starts a download.

mod assemble;
mod cache;
pub mod catalog;
mod classify;
mod dates;
mod http;
mod imdb;
mod observation;
mod rank;
mod sources;
mod store;
pub mod subtitles;
mod terms;
mod trash;

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use newsreel_core::{Error, ErrorCode, Profile, Release, SearchQuery, SearchResult, text};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, timeout_at};
use url::Url;

use crate::assemble::{Answer, Linked, Run, assemble, merged, vet};
use crate::cache::AnswerKey;
pub use crate::cache::ResultCache;
use crate::http::{FetchError, Http, Retry};
pub use crate::sources::SavedLink;
use crate::sources::{Ask, Ctx, Failure, Source};
use crate::terms::Terms;

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Newznab indexers, searched in this order.
    pub indexers: Vec<IndexerConfig>,
    /// Sources never searched, by name. A query that names one fails.
    pub disabled: Vec<String>,
}

/// A Newznab endpoint: NZBHydra, Prowlarr, or an indexer's own API, such as
/// `http://127.0.0.1:5076/api`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerConfig {
    /// The `--source` name: lowercase letters, digits, `-`, `_`, and `.`.
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl fmt::Debug for IndexerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexerConfig")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct SourceInfo {
    pub name: String,
}

/// A search under way, with answers merged and ranked like the final result.
/// Pending sources follow the query's source order, or authority order when it
/// names none.
#[derive(Clone, PartialEq, Debug)]
pub struct Progress {
    pub result: SearchResult,
    pub pending: Vec<String>,
}

#[derive(Clone)]
pub struct Search {
    inner: Arc<Inner>,
}

struct Inner {
    http: Http,
    sources: Vec<Arc<Source>>,
    disabled: Vec<String>,
    cache: ResultCache,
}

impl Search {
    /// Fails with `usage` when the config names an unknown source or a bad URL.
    pub fn new(config: &SearchConfig, cache: ResultCache) -> Result<Search, Error> {
        let http = Http::new().map_err(no_client)?;
        Inner::new(config, cache, http).map(|inner| Search { inner: Arc::new(inner) })
    }

    /// The results this search keeps for later lookups by ID.
    pub fn cache(&self) -> &ResultCache {
        &self.inner.cache
    }

    /// Every enabled source, most authoritative first.
    pub fn sources(&self) -> Vec<SourceInfo> {
        self.inner.sources.iter().map(|s| SourceInfo { name: s.meta.name.clone() }).collect()
    }

    /// Returns by the query's deadline, ranked for the query's profile. A source that has
    /// not answered by then is reported as `timeout`. Fails with `providers_unavailable`
    /// when no source answered, and with `usage` for a query without words or an unknown
    /// source.
    pub async fn search(&self, query: &SearchQuery) -> Result<SearchResult, Error> {
        self.search_with_progress(query, || query.profile.clone(), |_| {}).await
    }

    /// Like [`Search::search`], ranked for what `rank_for` returns each time results
    /// change, so a profile that becomes known during the search applies. Calls
    /// `progress` each time sources answer while others are still pending. Answers that
    /// arrive together make one call. A query with a wanted title searches each of the
    /// title's queries at every source, and a source is pending until all have answered.
    pub async fn search_with_progress(
        &self,
        query: &SearchQuery,
        rank_for: impl Fn() -> Profile + Send,
        mut progress: impl FnMut(Progress) + Send,
    ) -> Result<SearchResult, Error> {
        let started = Instant::now();
        let texts: Vec<Terms> = match &query.wanted {
            Some(wanted) => wanted.queries().iter().map(|text| Terms::parse(text)).collect(),
            None => vec![Terms::parse(&query.text)],
        };
        if texts.iter().any(Terms::is_empty) {
            return Err(
                Error::new(ErrorCode::Usage, "a search needs at least one word").with_hint("newsreel search <words>")
            );
        }
        // The first query's words weigh relevance, since it names the title most fully.
        let terms = &texts[0].clone();
        // A film is asked for by its ID too, which finds the releases that name it otherwise.
        // Episodes are tagged with other IDs than IMDb's, so a series is asked for by words.
        let film = query.wanted.as_ref().filter(|w| w.episode.is_none()).map(|w| Ask::Film(w.id.clone()));
        let texts: Vec<Ask> = film.into_iter().chain(texts.into_iter().map(Ask::Words)).collect();
        let deadline = started + Duration::from_millis(query.timeout_ms.into());
        let cache = &self.inner.cache;
        let mut selected = self.select(&query.sources)?;
        if query.wanted.is_some() {
            selected.retain(|source| source.video);
        }
        let lookups = selected.iter().flat_map(|source| texts.iter().map(move |terms| (source, terms))).enumerate();
        let lookups = lookups.map(|(index, (source, terms))| {
            let key = AnswerKey {
                source: source.meta.name.clone(),
                scope: source.scope(),
                text: terms.key(),
                adult: query.adult,
            };
            let cached = if query.fresh { None } else { cache.answer(&key, std::time::Instant::now()) };
            async move {
                if let Some(rows) = cached {
                    return (index, Run { source: source.meta.clone(), answer: Answer::Cached(rows), elapsed_ms: 0 });
                }
                let answer = AssertUnwindSafe(self.inner.run(source, terms, deadline))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Answer::Failed(Failure::BadResponse("the source adapter crashed".into())));
                if let Answer::Fresh(rows) = &answer {
                    cache.store(key, rows.clone(), std::time::Instant::now());
                }
                (index, Run { source: source.meta.clone(), answer, elapsed_ms: millis(started.elapsed()) })
            }
        });
        let mut lookups: FuturesUnordered<_> = lookups.collect();
        // By source, then by text, so runs merge in the same order however they finish.
        let mut runs: Vec<Option<Run>> = vec![None; selected.len() * texts.len()];
        while let Some((index, run)) = lookups.next().await {
            runs[index] = Some(run);
            while let Some(Some((index, run))) = lookups.next().now_or_never() {
                runs[index] = Some(run);
            }
            if !lookups.is_empty() {
                let answered: Vec<Run> = runs.iter().flatten().cloned().collect();
                let (result, linked) = merged(query, rank_for(), terms, &answered, millis(started.elapsed()));
                cache.remember(self.inner.saved_links(linked));
                let pending = selected
                    .iter()
                    .zip(runs.chunks(texts.len()))
                    .filter(|(_, runs)| runs.iter().any(Option::is_none))
                    .map(|(s, _)| s.meta.name.clone())
                    .collect();
                progress(Progress { result, pending });
            }
        }
        let runs: Vec<Run> = runs.into_iter().flatten().collect();
        let (result, linked) = assemble(query, rank_for(), terms, &runs, millis(started.elapsed()))?;
        cache.remember(self.inner.saved_links(linked));
        Ok(result)
    }

    /// The NZB link to download `release` from: its own, or the one an earlier engine process
    /// saved for it, with the current API key of the indexer that issued it. Fails with `usage`
    /// when that link needs a key and its indexer is gone or has none.
    pub fn nzb_url(&self, release: &Release) -> Result<Option<String>, Error> {
        if release.nzb_url.is_some() {
            return Ok(release.nzb_url.clone());
        }
        let Some(saved) = self.inner.cache.saved_link(&release.id) else { return Ok(None) };
        if saved.key_params.is_empty() {
            return Ok(Some(saved.url));
        }
        let name = &saved.source;
        let needs_key = |why: &str, hint: String| {
            Error::new(
                ErrorCode::Usage,
                format!("this result's NZB link needs an API key for the {name} indexer, which {why}"),
            )
            .with_hint(format!("{hint} and run newsreel engine stop, or search again"))
        };
        let Some(source) = self.inner.sources.iter().find(|s| s.meta.name == *name) else {
            return Err(if self.inner.disabled.contains(name) {
                needs_key("is disabled", format!("remove {name} from sources.disabled in the newsreel config"))
            } else {
                needs_key("is not configured", format!("add {name} back to sources.newznab in the newsreel config"))
            });
        };
        let url = source
            .link(&saved)
            .ok_or_else(|| needs_key("has none", format!("set api_key_env for {name} in the newsreel config")))?;
        Ok(Some(url))
    }

    /// The NZB at `url`, which carries the user's API key, so errors never quote it.
    pub async fn fetch_nzb(&self, url: &str) -> Result<Vec<u8>, Error> {
        // An NZB for an 80 GB release is about 15 MB.
        const CAP: usize = 64 << 20;
        const WITHIN: Duration = Duration::from_secs(20);
        let url = Url::parse(url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| Error::new(ErrorCode::InvalidRef, "an NZB URL must start with http:// or https://"))?;
        // An indexer counts each request as a grab.
        let body = self.inner.http.fetch(&url, CAP, Instant::now() + WITHIN, Retry::Never).await;
        body.map_err(|e| {
            Error::new(ErrorCode::FetchFailed, format!("could not fetch the NZB: {}", fetch_reason(e)))
                .with_hint("check the network connection and the indexer, or search again")
        })
    }

    fn select(&self, names: &[String]) -> Result<Vec<Arc<Source>>, Error> {
        let all = &self.inner.sources;
        if names.is_empty() {
            return Ok(all.clone());
        }
        let mut chosen: Vec<Arc<Source>> = Vec::new();
        for name in names {
            let Some(source) = all.iter().find(|s| s.meta.name.eq_ignore_ascii_case(name.trim())) else {
                if let Some(off) = self.inner.disabled.iter().find(|d| d.eq_ignore_ascii_case(name.trim())) {
                    return Err(Error::new(ErrorCode::Usage, format!("source {off:?} is disabled"))
                        .with_hint("remove it from sources.disabled in the newsreel config"));
                }
                let known: Vec<&str> = all.iter().map(|s| s.meta.name.as_str()).collect();
                return Err(Error::new(ErrorCode::Usage, format!("unknown source {name:?}"))
                    .with_hint(format!("available sources: {}", known.join(", "))));
            };
            if !chosen.iter().any(|c| Arc::ptr_eq(c, source)) {
                chosen.push(source.clone());
            }
        }
        Ok(chosen)
    }
}

impl Inner {
    fn new(config: &SearchConfig, cache: ResultCache, http: Http) -> Result<Inner, Error> {
        let sources = sources::registry(config)?;
        let disabled = config.disabled.clone();
        Ok(Inner { http, sources, disabled, cache })
    }

    fn saved_links(&self, linked: Vec<Linked>) -> Vec<(Release, Option<SavedLink>)> {
        linked
            .into_iter()
            .map(|(release, name)| {
                let saved = self
                    .sources
                    .iter()
                    .find(|s| s.meta.name == name)
                    .and_then(|source| source.save_link(release.nzb_url.as_deref()?));
                (release, saved)
            })
            .collect()
    }

    async fn run(&self, source: &Source, ask: &Ask, deadline: Instant) -> Answer {
        let ctx = Ctx { http: &self.http, ask, deadline };
        let rows = timeout_at(deadline, source.search(&ctx)).await.unwrap_or(Err(Failure::Timeout));
        // Rows tagged with the film need not hold its words.
        let rows = match ask {
            Ask::Words(terms) => rows.and_then(|rows| vet(terms, rows)),
            Ask::Film(_) => rows,
        };
        match rows {
            Ok(rows) => Answer::Fresh(Arc::new(rows)),
            Err(failure) => Answer::Failed(failure),
        }
    }
}

fn fetch_reason(e: FetchError) -> String {
    match e {
        FetchError::Timeout(waited) => match waited.as_secs_f64().round() as u64 {
            1 => "no response in 1 second".to_string(),
            seconds => format!("no response in {seconds} seconds"),
        },
        FetchError::Status(status) => format!("HTTP {status}"),
        FetchError::Connect(reason) | FetchError::Decode(reason) => text::truncate(&text::sanitize(&reason), 160),
        FetchError::TooLarge(cap) => format!("larger than {} MiB", cap >> 20),
    }
}

fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

fn no_client(e: reqwest::Error) -> Error {
    Error::internal(format!("cannot build the HTTP client: {e}"))
}

/// Runs one source the way a search does, with a 60-second deadline, and returns
/// every response body it received by URL, plus how the source fared. The
/// `record` example writes these as test fixtures.
#[doc(hidden)]
pub async fn record(config: &SearchConfig, source: &str, query: &str) -> Result<(String, http::Recorded), Error> {
    let inner = Inner::new(config, ResultCache::new(), Http::recording().map_err(no_client)?)?;
    let source = inner
        .sources
        .iter()
        .find(|s| s.meta.name == source)
        .ok_or_else(|| Error::new(ErrorCode::Usage, format!("unknown source {source:?}")))?;
    let ask = Ask::Words(Terms::parse(query));
    let ctx = Ctx { http: &inner.http, ask: &ask, deadline: Instant::now() + Duration::from_secs(60) };
    let outcome = match source.search(&ctx).await {
        Ok(rows) => format!("{} rows", rows.len()),
        Err(failure) => format!("{failure:?}"),
    };
    Ok((outcome, inner.http.take_recorded()))
}
