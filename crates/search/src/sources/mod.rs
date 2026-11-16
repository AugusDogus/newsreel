mod newznab;
mod rss;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use torrential_core::{Error, ErrorCode, SourceOutcome, TitleId, text};
use url::Url;

use crate::http::{FetchError, Http};
use crate::observation::Observation;
use crate::terms::Terms;
use crate::{IndexerConfig, SearchConfig};
use newznab::Newznab;

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct SourceMeta {
    pub name: String,
    /// Earlier indexers in the config win ties, such as which name a release shows.
    pub authority: u16,
}

pub(crate) struct Source {
    pub meta: Arc<SourceMeta>,
    /// It can list films and series. An indexer is assumed to.
    pub video: bool,
    base: Url,
    newznab: Newznab,
}

/// An NZB link from a configured indexer, as an engine saves it for the next one: without the
/// API key it was issued under. [`crate::Search::nzb_url`] puts the indexer's
/// current key back.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SavedLink {
    /// The source's name in the config.
    pub source: String,
    /// The link without the query parameters whose value was the key.
    pub url: String,
    /// Those parameters' names, such as `apikey`. Empty for a link that carries no key.
    pub key_params: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Failure {
    Timeout,
    Http(u16),
    Connect(String),
    BadResponse(String),
    Irrelevant,
}

impl Failure {
    pub fn outcome(&self) -> SourceOutcome {
        let clean = |reason: &str| text::truncate(&text::sanitize(reason), 160);
        match self {
            Self::Timeout => SourceOutcome::Timeout,
            Self::Http(status) => SourceOutcome::Http { status: *status },
            Self::Connect(reason) => SourceOutcome::Connect { reason: clean(reason) },
            Self::BadResponse(reason) => SourceOutcome::BadResponse { reason: clean(reason) },
            Self::Irrelevant => SourceOutcome::Irrelevant,
        }
    }
}

impl From<FetchError> for Failure {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Timeout(_) => Self::Timeout,
            FetchError::Status(status) => Self::Http(status),
            FetchError::Connect(reason) => Self::Connect(reason),
            FetchError::TooLarge(cap) => Self::BadResponse(format!("response larger than {cap} bytes")),
            FetchError::Decode(reason) => Self::BadResponse(reason),
        }
    }
}

/// What a search asks a source for.
pub(crate) enum Ask {
    /// Releases whose names hold these words.
    Words(Terms),
    /// The releases the indexer tagged with this film.
    Film(TitleId),
}

impl Ask {
    /// The ask as the cache keys its answers.
    pub fn key(&self) -> String {
        match self {
            Self::Words(terms) => terms.text().to_string(),
            Self::Film(id) => format!("film:{}", id.as_str()),
        }
    }
}

pub(crate) struct Ctx<'a> {
    pub http: &'a Http,
    pub ask: &'a Ask,
    pub deadline: Instant,
}

impl Source {
    /// What the answers cached for this source depend on beside the query.
    pub fn scope(&self) -> String {
        self.newznab.scope(&self.base)
    }

    /// The link at `url` as a later engine keeps it.
    pub fn save_link(&self, url: &str) -> Option<SavedLink> {
        self.newznab.save_link(&self.meta.name, url)
    }

    /// A saved link with the key it needs now.
    pub fn link(&self, saved: &SavedLink) -> Option<String> {
        self.newznab.link(saved)
    }

    pub async fn search(&self, ctx: &Ctx<'_>) -> Result<Vec<Observation>, Failure> {
        self.newznab.search(ctx, &self.base).await
    }
}


pub(crate) fn registry(config: &SearchConfig) -> Result<Vec<Arc<Source>>, Error> {
    let mut sources: Vec<Arc<Source>> = Vec::new();
    for (authority, IndexerConfig { name, url, api_key }) in (0..).zip(&config.indexers) {
        let valid = !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b));
        if !valid {
            return Err(config_error(format!(
                "indexer name {name:?} must be lowercase letters, digits, '-', '_', or '.'"
            )));
        }
        if sources.iter().any(|s| s.meta.name == *name) {
            return Err(config_error(format!("source name {name:?} is used twice")));
        }
        let meta = SourceMeta { name: name.clone(), authority };
        let base = url::Url::parse(url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| config_error(format!("indexer {name:?} needs an http or https URL")))?;
        let newznab = Newznab { api_key: api_key.clone() };
        sources.push(Arc::new(Source { meta: Arc::new(meta), video: true, base, newznab }));
    }
    for name in &config.disabled {
        if !sources.iter().any(|s| s.meta.name == *name) {
            return Err(config_error(format!("sources.disabled lists an unknown source {name:?}")));
        }
    }
    sources.retain(|s| !config.disabled.contains(&s.meta.name));
    Ok(sources)
}

fn config_error(message: String) -> Error {
    Error::new(ErrorCode::Usage, message).with_hint("fix the sources section of the torrential config")
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;

    pub fn read(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hydra(key: Option<&str>) -> Arc<Source> {
        let indexer = IndexerConfig {
            name: "hydra".into(),
            url: "http://127.0.0.1:5076/api".into(),
            api_key: key.map(String::from),
        };
        let config = SearchConfig { indexers: vec![indexer], ..SearchConfig::default() };
        registry(&config).unwrap().into_iter().find(|s| s.meta.name == "hydra").unwrap()
    }

    #[test]
    fn a_saved_link_leaves_out_the_key_and_takes_the_current_one_back() {
        let save = |key, url: &str| hydra(key).save_link(url).map(|l| (l.url, l.key_params));
        let issued = "http://127.0.0.1:5076/getnzb/api/x?r=k3y&path=a%2Bb%3D&file=Sintel+2010";
        let saved = hydra(Some("k3y")).save_link(issued).unwrap();

        assert_eq!(
            saved,
            SavedLink {
                source: "hydra".into(),
                url: "http://127.0.0.1:5076/getnzb/api/x?path=a%2Bb%3D&file=Sintel+2010".into(),
                key_params: vec!["r".into()],
            }
        );
        assert_eq!(
            hydra(Some("n3w")).link(&saved).as_deref(),
            Some("http://127.0.0.1:5076/getnzb/api/x?path=a%2Bb%3D&file=Sintel+2010&r=n3w")
        );
        assert_eq!(hydra(None).link(&saved), None);
        assert_eq!(
            save(Some("k3y"), "http://h/1/download?apikey=k3y&link=x"),
            Some(("http://h/1/download?link=x".into(), vec!["apikey".into()]))
        );
        assert_eq!(save(Some("k3y"), "http://h/dl/k3y/a.nzb?apikey=k3y"), None, "a key in the path");
        assert_eq!(save(Some("k3y"), "http://h/dl?apikey=k3y&next=%2Fget%3Fkey%3Dk3y"), None, "an encoded key");
        assert_eq!(save(None, "http://h/dl?id=7"), Some(("http://h/dl?id=7".into(), Vec::new())));
    }
}
