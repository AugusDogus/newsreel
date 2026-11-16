use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use torrential_core::error::Details;
use torrential_core::title::{Namesakes, Wanted};
use torrential_core::{ErrorCode, HexPrefix, Release, ResultId, SearchQuery, SourceOutcome};
use torrential_search::{IndexerConfig, Progress, ResultCache, SavedLink, Search, SearchConfig};

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    delay: Duration,
    stall: Duration,
    content_length: bool,
}

fn ok(body: Vec<u8>) -> Reply {
    Reply { status: 200, body, delay: Duration::ZERO, stall: Duration::ZERO, content_length: true }
}

fn status(code: u16) -> Reply {
    Reply { status: code, body: b"no".to_vec(), delay: Duration::ZERO, stall: Duration::ZERO, content_length: true }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

fn hydra_feed() -> Vec<u8> {
    fixture("newznab/hydra-sintel.xml")
}

/// Keyless Newznab indexers named `names`, each at `{base}/{name}/api`.
fn indexers(base: &str, names: &[&str]) -> SearchConfig {
    let indexers = names
        .iter()
        .map(|name| IndexerConfig { name: name.to_string(), url: format!("{base}/{name}/api"), api_key: None })
        .collect();
    SearchConfig { indexers, ..SearchConfig::default() }
}

/// One indexer named `hydra` at `{base}/newznab/api`.
fn keyed(base: &str, api_key: Option<&str>) -> SearchConfig {
    let hydra =
        IndexerConfig { name: "hydra".into(), url: format!("{base}/newznab/api"), api_key: api_key.map(String::from) };
    SearchConfig { indexers: vec![hydra], ..SearchConfig::default() }
}

/// The ID of the hydra fixture's 1080p BluRay row: the SHA-1 of its NZB link.
const BLURAY: &str = "uf26efcec57e8563e97dbbdd58cd65958f41fa450";
const BLURAY_NZB: &str = "http://127.0.0.1:5076/getnzb/api/a1?apikey=k3y";

struct Server {
    base: String,
    hits: Arc<Mutex<HashMap<&'static str, usize>>>,
}

impl Server {
    async fn start(routes: Vec<(&'static str, Vec<Reply>)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits: Arc<Mutex<HashMap<&'static str, usize>>> = Arc::default();
        let (routes, counter) = (Arc::new(routes), hits.clone());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (routes, counter) = (routes.clone(), counter.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("/").to_string();
                    let reply = match routes.iter().find(|(prefix, _)| path.starts_with(prefix)) {
                        Some((prefix, replies)) => {
                            let mut counter = counter.lock().unwrap();
                            let n = counter.entry(*prefix).or_insert(0);
                            *n += 1;
                            replies[(*n - 1).min(replies.len() - 1)].clone()
                        }
                        None => status(404),
                    };
                    tokio::time::sleep(reply.delay).await;
                    let mut response = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status);
                    if reply.content_length {
                        response.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
                    }
                    response.push_str("\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                    tokio::time::sleep(reply.stall).await;
                    let _ = socket.write_all(&reply.body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Server { base, hits }
    }

    fn hits(&self, prefix: &str) -> usize {
        self.hits.lock().unwrap().get(prefix).copied().unwrap_or(0)
    }

    fn config(&self, sources: &[&str]) -> SearchConfig {
        indexers(&self.base, sources)
    }
}

fn query(text: &str, sources: &[&str]) -> SearchQuery {
    SearchQuery { sources: sources.iter().map(|s| s.to_string()).collect(), timeout_ms: 3000, ..SearchQuery::new(text) }
}

fn outcomes(result: &torrential_core::SearchResult) -> Vec<(String, SourceOutcome)> {
    result.sources.iter().map(|r| (r.source.clone(), r.outcome.clone())).collect()
}

#[tokio::test]
async fn two_indexers_rows_are_listed_and_ranked_together() {
    let server = Server::start(vec![
        ("/hydra/", vec![ok(hydra_feed())]),
        ("/geek/", vec![ok(fixture("newznab/geek-sintel.xml"))]),
    ])
    .await;
    let search = Search::new(&server.config(&["hydra", "geek"]), ResultCache::new()).unwrap();
    let mut q = query("sintel", &["hydra", "geek"]);
    q.limit = 200;

    let result = search.search(&q).await.unwrap();

    assert_eq!(
        outcomes(&result),
        [("hydra".into(), SourceOutcome::Ok { results: 3 }), ("geek".into(), SourceOutcome::Ok { results: 3 })]
    );
    let listed: Vec<(&str, &[String], Option<u64>)> =
        result.releases.iter().map(|r| (r.name.as_str(), r.sources.as_slice(), r.grabs)).collect();
    assert_eq!(
        listed,
        [
            ("Sintel.2010.1080p.BluRay.x264-GRP", &["hydra".to_string()][..], Some(900)),
            ("Sintel.2010.720p.WEB.x264-GRP", &["hydra".to_string()][..], Some(300)),
            ("Sintel.2010.2160p.UHD.BluRay.x265-GRP", &["geek".to_string()][..], Some(120)),
            ("Sintel.2010.1080p.WEB.H264-OTHER", &["geek".to_string()][..], Some(51)),
            ("Sintel The Making Of 2011 720p", &["geek".to_string()][..], Some(13)),
        ]
    );
    assert_eq!((result.total, result.hidden_adult), (5, 1));
    let bluray = &result.releases[0];
    assert_eq!(bluray.id.to_ref(), BLURAY);
    assert_eq!((bluray.size, bluray.published), (Some(4_509_715_660), Some(1_777_127_700)));
}

// Windows retries a refused connection for about two seconds, past the search's deadline, so it reads as a timeout.
#[cfg(unix)]
#[tokio::test]
async fn every_source_failing_is_providers_unavailable() {
    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let server = Server::start(vec![
        ("/down/", vec![status(503)]),
        ("/blocked/", vec![ok(b"<html>Just a moment...</html>".to_vec())]),
    ])
    .await;
    let mut config = server.config(&["blocked", "down"]);
    config.indexers.extend(indexers(&format!("http://{refused}"), &["refused"]).indexers);
    let search = Search::new(&config, ResultCache::new()).unwrap();

    let err = search.search(&query("sintel", &["down", "refused", "blocked"])).await.unwrap_err();

    assert_eq!(err.code, ErrorCode::ProvidersUnavailable);
    assert_eq!(err.message, "all sources failed (blocked, down, refused)");
    let Some(Details::Sources(reports)) = err.details else { panic!("no source reports") };
    let kinds: Vec<(&str, &str)> = reports
        .iter()
        .map(|r| {
            let kind = match &r.outcome {
                SourceOutcome::Http { status: 503 } => "http 503",
                SourceOutcome::Connect { .. } => "connect",
                SourceOutcome::BadResponse { .. } => "bad_response",
                _ => "other",
            };
            (r.source.as_str(), kind)
        })
        .collect();
    assert_eq!(kinds, [("blocked", "bad_response"), ("down", "http 503"), ("refused", "connect")]);
    assert_eq!(server.hits("/down/"), 1);
}

#[tokio::test]
async fn failure_reasons_never_carry_terminal_escapes() {
    let hostile =
        b"<?xml version=\"1.0\"?><error code=\"1\" description=\"\x1b]52;c;cm0gLXJm\x07\x1b[2Jgone\"/>".to_vec();
    let server = Server::start(vec![("/hostile/", vec![ok(hostile)]), ("/hydra/", vec![ok(hydra_feed())])]).await;
    let search = Search::new(&server.config(&["hydra", "hostile"]), ResultCache::new()).unwrap();

    let result = search.search(&query("sintel", &["hostile", "hydra"])).await.unwrap();

    assert_eq!(
        outcomes(&result)[1],
        ("hostile".into(), SourceOutcome::BadResponse { reason: "indexer error 1: gone".into() })
    );
}

#[tokio::test]
async fn a_slow_answer_that_has_started_keeps_the_whole_deadline() {
    let slow = Reply { stall: Duration::from_millis(2200), ..ok(hydra_feed()) };
    let server = Server::start(vec![("/hydra/", vec![slow])]).await;
    let search = Search::new(&server.config(&["hydra"]), ResultCache::new()).unwrap();

    let result = search.search(&query("sintel", &["hydra"])).await.unwrap();

    assert_eq!(outcomes(&result), [("hydra".into(), SourceOutcome::Ok { results: 3 })]);
    assert_eq!(server.hits("/hydra/"), 1);
}

#[tokio::test]
async fn a_short_budget_waits_for_its_only_request() {
    let slow = Reply { delay: Duration::from_millis(750), ..ok(hydra_feed()) };
    let server = Server::start(vec![("/hydra/", vec![slow])]).await;
    let search = Search::new(&server.config(&["hydra"]), ResultCache::new()).unwrap();
    let mut q = query("sintel", &["hydra"]);
    q.timeout_ms = 1200;

    let result = search.search(&q).await.unwrap();

    assert_eq!(outcomes(&result), [("hydra".into(), SourceOutcome::Ok { results: 3 })]);
    assert_eq!(server.hits("/hydra/"), 1);
}

#[tokio::test]
async fn an_indexer_is_asked_only_once() {
    let server = Server::start(vec![("/newznab/api", vec![status(503), ok(hydra_feed())])]).await;
    let search = Search::new(&keyed(&server.base, None), ResultCache::new()).unwrap();

    let err = search.search(&query("sintel", &["hydra"])).await.unwrap_err();

    let Some(Details::Sources(reports)) = err.details else { panic!("no source reports") };
    assert_eq!(reports[0].outcome, SourceOutcome::Http { status: 503 });
    assert_eq!(server.hits("/newznab/api"), 1);
}

#[tokio::test]
async fn an_oversized_answer_is_an_error_not_a_truncated_list() {
    let mut huge = hydra_feed();
    huge.resize(9 << 20, b' ');
    let server = Server::start(vec![
        ("/huge/", vec![Reply { content_length: false, ..ok(huge) }]),
        ("/hydra/", vec![ok(hydra_feed())]),
    ])
    .await;
    let search = Search::new(&server.config(&["hydra", "huge"]), ResultCache::new()).unwrap();

    let result = search.search(&query("sintel", &["hydra", "huge"])).await.unwrap();

    assert_eq!(
        outcomes(&result)[1],
        ("huge".into(), SourceOutcome::BadResponse { reason: "response larger than 8388608 bytes".into() })
    );
}

#[tokio::test]
async fn a_slow_source_times_out_by_the_deadline() {
    let slow = Reply { delay: Duration::from_secs(10), ..ok(hydra_feed()) };
    let server = Server::start(vec![("/slow/", vec![slow]), ("/hydra/", vec![ok(hydra_feed())])]).await;
    let search = Search::new(&server.config(&["slow", "hydra"]), ResultCache::new()).unwrap();
    let mut q = query("sintel", &["slow", "hydra"]);
    q.timeout_ms = 1500;

    let started = Instant::now();
    let result = search.search(&q).await.unwrap();
    let wall = started.elapsed();

    assert_eq!(outcomes(&result)[0], ("slow".into(), SourceOutcome::Timeout));
    assert_eq!(outcomes(&result)[1], ("hydra".into(), SourceOutcome::Ok { results: 3 }));
    assert!(wall < Duration::from_millis(1700), "returned after {wall:?}");
    assert!(result.elapsed_ms < 1700, "reported {} ms", result.elapsed_ms);
}

#[tokio::test]
async fn a_repeat_search_is_served_from_the_cache_unless_fresh() {
    let server = Server::start(vec![("/hydra/", vec![ok(hydra_feed())]), ("/down/", vec![status(503)])]).await;
    let search = Search::new(&server.config(&["down", "hydra"]), ResultCache::new()).unwrap();
    let q = query("Sintel", &["down", "hydra"]);

    let first = search.search(&q).await.unwrap();
    let mut narrower = query("sintel", &["down", "hydra"]);
    narrower.min_grabs = Some(500);
    let second = search.search(&narrower).await.unwrap();
    let fresh = search.search(&SearchQuery { fresh: true, ..q.clone() }).await.unwrap();

    assert_eq!(outcomes(&first)[1], ("hydra".into(), SourceOutcome::Ok { results: 3 }));
    assert_eq!(outcomes(&second)[1], ("hydra".into(), SourceOutcome::Cached { results: 3 }));
    let grabbed: Vec<(&str, Option<u64>)> = second.releases.iter().map(|r| (r.name.as_str(), r.grabs)).collect();
    assert_eq!(grabbed, [("Sintel.2010.1080p.BluRay.x264-GRP", Some(900))]);
    assert_eq!(outcomes(&fresh)[1], ("hydra".into(), SourceOutcome::Ok { results: 3 }));
    assert_eq!((server.hits("/hydra/"), server.hits("/down/")), (2, 3));
}

#[tokio::test]
async fn a_shared_cache_does_not_answer_for_another_endpoint() {
    let first = Server::start(vec![("/hydra/", vec![ok(hydra_feed())])]).await;
    let mirror = Server::start(vec![("/hydra/", vec![ok(hydra_feed())])]).await;
    let cache = ResultCache::new();
    let q = query("sintel", &["hydra"]);

    Search::new(&first.config(&["hydra"]), cache.clone()).unwrap().search(&q).await.unwrap();
    let moved = Search::new(&mirror.config(&["hydra"]), cache).unwrap().search(&q).await.unwrap();

    assert_eq!(outcomes(&moved), [("hydra".into(), SourceOutcome::Ok { results: 3 })]);
    assert_eq!(mirror.hits("/hydra/"), 1);
}

#[tokio::test]
async fn api_keys_stay_out_of_results_but_resolve_for_the_engine() {
    let server = Server::start(vec![("/newznab/api", vec![ok(hydra_feed())])]).await;
    let cache = ResultCache::new();
    let search = Search::new(&keyed(&server.base, Some("k3y")), cache.clone()).unwrap();

    let result = search.search(&query("sintel", &["hydra"])).await.unwrap();

    assert_eq!(result.releases.len(), 2);
    assert!(!format!("{result:?}").contains("k3y"));
    assert!(!serde_json::to_string(&result).unwrap().contains("k3y"));
    assert!(result.releases.iter().all(|r| r.nzb_url.is_none()));
    let resolved = cache.find(&BLURAY.parse().unwrap());
    assert_eq!(resolved[0].nzb_url.as_deref(), Some(BLURAY_NZB));
}

#[tokio::test]
async fn a_new_api_key_is_not_answered_from_the_old_one() {
    let server = Server::start(vec![("/newznab/api", vec![ok(hydra_feed())])]).await;
    let cache = ResultCache::new();
    let q = query("sintel", &["hydra"]);

    Search::new(&keyed(&server.base, Some("old")), cache.clone()).unwrap().search(&q).await.unwrap();
    let rekeyed = Search::new(&keyed(&server.base, Some("new")), cache).unwrap().search(&q).await.unwrap();

    assert_eq!(outcomes(&rekeyed), [("hydra".into(), SourceOutcome::Ok { results: 3 })]);
    assert_eq!(server.hits("/newznab/api"), 2);
}

#[tokio::test]
async fn returned_releases_resolve_by_id_prefix() {
    let server = Server::start(vec![("/hydra/", vec![ok(hydra_feed())])]).await;
    let cache = ResultCache::new();
    let search = Search::new(&server.config(&["hydra"]), cache.clone()).unwrap();

    search.search(&query("sintel", &["hydra"])).await.unwrap();

    let prefix: HexPrefix = BLURAY[..7].parse().unwrap();
    let found: Vec<String> = cache.find(&prefix).into_iter().map(|r| r.name).collect();
    assert_eq!(found, ["Sintel.2010.1080p.BluRay.x264-GRP"]);
    assert_eq!(cache.find(&BLURAY[1..7].parse().unwrap()), [], "a download ID prefix names no result");
    assert_eq!(cache.find(&"uffff00".parse().unwrap()), []);
}

#[tokio::test]
async fn a_block_page_is_not_an_empty_listing() {
    let block = ok(b"<html><title>Access denied</title><body>blocked by policy</body></html>".to_vec());
    let server = Server::start(vec![("/hydra/", vec![block])]).await;
    let search = Search::new(&server.config(&["hydra"]), ResultCache::new()).unwrap();

    let err = search.search(&query("debian 13", &["hydra"])).await.unwrap_err();

    assert_eq!(err.code, ErrorCode::ProvidersUnavailable);
    let Some(Details::Sources(reports)) = err.details else { panic!("no source reports") };
    assert_eq!(reports[0].outcome, SourceOutcome::BadResponse { reason: "expected an RSS feed, got <html>".into() });
}

#[tokio::test]
async fn the_plain_image_outranks_bigger_variants() {
    let server = Server::start(vec![("/geek/", vec![ok(fixture("newznab/debian-netinst.xml"))])]).await;
    let search = Search::new(&server.config(&["geek"]), ResultCache::new()).unwrap();

    let result = search.search(&query("debian 13.7 netinst", &["geek"])).await.unwrap();

    let listed: Vec<(&str, Option<u64>)> = result.releases.iter().map(|r| (r.name.as_str(), r.size)).collect();
    assert_eq!(
        listed,
        [
            ("debian-13.7.0-amd64-netinst.iso", Some(792_723_456)),
            ("debian-13.7.0-arm64-netinst.iso", Some(735_358_976)),
            ("debian-edu-13.7.0-amd64-netinst.iso", Some(801_112_064)),
            ("debian-mac-13.7.0-amd64-netinst.iso", Some(793_772_032)),
        ]
    );
}

#[tokio::test]
async fn progress_shows_each_answer_and_the_final_ranking_is_unchanged() {
    let late = |body: Vec<u8>, ms: u64| Reply { delay: Duration::from_millis(ms), ..ok(body) };
    let server = Server::start(vec![
        ("/hydra/", vec![ok(hydra_feed())]),
        ("/geek/", vec![late(fixture("newznab/geek-sintel.xml"), 300)]),
        ("/missing/", vec![Reply { delay: Duration::from_millis(600), ..status(404) }]),
    ])
    .await;
    let sources = ["hydra", "geek", "missing"];
    let mut q = query("sintel", &sources);
    q.limit = 200;
    let streamed = Search::new(&server.config(&sources), ResultCache::new()).unwrap();
    let mut updates: Vec<Progress> = Vec::new();

    let last = streamed.search_with_progress(&q, || q.profile.clone(), |p| updates.push(p)).await.unwrap();
    let whole = Search::new(&server.config(&sources), ResultCache::new()).unwrap().search(&q).await.unwrap();

    let seen: Vec<String> = updates
        .iter()
        .map(|p| format!("{:?} total {} pending {:?}", outcomes(&p.result), p.result.total, p.pending))
        .collect();
    assert_eq!(
        seen,
        [
            r#"[("hydra", Ok { results: 3 })] total 2 pending ["geek", "missing"]"#,
            r#"[("hydra", Ok { results: 3 }), ("geek", Ok { results: 3 })] total 5 pending ["missing"]"#,
        ]
    );
    assert_eq!(outcomes(&last)[2], ("missing".into(), SourceOutcome::Http { status: 404 }));
    assert_eq!(last.releases, whole.releases);
    assert_eq!(last.releases, updates[1].result.releases);
}

#[tokio::test]
async fn a_disabled_source_is_neither_searched_nor_nameable() {
    let server = Server::start(vec![("/hydra/", vec![ok(hydra_feed())])]).await;
    let config =
        SearchConfig { disabled: vec!["geek".into(), "planet".into()], ..server.config(&["hydra", "geek", "planet"]) };
    let search = Search::new(&config, ResultCache::new()).unwrap();

    let named = search.search(&query("sintel", &["hydra", "geek"])).await.unwrap_err();
    let unknown = Search::new(&SearchConfig { disabled: vec!["slug".into()], ..config.clone() }, ResultCache::new())
        .err()
        .unwrap();

    let names: Vec<String> = search.sources().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["hydra"]);
    assert_eq!((named.code, named.message.as_str()), (ErrorCode::Usage, "source \"geek\" is disabled"));
    assert_eq!(server.hits("/hydra/"), 0);
    assert_eq!(unknown.message, "sources.disabled lists an unknown source \"slug\"");
}

#[tokio::test]
async fn an_nzb_fetch_failure_never_quotes_the_link() {
    let nzb =
        br#"<?xml version="1.0" encoding="UTF-8"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"></nzb>"#.to_vec();
    let server = Server::start(vec![
        ("/getnzb/ok", vec![ok(nzb.clone())]),
        ("/getnzb/huge", vec![Reply { content_length: false, ..ok(vec![b'<'; 65 << 20]) }]),
    ])
    .await;
    let search = Search::new(&SearchConfig::default(), ResultCache::new()).unwrap();
    let link = |path: &str| format!("{}{path}?apikey=k3y", server.base);

    let fetched = search.fetch_nzb(&link("/getnzb/ok")).await.unwrap();
    let missing = search.fetch_nzb(&link("/getnzb/gone")).await.unwrap_err();
    let huge = search.fetch_nzb(&link("/getnzb/huge")).await.unwrap_err();
    let refused = search.fetch_nzb("http://127.0.0.1:9/getnzb?apikey=k3y").await.unwrap_err();
    let not_http = search.fetch_nzb("ftp://indexer.example/getnzb/1.nzb").await.unwrap_err();

    assert_eq!(fetched, nzb);
    assert_eq!((missing.code, missing.message.as_str()), (ErrorCode::FetchFailed, "could not fetch the NZB: HTTP 404"));
    assert_eq!(huge.message, "could not fetch the NZB: larger than 64 MiB");
    assert!(refused.message.starts_with("could not fetch the NZB: "), "{}", refused.message);
    assert_eq!(not_http.code, ErrorCode::InvalidRef);
    for error in [&missing, &huge, &refused] {
        assert!(!serde_json::to_string(error).unwrap().contains("k3y"), "{error:?}");
    }
}

#[tokio::test]
async fn restored_results_resolve_without_replacing_fresh_links() {
    let server = Server::start(vec![("/newznab/api", vec![ok(hydra_feed())])]).await;
    let cache = ResultCache::new();
    let search = Search::new(&keyed(&server.base, Some("k3y")), cache.clone()).unwrap();
    let result = search.search(&query("sintel", &["hydra"])).await.unwrap();
    let bluray = result.releases.iter().find(|r| r.id.to_ref() == BLURAY).unwrap().clone();
    let older =
        Release { id: ResultId::from_bytes([0x9b; 20]), name: "Sintel.2010.DVDRip.XviD".into(), ..bluray.clone() };

    cache.restore(vec![(bluray.clone(), None), (older, None)]);

    let private = cache.find(&BLURAY.parse().unwrap());
    assert_eq!(private[0].nzb_url.as_deref(), Some(BLURAY_NZB));
    let restored: Vec<String> = cache.find(&"u9b9b9b".parse().unwrap()).into_iter().map(|r| r.name).collect();
    assert_eq!(restored, ["Sintel.2010.DVDRip.XviD"]);
}

#[tokio::test]
async fn a_private_link_is_saved_without_its_key_and_takes_the_current_one_back() {
    let server = Server::start(vec![("/newznab/api", vec![ok(hydra_feed())])]).await;
    let cache = ResultCache::new();
    let search = Search::new(&keyed(&server.base, Some("k3y")), cache.clone()).unwrap();
    let result = search.search(&query("sintel", &["hydra"])).await.unwrap();
    let bluray = result.releases.iter().find(|r| r.id.to_ref() == BLURAY).unwrap().clone();
    let saved = cache.saved_link(&bluray.id);

    let next_engine = |config: SearchConfig| {
        let cache = ResultCache::new();
        cache.restore(vec![(bluray.clone(), saved.clone())]);
        let found = cache.find(&BLURAY.parse().unwrap()).remove(0);
        Search::new(&config, cache).unwrap().nzb_url(&found)
    };
    let rotated = next_engine(keyed(&server.base, Some("n3w")));
    let keyless = next_engine(keyed(&server.base, None)).unwrap_err();
    let gone = next_engine(SearchConfig::default()).unwrap_err();

    assert_eq!(
        saved,
        Some(SavedLink {
            source: "hydra".into(),
            url: "http://127.0.0.1:5076/getnzb/api/a1".into(),
            key_params: vec!["apikey".into()],
        })
    );
    assert_eq!(rotated.unwrap().as_deref(), Some("http://127.0.0.1:5076/getnzb/api/a1?apikey=n3w"));
    assert_eq!(
        [keyless.message, gone.message],
        [
            "this result's NZB link needs an API key for the hydra indexer, which has none",
            "this result's NZB link needs an API key for the hydra indexer, which is not configured",
        ]
    );
}

#[tokio::test]
async fn a_public_link_is_saved_whole_and_needs_no_key() {
    let server = Server::start(vec![("/geek/", vec![ok(fixture("newznab/geek-sintel.xml"))])]).await;
    let cache = ResultCache::new();
    let search = Search::new(&server.config(&["geek"]), cache.clone()).unwrap();
    let result = search.search(&query("sintel", &["geek"])).await.unwrap();
    let uhd = result.releases.iter().find(|r| r.name == "Sintel.2010.2160p.UHD.BluRay.x265-GRP").unwrap().clone();
    let saved = cache.saved_link(&uhd.id);

    let next = ResultCache::new();
    next.restore(vec![(uhd.clone(), saved.clone())]);
    let found = next.find(&uhd.id.to_ref().parse().unwrap()).remove(0);
    let url = Search::new(&server.config(&["geek"]), next.clone()).unwrap().nzb_url(&found);
    let without_geek = Search::new(&SearchConfig::default(), next).unwrap().nzb_url(&found);

    assert_eq!(uhd.id.to_ref(), "u280e147f1aa5f6ac87bc969511e6f21769a83ac1");
    assert_eq!(saved.map(|s| s.key_params), Some(Vec::new()));
    assert_eq!(url.unwrap().as_deref(), Some("https://geek.example/getnzb/g1.nzb"));
    assert_eq!(
        without_geek.unwrap().as_deref(),
        Some("https://geek.example/getnzb/g1.nzb"),
        "a link without a key needs no indexer to put one back"
    );
}

/// A Newznab feed of `(tag, name, grabs, size)` rows, each linking to an NZB named by its tag.
fn feed(rows: &[(&str, &str, u64, u64)]) -> Vec<u8> {
    let items: String = rows
        .iter()
        .map(|(tag, name, grabs, size)| {
            format!(
                r#"<item><title>{name}</title><link>https://indexer.example/getnzb/{tag}.nzb</link><newznab:attr name="category" value="2040" /><newznab:attr name="size" value="{size}" /><newznab:attr name="grabs" value="{grabs}" /></item>"#
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel>{items}</channel></rss>"#
    )
    .into_bytes()
}

#[tokio::test]
async fn a_title_searches_each_of_its_queries_and_reports_each_indexer_once() {
    let bluray = ("a", "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", 40, 6_000_000_000);
    let dated = feed(&[bluray, ("b", "Night of the Living Dead (1990) 1080p BluRay", 90, 9_000_000_000)]);
    let yearless = feed(&[bluray, ("c", "Night.of.the.Living.Dead.720p.WEB.x264", 12, 2_000_000_000)]);
    let late = |body: Vec<u8>| Reply { delay: Duration::from_millis(300), ..ok(body) };
    let server = Server::start(vec![
        ("/geek/api?t=search&q=night+of+the+living+dead+1968", vec![ok(dated)]),
        ("/geek/api?t=search&q=night+of+the+living+dead", vec![late(yearless)]),
        ("/geek/api?t=movie&imdbid=0063350", vec![ok(feed(&[]))]),
        ("/hydra/", vec![status(404)]),
    ])
    .await;
    let sources = ["hydra", "geek"];
    let wanted = Wanted {
        id: "tt0063350".parse().unwrap(),
        name: "Night of the Living Dead".into(),
        year: Some(1968),
        episode: None,
        namesakes: Namesakes::Leading,
        runtime: Some(Duration::from_secs(96 * 60)),
        season_episodes: None,
        countries: Default::default(),
    };
    let q = SearchQuery { wanted: Some(wanted), ..query("night of the living dead", &sources) };
    let search = Search::new(&server.config(&sources), ResultCache::new()).unwrap();
    let mut updates: Vec<Progress> = Vec::new();

    let result = search.search_with_progress(&q, || q.profile.clone(), |p| updates.push(p)).await.unwrap();

    let spans: Vec<(&str, String)> =
        result.releases.iter().map(|r| (r.name.as_str(), r.span.map(|s| s.to_string()).unwrap_or_default())).collect();
    assert_eq!(
        spans,
        [
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", "film".to_string()),
            ("Night.of.the.Living.Dead.720p.WEB.x264", "film".to_string()),
        ]
    );
    assert_eq!(
        outcomes(&result),
        [("hydra".into(), SourceOutcome::Http { status: 404 }), ("geek".into(), SourceOutcome::Ok { results: 4 })]
    );
    assert_eq!(updates.last().map(|p| p.pending.clone()), Some(vec!["geek".to_string()]));
    assert_eq!(server.hits("/geek/api?t=search&q=night+of+the+living+dead+1968"), 1);
    assert_eq!(server.hits("/geek/api?t=search&q=night+of+the+living+dead"), 1);
    assert_eq!(server.hits("/geek/api?t=movie&imdbid=0063350"), 1);
    assert_eq!(server.hits("/hydra/"), 3);
}

#[tokio::test]
async fn a_film_is_asked_for_by_its_id_and_a_release_tagged_as_it_fits_by_another_name() {
    let tagged = |rows: &[(&str, &str, &str)]| {
        let items: String = rows
            .iter()
            .map(|(tag, name, imdb)| {
                format!(
                    r#"<item><title>{name}</title><link>https://indexer.example/getnzb/{tag}.nzb</link><newznab:attr name="category" value="2040" /><newznab:attr name="size" value="6000000000" /><newznab:attr name="imdb" value="{imdb}" /></item>"#
                )
            })
            .collect();
        format!(
            r#"<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel>{items}</channel></rss>"#
        )
        .into_bytes()
    };
    let by_id = tagged(&[
        ("a", "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", "0063350"),
        ("d", "Die.Nacht.der.lebenden.Toten.1968.German.1080p.BluRay.x264-GRP", "63350"),
        ("f", "Living.Dead.Collection.1080p.BluRay", "tt0063350"),
    ]);
    // Words find a release the indexer tagged as the 1990 remake, which this film is not.
    let by_words = tagged(&[("e", "Night.of.the.Living.Dead.1968.1080p.WEB.x264-GRP", "0100258")]);
    let server = Server::start(vec![
        ("/geek/api?t=movie&imdbid=0063350&limit=300", vec![ok(by_id)]),
        ("/geek/api?t=search", vec![ok(by_words)]),
    ])
    .await;
    let wanted = Wanted {
        id: "tt0063350".parse().unwrap(),
        name: "Night of the Living Dead".into(),
        year: Some(1968),
        episode: None,
        namesakes: Namesakes::Leading,
        runtime: Some(Duration::from_secs(96 * 60)),
        season_episodes: None,
        countries: Default::default(),
    };
    let q = SearchQuery { wanted: Some(wanted), ..query("night of the living dead", &["geek"]) };
    let search = Search::new(&server.config(&["geek"]), ResultCache::new()).unwrap();

    let result = search.search(&q).await.unwrap();

    let names: Vec<&str> = result.releases.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
            "Die.Nacht.der.lebenden.Toten.1968.German.1080p.BluRay.x264-GRP"
        ]
    );
    assert_eq!(server.hits("/geek/api?t=movie&imdbid=0063350&limit=300"), 1);
}

#[tokio::test]
async fn rejects_unknown_sources_and_queries_without_words() {
    let search = Search::new(&indexers("http://127.0.0.1:9", &["hydra", "geek"]), ResultCache::new()).unwrap();

    let unknown = search.search(&query("sintel", &["slug"])).await.unwrap_err();
    let empty = search.search(&SearchQuery::new(" -- ")).await.unwrap_err();

    assert_eq!((unknown.code, unknown.message.as_str()), (ErrorCode::Usage, "unknown source \"slug\""));
    assert_eq!(unknown.hint.as_deref(), Some("available sources: hydra, geek"));
    assert_eq!((empty.code, empty.message.as_str()), (ErrorCode::Usage, "a search needs at least one word"));
}
