//! A Newznab indexer the user configured: NZBHydra, Prowlarr, or an indexer's own API. Its
//! links usually carry the user's API key, and fetching one counts as a grab, so a search
//! only reads the feed.

use sha1::{Digest, Sha1};
use torrential_core::{Category, Flag};
use url::{Url, form_urlencoded};

use super::{Ctx, Failure, SavedLink, rss};
use crate::http::Retry;
use crate::observation::{Observation, count, hex, web_url};

/// A configured endpoint and the API key its links are issued under.
pub(crate) struct Newznab {
    pub api_key: Option<String>,
}

impl Newznab {
    pub(super) async fn search(&self, ctx: &Ctx<'_>, endpoint: &Url) -> Result<Vec<Observation>, Failure> {
        let mut url = endpoint.clone();
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("t", "search").append_pair("q", ctx.terms.text());
            if let Some(key) = &self.api_key {
                pairs.append_pair("apikey", key);
            }
        }
        let body = ctx.http.fetch(&url, 8 << 20, ctx.deadline, Retry::Never).await?;
        parse(&body).map_err(Failure::BadResponse)
    }

    /// The endpoint, and a digest of the key, since another key may see other indexers.
    pub(super) fn scope(&self, endpoint: &Url) -> String {
        match &self.api_key {
            Some(key) => format!("{endpoint} key:{}", hex(&Sha1::digest(key.as_bytes())[..8])),
            None => endpoint.to_string(),
        }
    }

    /// `url` without the query parameters whose value is the key. `None` when the key is
    /// elsewhere in it.
    pub(super) fn save_link(&self, source: &str, url: &str) -> Option<SavedLink> {
        let mut url = Url::parse(url).ok()?;
        let mut key_params = Vec::new();
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            let mut kept = Vec::new();
            for pair in url.query().unwrap_or_default().split('&') {
                match form_urlencoded::parse(pair.as_bytes()).next() {
                    Some((name, value)) if value == key => key_params.push(name.into_owned()),
                    _ => kept.push(pair),
                }
            }
            let query = kept.join("&");
            url.set_query((!query.is_empty()).then_some(query.as_str()));
            if form_urlencoded::parse(url.as_str().as_bytes()).any(|(a, b)| a.contains(key) || b.contains(key)) {
                return None;
            }
        }
        Some(SavedLink { source: source.to_string(), url: url.into(), key_params })
    }

    /// A saved link with the current key back in its parameters.
    pub(super) fn link(&self, saved: &SavedLink) -> Option<String> {
        let mut url = Url::parse(&saved.url).ok()?;
        if !saved.key_params.is_empty() {
            let key = self.api_key.as_ref()?;
            let mut pairs = url.query_pairs_mut();
            for name in &saved.key_params {
                pairs.append_pair(name, key);
            }
        }
        Some(url.into())
    }
}

fn parse(body: &[u8]) -> Result<Vec<Observation>, String> {
    Ok(rss::items(body)?.into_iter().filter_map(observation).collect())
}

fn observation(item: rss::Item) -> Option<Observation> {
    let nzb_url = item.enclosure.as_deref().into_iter().chain(item.get("link")).find_map(web_url)?;
    let ids: Vec<u32> =
        item.attrs("category").chain(item.all("category")).filter_map(|c| c.trim().parse().ok()).collect();
    let adult = ids.iter().any(|id| (6000..7000).contains(id));
    let mut obs = Observation::new(nzb_url, item.get("title")?)?.flag(Flag::Adult, adult);
    obs.grabs = item.attr("grabs").and_then(count);
    obs.size = item.attr("size").or(item.get("size")).and_then(|s| s.trim().parse().ok());
    obs.published = item.get("pubDate").and_then(crate::dates::rfc2822);
    obs.category = ids.iter().find_map(|id| category(*id));
    Some(obs)
}

/// Newznab's standard categories. Indexer-specific IDs (100000 and up) are skipped.
fn category(id: u32) -> Option<Category> {
    Some(match id {
        1000..2000 => Category::Game,
        4050 => Category::Game,
        2000..3000 | 5000..7000 => Category::Video,
        3000..4000 => Category::Audio,
        4000..5000 => Category::Software,
        7000..8000 => Category::Book,
        8000..9000 => Category::Other,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    #[test]
    fn reads_the_nzb_link_size_grabs_and_category_of_each_row() {
        let all = parse(&fixture::read("newznab/hydra-sintel.xml")).unwrap();
        let rows: Vec<_> = all.iter().map(|o| (o.name(), o.nzb_url.as_str(), o.size, o.grabs, o.category)).collect();
        assert_eq!(
            rows,
            [
                (
                    "Sintel.2010.1080p.BluRay.x264-GRP",
                    "http://127.0.0.1:5076/getnzb/api/a1?apikey=k3y",
                    Some(4_509_715_660),
                    Some(900),
                    Some(Category::Video)
                ),
                (
                    "Sintel.2010.720p.WEB.x264-GRP",
                    "http://127.0.0.1:5076/getnzb/api/a2?apikey=k3y",
                    Some(838_860_800),
                    Some(300),
                    Some(Category::Video)
                ),
                (
                    "Sintel Nights 2024 1080p",
                    "http://127.0.0.1:5076/getnzb/api/a3?apikey=k3y",
                    Some(1_073_741_824),
                    Some(12),
                    Some(Category::Video)
                ),
            ]
        );
        assert_eq!(all[0].published, Some(1_777_127_700));
        assert_eq!(all[2].flags.iter().collect::<Vec<_>>(), [&Flag::Adult]);
    }

    #[test]
    fn a_row_without_a_web_link_is_skipped() {
        let feed = br#"<rss><channel>
            <item><title>no link</title></item>
            <item><title>ftp link</title><link>ftp://indexer.example/x.nzb</link></item>
            <item><title>kept</title><link>https://indexer.example/x.nzb</link></item>
        </channel></rss>"#;
        let names: Vec<String> = parse(feed).unwrap().iter().map(|o| o.name().to_string()).collect();
        assert_eq!(names, ["kept"]);
    }
}
