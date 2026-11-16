use std::collections::BTreeSet;

use torrential_core::{Category, Flag, TitleId};

/// One row an indexer answered.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Observation {
    /// Where the NZB is. It names the row, so the same link is the same result.
    pub nzb_url: String,
    name: String,
    pub size: Option<u64>,
    pub grabs: Option<u64>,
    pub published: Option<i64>,
    pub category: Option<Category>,
    pub flags: BTreeSet<Flag>,
    /// The title the indexer tagged the row with.
    pub imdb: Option<TitleId>,
}

impl Observation {
    pub fn new(nzb_url: String, raw_name: &str) -> Option<Observation> {
        let name = torrential_core::text::sanitize(raw_name);
        (!name.is_empty()).then(|| Observation {
            nzb_url,
            name,
            size: None,
            grabs: None,
            published: None,
            category: None,
            flags: BTreeSet::new(),
            imdb: None,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn flag(mut self, flag: Flag, on: bool) -> Self {
        if on {
            self.flags.insert(flag);
        }
        self
    }
}

pub(crate) fn count(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

pub(crate) fn web_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host().is_some()).then(|| url.to_string())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_counts_stay_unknown() {
        assert_eq!(count(" 37 "), Some(37));
        assert_eq!(count("0"), Some(0));
        assert_eq!(count("-1"), None);
        assert_eq!(count(""), None);
    }

    #[test]
    fn links_must_be_web_urls() {
        assert_eq!(web_url(" https://indexer.example/get/7 ").as_deref(), Some("https://indexer.example/get/7"));
        assert_eq!(web_url("ftp://indexer.example/get/7"), None);
        assert_eq!(web_url("not a url"), None);
    }
}
