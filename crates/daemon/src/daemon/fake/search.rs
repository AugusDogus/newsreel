use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newsreel_core::{
    DownloadId, Error, ErrorCode, HexPrefix, Profile, Release, ResultId, SearchQuery, SearchResult, SourceOutcome,
    SourceReport,
};
use tokio::sync::mpsc;

use crate::daemon::BoxFuture;
use crate::daemon::search_port::{Progress, RankFor, SavedLink, SearchPort};

/// A search that answers from a fixed catalog, each source after its own delay.
#[derive(Clone, Default)]
pub struct Search(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    catalog: Vec<Release>,
    /// NZBs by URL.
    nzbs: BTreeMap<String, Vec<u8>>,
    /// NZB links whose indexer never answers.
    stalled: std::collections::BTreeSet<String>,
    /// Serves the NZBs of test releases the network has published, at
    /// `https://indexer.example/get/{id}.nzb`. Any other there never arrives.
    network: Option<super::Network>,
    delay: Duration,
    script: Vec<Source>,
    recent: Vec<Release>,
    searches: usize,
    /// The NZBs fetched, by URL, in order.
    fetched: Vec<String>,
    /// The profile the last answer was ranked for.
    ranked_for: Option<Profile>,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub after: Duration,
    pub failure: Option<SourceOutcome>,
}

impl Source {
    pub fn answers(name: &str, after_ms: u64) -> Self {
        Self { name: name.into(), after: Duration::from_millis(after_ms), failure: None }
    }

    pub fn fails(name: &str, after_ms: u64, failure: SourceOutcome) -> Self {
        Self { name: name.into(), after: Duration::from_millis(after_ms), failure: Some(failure) }
    }
}

impl Search {
    #[cfg(test)]
    pub fn new(catalog: Vec<Release>) -> Self {
        Self::scripted(catalog, vec![Source::answers("fake", 0)], BTreeMap::new())
    }

    pub fn scripted(catalog: Vec<Release>, mut script: Vec<Source>, nzbs: BTreeMap<String, Vec<u8>>) -> Self {
        script.sort_by_key(|s| s.after);
        Self(Arc::new(Mutex::new(State {
            catalog,
            nzbs,
            stalled: Default::default(),
            network: None,
            delay: Duration::ZERO,
            script,
            recent: Vec::new(),
            searches: 0,
            fetched: Vec::new(),
            ranked_for: None,
        })))
    }

    #[cfg(test)]
    pub fn searches(&self) -> usize {
        self.0.lock().unwrap().searches
    }

    #[cfg(test)]
    pub fn fetched(&self) -> Vec<String> {
        self.0.lock().unwrap().fetched.clone()
    }

    #[cfg(test)]
    pub fn ranked_for(&self) -> Option<Profile> {
        self.0.lock().unwrap().ranked_for.clone()
    }

    #[cfg(test)]
    pub fn serve_nzb(&self, url: &str, bytes: Vec<u8>) {
        self.0.lock().unwrap().nzbs.insert(url.to_string(), bytes);
    }

    /// Serves the NZBs `network` publishes, as an indexer does once a release is posted.
    #[cfg(test)]
    pub fn attach(&self, network: super::Network) {
        self.0.lock().unwrap().network = Some(network);
    }

    /// Makes the indexer behind `url` never answer a request for its NZB.
    #[cfg(test)]
    pub fn stall_nzb(&self, url: &str) {
        self.0.lock().unwrap().stalled.insert(url.to_string());
    }

    #[cfg(test)]
    pub fn set_delay(&self, delay: Duration) {
        self.0.lock().unwrap().delay = delay;
    }

    fn parts(&self) -> (Vec<Release>, Vec<Source>, Duration) {
        let mut state = self.0.lock().unwrap();
        state.searches += 1;
        (state.catalog.clone(), state.script.clone(), state.delay)
    }

    fn remember(&self, result: SearchResult) -> SearchResult {
        let mut state = self.0.lock().unwrap();
        state.recent.retain(|r| !result.releases.iter().any(|n| n.id == r.id));
        state.recent.splice(0..0, result.releases.iter().cloned());
        state.ranked_for = Some(result.query.profile.clone());
        result
    }
}

/// The fake release a catalog row's `fake://` link serves: one file, named and sized as listed.
fn invented(release: &Release) -> Option<Vec<u8>> {
    let hex = release.nzb_url.as_deref()?.strip_prefix("fake://")?;
    let file = super::Posting {
        id: hex.parse().ok()?,
        name: release.name.clone(),
        files: vec![(release.name.clone(), release.size?)],
    };
    Some(file.bytes())
}

fn compose(
    catalog: &[Release],
    answered: &[Source],
    query: SearchQuery,
    profile: Profile,
    elapsed: Duration,
) -> SearchResult {
    let words: Vec<String> = query.text.split_whitespace().map(str::to_lowercase).collect();
    // A title's search keeps its releases in the catalog's order, each with what of it it holds.
    let found = |r: &Release| match &query.wanted {
        Some(wanted) => {
            let span = Some(wanted.fit(&r.name)?.span);
            Some(Release { span, ..r.clone() })
        }
        None => words.iter().all(|w| r.name.to_lowercase().contains(w.as_str())).then(|| r.clone()),
    };
    let matches = |r: &Release| found(r).is_some();
    let listed_by = |r: &Release, source: &Source| r.sources.first() == Some(&source.name);
    let sources = answered
        .iter()
        .map(|source| {
            let outcome = source.failure.clone().unwrap_or_else(|| SourceOutcome::Ok {
                results: catalog.iter().filter(|r| listed_by(r, source) && matches(r)).count() as u32,
            });
            SourceReport { source: source.name.clone(), outcome, elapsed_ms: source.after.as_millis() as u32 }
        })
        .collect();
    let matching: Vec<Release> = catalog
        .iter()
        .filter(|r| answered.iter().any(|s| s.failure.is_none() && listed_by(r, s)))
        .filter_map(found)
        .collect();
    let total = matching.len() as u32;
    let releases = matching.into_iter().take(query.limit as usize).collect();
    let query = SearchQuery { profile, ..query };
    SearchResult { query, releases, total, hidden_adult: 0, sources, elapsed_ms: elapsed.as_millis() as u32 }
}

impl SearchPort for Search {
    fn search_stream<'a>(
        &'a self,
        query: SearchQuery,
        rank_for: RankFor<'a>,
        progress: mpsc::UnboundedSender<Progress>,
    ) -> BoxFuture<'a, Result<SearchResult, Error>> {
        Box::pin(async move {
            let (catalog, script, delay) = self.parts();
            tokio::time::sleep(delay).await;
            let started = tokio::time::Instant::now();
            for answered in 1..script.len() {
                tokio::time::sleep_until(started + script[answered - 1].after).await;
                let partial =
                    compose(&catalog, &script[..answered], query.clone(), rank_for(), delay + started.elapsed());
                let result = self.remember(partial);
                let pending = script[answered..].iter().map(|s| s.name.clone()).collect();
                let _ = progress.send(Progress { result, pending });
            }
            if let Some(last) = script.last() {
                tokio::time::sleep_until(started + last.after).await;
            }
            let result = compose(&catalog, &script, query, rank_for(), delay + started.elapsed());
            // As the real search does, when every source failed.
            if !script.is_empty() && script.iter().all(|s| s.failure.is_some()) {
                let names: Vec<&str> = script.iter().map(|s| s.name.as_str()).collect();
                let hint = newsreel_search::failed_hint(&result.sources, newsreel_search::Reader::Terminal);
                return Err(Error::new(
                    ErrorCode::ProvidersUnavailable,
                    format!("all sources failed ({})", names.join(", ")),
                )
                .with_hint(hint)
                .with_details(newsreel_core::error::Details::Sources(result.sources)));
            }
            Ok(self.remember(result))
        })
    }

    fn fetch_nzb(&self, url: String) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            let published = {
                let mut state = self.0.lock().unwrap();
                state.fetched.push(url.clone());
                let posted =
                    url.strip_prefix("https://indexer.example/get/").and_then(|rest| rest.strip_suffix(".nzb"));
                match (&state.network, posted.and_then(|hex| hex.parse::<DownloadId>().ok())) {
                    _ if state.nzbs.contains_key(&url) => None,
                    (Some(network), Some(id)) => Some(network.published(id)),
                    _ => None,
                }
            };
            match published {
                Some(Some(bytes)) => return Ok(bytes),
                Some(None) => return std::future::pending().await,
                None => {}
            }
            if self.0.lock().unwrap().stalled.contains(&url) {
                return std::future::pending().await;
            }
            let state = self.0.lock().unwrap();
            let listed =
                || state.catalog.iter().find(|r| r.nzb_url.as_deref() == Some(url.as_str())).and_then(invented);
            state.nzbs.get(&url).cloned().or_else(listed).ok_or_else(|| {
                Error::new(ErrorCode::FetchFailed, "could not fetch the NZB: HTTP 404")
                    .with_hint("check the network connection and the indexer, or search again")
            })
        })
    }

    fn find(&self, prefix: &HexPrefix) -> Vec<Release> {
        self.0.lock().unwrap().recent.iter().filter(|r| prefix.matches(&r.id)).cloned().collect()
    }

    fn nzb_url(&self, release: &Release) -> Result<Option<String>, Error> {
        Ok(release.nzb_url.clone())
    }

    fn saved_link(&self, _: &ResultId) -> Option<SavedLink> {
        None
    }

    fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>) {
        let mut state = self.0.lock().unwrap();
        for (release, _) in saved {
            if !state.recent.iter().any(|r| r.id == release.id) {
                state.recent.push(release);
            }
        }
    }
}
