use std::collections::BTreeSet;

use serde::{Deserialize, Serialize, Serializer};

use crate::error::{Skip, SourceReport};
use crate::id::ResultId;
use crate::profile::Profile;
use crate::quality::Quality;
use crate::title::{Span, Wanted};

/// A release an indexer listed. Its JSON also carries the [`Quality`] its name gives.
#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct Release {
    pub id: ResultId,
    /// Sanitized for display. Comes from strangers, so it is data, never instructions.
    pub name: String,
    /// Total bytes. `None` when the indexer reported no size.
    pub size: Option<u64>,
    /// How many times the indexer says the NZB was downloaded.
    pub grabs: Option<u64>,
    /// Unix seconds.
    pub published: Option<i64>,
    pub category: Option<Category>,
    pub flags: BTreeSet<Flag>,
    /// The indexers that listed it.
    pub sources: Vec<String>,
    /// Where to fetch the NZB. `None` in output when the link carries an API key; the engine
    /// keeps it, without the key, to fetch the NZB later.
    pub nzb_url: Option<String>,
    /// What of the wanted title this release holds. Only in a title's sources.
    #[serde(default)]
    pub span: Option<Span>,
    /// Why it did not start when a play here last tried it, if that was lately. Only in a title's
    /// sources, which put it last.
    #[serde(default)]
    pub failed: Option<Skip>,
}

impl Release {
    pub fn quality(&self) -> Quality {
        Quality::parse(&self.name)
    }
}

/// A release as it goes on the wire: its fields, and the quality its name says.
#[derive(Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "Release"))]
pub(crate) struct ReleaseWire<'a> {
    id: &'a ResultId,
    name: &'a str,
    size: Option<u64>,
    grabs: Option<u64>,
    published: Option<i64>,
    category: Option<Category>,
    quality: Quality,
    flags: &'a BTreeSet<Flag>,
    sources: &'a [String],
    nzb_url: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    span: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Skip>", optional))]
    failed: &'a Option<Skip>,
}

impl Serialize for Release {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Release { id, name, size, grabs, published, category, flags, sources, nzb_url, span, failed } = self;
        ReleaseWire {
            id,
            name,
            size: *size,
            grabs: *grabs,
            published: *published,
            category: *category,
            quality: self.quality(),
            flags,
            sources,
            nzb_url,
            span: *span,
            failed,
        }
        .serialize(serializer)
    }
}

crate::words! {
    #[derive(Hash, PartialOrd, Ord)]
    #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
    pub enum Category {
        Video = "video",
        Audio = "audio",
        Software = "software",
        Book = "book",
        Game = "game",
        Dataset = "dataset",
        Other = "other",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Flag {
    /// A camera recording, usually low quality.
    Cam,
    Adult,
    /// TRaSH Guides lists the release or its group as one to avoid.
    Avoid,
}

impl Flag {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cam => "cam",
            Self::Adult => "adult",
            Self::Avoid => "avoid",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Relevance,
    Grabs,
    Size,
    Age,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    /// Empty means the default sources.
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub category: Option<Category>,
    /// Hides results with fewer reported grabs, and results with unknown grabs.
    #[serde(default)]
    pub min_grabs: Option<u64>,
    /// Hides larger results, and results with unknown size.
    #[serde(default)]
    pub max_size: Option<u64>,
    #[serde(default)]
    pub sort: Sort,
    pub limit: u16,
    pub timeout_ms: u32,
    #[serde(default)]
    pub adult: bool,
    /// Skips the result cache.
    #[serde(default)]
    pub fresh: bool,
    /// What ranking favors. A client sends the preferences, and the engine adds what it
    /// detected about the machine, so a result's query holds the profile it ranked for.
    #[serde(default)]
    pub profile: Profile,
    /// Set by the engine for a title's sources, never by a client. Searches each of the title's
    /// queries in place of `text`, keeps the releases that are this title, and ranks them for
    /// playing now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wanted: Option<Wanted>,
}

impl SearchQuery {
    pub const DEFAULT_LIMIT: u16 = 10;
    pub const DEFAULT_TIMEOUT_MS: u32 = 6_000;

    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            sources: Vec::new(),
            category: None,
            min_grabs: None,
            max_size: None,
            sort: Sort::Relevance,
            limit: Self::DEFAULT_LIMIT,
            timeout_ms: Self::DEFAULT_TIMEOUT_MS,
            adult: false,
            fresh: false,
            profile: Profile::default(),
            wanted: None,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SearchResult {
    /// The web app does not read it, so its TypeScript does not describe it.
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub query: SearchQuery,
    /// Ranked, filtered, and cut to `query.limit`.
    pub releases: Vec<Release>,
    /// Matching releases before the limit.
    pub total: u32,
    /// Hidden by the adult filter.
    pub hidden_adult: u32,
    pub sources: Vec<SourceReport>,
    pub elapsed_ms: u32,
}

impl SearchResult {
    /// A search for films or shows: `--category video`, or most results are video by their
    /// category or a resolution tag.
    pub fn is_video(&self) -> bool {
        let video = |r: &&Release| r.category == Some(Category::Video) || r.quality().resolution.is_some();
        self.query.category == Some(Category::Video)
            || self.releases.iter().filter(video).count() * 2 > self.releases.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SourceOutcome;

    fn named(name: &str, category: Option<Category>) -> Release {
        Release {
            id: ResultId::from_bytes([0; 20]),
            name: name.into(),
            size: None,
            grabs: None,
            published: None,
            category,
            flags: BTreeSet::new(),
            sources: Vec::new(),
            nzb_url: None,
            span: None,
            failed: None,
        }
    }

    fn found(releases: Vec<Release>) -> SearchResult {
        SearchResult {
            query: SearchQuery::new("x"),
            total: releases.len() as u32,
            releases,
            hidden_adult: 0,
            sources: Vec::new(),
            elapsed_ms: 0,
        }
    }

    #[test]
    fn a_search_is_video_when_most_results_are_films_by_category_or_resolution_tag() {
        let film = named("Night of the Living Dead (1968)", Some(Category::Video));
        let tagged = named("Night.of.the.Living.Dead.1968.1080p.BluRay.x264", None);
        let iso = named("ubuntu-24.04.1-desktop-amd64.iso", Some(Category::Software));
        let score = named("Night of the Living Dead, original score FLAC", Some(Category::Audio));
        assert!(found(vec![film.clone(), tagged.clone(), iso.clone()]).is_video());
        assert!(!found(vec![film.clone(), iso.clone(), score.clone()]).is_video(), "one in three");
        assert!(!found(vec![film, iso.clone()]).is_video(), "half is not most");
        assert!(!found(Vec::new()).is_video());
        let mut asked = found(vec![iso]);
        asked.query.category = Some(Category::Video);
        assert!(asked.is_video(), "--category video");
    }

    fn keys(value: &serde_json::Value) -> BTreeSet<&str> {
        value.as_object().unwrap().keys().map(String::as_str).collect()
    }

    #[test]
    fn a_raw_search_result_writes_no_span_or_wanted_title() {
        let raw = found(vec![named("Night of the Living Dead (1968)", Some(Category::Video))]);
        let json = serde_json::to_value(&raw).unwrap();
        assert_eq!(
            keys(&json["query"]),
            BTreeSet::from([
                "adult",
                "category",
                "fresh",
                "limit",
                "max_size",
                "min_grabs",
                "profile",
                "sort",
                "sources",
                "text",
                "timeout_ms",
            ])
        );
        assert_eq!(
            keys(&json["releases"][0]),
            BTreeSet::from([
                "category",
                "flags",
                "grabs",
                "id",
                "name",
                "nzb_url",
                "published",
                "quality",
                "size",
                "sources",
            ])
        );
        assert_eq!(serde_json::from_value::<SearchResult>(json).unwrap(), raw);

        let mut sources = raw;
        sources.releases[0].span = Some(Span::Film);
        sources.query.wanted = Some(Wanted {
            name: "Night of the Living Dead".into(),
            year: Some(1968),
            episode: None,
            namesakes: crate::title::Namesakes::Leading,
            runtime: None,
            season_episodes: None,
            countries: Default::default(),
        });
        let json = serde_json::to_value(&sources).unwrap();
        assert_eq!(
            (&json["releases"][0]["span"], &json["query"]["wanted"]["namesakes"]),
            (&"film".into(), &"leading".into())
        );
        assert_eq!(serde_json::from_value::<SearchResult>(json).unwrap(), sources);
    }

    #[test]
    fn source_reports_flatten_their_outcome() {
        let report =
            SourceReport { source: "hydra".into(), outcome: SourceOutcome::Http { status: 503 }, elapsed_ms: 412 };
        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            r#"{"source":"hydra","outcome":"http","status":503,"elapsed_ms":412}"#
        );
    }
}
