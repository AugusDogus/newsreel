use std::path::{Path, PathBuf};

use serde_json::Value;
use usenet::fake::{FakeServer, Posting};

mod common;
use common::{Home, Run, Server, hooks, ok, status};

/// One Newznab search result.
struct Item {
    title: &'static str,
    size: u64,
    grabs: u32,
    /// Where the NZB is, under the server's base URL.
    link: &'static str,
}

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// A Newznab answer listing `items`, each a movie, with links under `base`.
fn feed(base: &str, items: &[Item]) -> Vec<u8> {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\" \
         xmlns:newznab=\"http://www.newznab.com/DTD/2010/feeds/attributes/\"><channel><title>indexer</title>\n",
    );
    for item in items {
        let link = format!("{base}{}", item.link).replace('&', "&amp;");
        xml.push_str(&format!(
            "<item><title>{}</title><guid isPermaLink=\"false\">{}</guid><link>{link}</link>\
             <pubDate>Thu, 15 Oct 2026 14:00:32 -0000</pubDate>\
             <enclosure url=\"{link}\" length=\"{}\" type=\"application/x-nzb\"/>\
             <newznab:attr name=\"category\" value=\"2040\"/><newznab:attr name=\"size\" value=\"{}\"/>\
             <newznab:attr name=\"grabs\" value=\"{}\"/></item>\n",
            item.title, item.link, item.size, item.size, item.grabs
        ));
    }
    xml.push_str("</channel></rss>\n");
    xml.into_bytes()
}

impl Server {
    /// A `sources.newznab` entry for an indexer this server answers at `/{name}/api`.
    fn indexer(&self, name: &str, key_env: Option<&str>) -> String {
        let key = key_env.map(|env| format!("      api_key_env: {env}\n")).unwrap_or_default();
        format!("    - name: {name}\n      url: {}/{name}/api\n{key}", self.base)
    }
}

const PASSWORD_ENV: &str = "TEST_NNTP_PASSWORD";

impl Home {
    /// A home whose config lists `indexers` under `sources.newznab`.
    fn searching(indexers: &[String]) -> Home {
        let home = Home::new().with(hooks::DISK, "roomy").with(PASSWORD_ENV, usenet::fake::PASSWORD);
        let sources =
            if indexers.is_empty() { String::new() } else { format!("sources:\n  newznab:\n{}", indexers.concat()) };
        home.write_config(&sources);
        home
    }

    /// Adds a Usenet server to the config.
    fn served_by(self, server: &FakeServer) -> Home {
        let s = server.config(4, 0);
        let config = std::fs::read_to_string(self.path("th/config/config.yaml")).unwrap();
        self.write_config(&format!(
            "{config}usenet:\n  servers:\n    - host: {}\n      port: {}\n      tls: false\n      username: {}\n      \
             password_env: {PASSWORD_ENV}\n",
            s.host, s.port, s.username
        ));
        self
    }
}

fn sintel(base: &str) -> Vec<u8> {
    feed(
        base,
        &[
            Item { title: "Sintel.2010.720p.WEB.x264-GRP", size: 800 * MIB, grabs: 300, link: "/getnzb/b.nzb" },
            Item { title: "Sintel.2010.1080p.BluRay.x264-GRP", size: 4 * GIB, grabs: 900, link: "/getnzb/a.nzb" },
        ],
    )
}

fn sintel_4k(base: &str) -> Vec<u8> {
    feed(
        base,
        &[Item { title: "Sintel.2010.2160p.UHD.BluRay.x265-GRP", size: 20 * GIB, grabs: 120, link: "/getnzb/c.nzb" }],
    )
}

/// Rows of a plain search table, without the AGE column, which depends on today.
fn rows(run: &Run) -> Vec<String> {
    let lines: Vec<&str> = run.stdout.lines().take_while(|l| !l.starts_with('#')).collect();
    let age = lines[0].split('\t').position(|h| h == "AGE").unwrap();
    lines
        .iter()
        .map(|l| l.split('\t').enumerate().filter(|(i, _)| *i != age).map(|(_, c)| c).collect::<Vec<_>>().join(" | "))
        .collect()
}

fn locators(found: &Value) -> Vec<String> {
    found["result"]["releases"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()[..7].to_string()).collect()
}

#[test]
fn search_ranks_what_the_indexers_answered() {
    let server =
        Server::start(|base| vec![("/hydra/api", vec![ok(sintel(base))]), ("/geek/api", vec![ok(sintel_4k(base))])]);
    let home = Home::searching(&[server.indexer("hydra", None), server.indexer("geek", None)]);

    let plain = home.run(&["search", "sintel"]);
    let json = home.run(&["search", "sintel", "--json"]).json();

    assert_eq!(plain.code, 0, "{}", plain.stderr);
    let table = rows(&plain);
    assert_eq!(table[0], "ID | GRABS | SIZE | QUALITY | SOURCE | FLAGS | NAME");
    let named: Vec<String> =
        table[1..].iter().map(|r| r.split(" | ").skip(1).collect::<Vec<_>>().join(" | ")).collect();
    assert_eq!(
        named,
        [
            "900 | 4.0 GiB | 1080p h264 | hydra | - | Sintel.2010.1080p.BluRay.x264-GRP",
            "300 | 800 MiB | 720p h264 | hydra | - | Sintel.2010.720p.WEB.x264-GRP",
            "120 | 20.0 GiB | 2160p hevc | geek | - | Sintel.2010.2160p.UHD.BluRay.x265-GRP",
        ],
        "ranked for a 1080p screen: closest resolution first"
    );
    let result = &json["result"];
    let grabs: Vec<u64> = result["releases"].as_array().unwrap().iter().map(|r| r["grabs"].as_u64().unwrap()).collect();
    assert_eq!(grabs, [900, 300, 120]);
    assert!(locators(&json).iter().all(|id| id.starts_with('u')), "results are named by their NZB link");
    let sources: Vec<String> =
        result["sources"].as_array().unwrap().iter().map(|s| format!("{} {}", s["source"], s["outcome"])).collect();
    assert_eq!(sources, [r#""hydra" "cached""#, r#""geek" "cached""#]);
    assert_eq!((server.asked("/hydra/").len(), server.asked("/geek/").len()), (1, 1));
}

/// A release posted to `server`, as one file of `size` bytes.
fn post(server: &FakeServer, title: &str, file: &str, size: usize) -> Posting {
    let data: Vec<u8> = (0..size as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let posting = Posting::new(title, &[(file, &data)], 16 * 1024);
    server.add(&posting);
    posting
}

#[test]
fn get_on_a_search_result_downloads_into_the_configured_folder() {
    let usenet = FakeServer::start();
    let posting = post(&usenet, "Sintel.2010.1080p.BluRay.x264-GRP", "Sintel.2010.1080p.BluRay.x264-GRP.mkv", 300_000);
    let server = Server::start(|base| {
        vec![("/hydra/api", vec![ok(sintel(base))]), ("/getnzb/a.nzb", vec![ok(posting.nzb.clone())])]
    });
    let home = Home::searching(&[server.indexer("hydra", None)]).served_by(&usenet);
    let films = home.path("films");
    home.write_config(&format!(
        "{}downloads:\n  dir: '{}'\n",
        std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap(),
        films.display()
    ));

    let found = home.run(&["search", "sintel", "1080p", "--json"]).json();
    let release = &found["result"]["releases"][0];
    let locator = &release["id"].as_str().unwrap()[..7];
    let get = home.run(&["get", locator]);
    let id = get.stdout.split_whitespace().nth(1).unwrap_or_default().to_string();
    let status = home.run(&["status", &id, "--json"]).json();
    // The result's ID keeps naming the download it became.
    let by_result = home.run(&["status", locator, "--json"]).json();
    let wait = home.run(&["wait", locator, "--timeout", "60s"]);
    let again = home.run(&["get", locator, "--json"]).json();
    let files = home.run(&["files", locator, "--json"]).json();

    assert_eq!(release["name"], "Sintel.2010.1080p.BluRay.x264-GRP");
    assert_eq!(get.code, 0, "{}{}", get.stdout, get.stderr);
    assert!(get.stdout.starts_with(&format!("started {id} Sintel.2010.1080p.BluRay.x264-GRP.mkv\n")), "{}", get.stdout);
    let job = &status["jobs"][0];
    assert_eq!(job["dir"].as_str(), films.to_str());
    assert_eq!(job["name"], "Sintel.2010.1080p.BluRay.x264-GRP.mkv");
    assert_eq!(by_result["jobs"][0]["id"].as_str().map(|full| &full[..6]), Some(id.as_str()));
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(again["reply"]["outcome"], "existing", "{again:#}");
    assert_eq!(files["files"]["source"], "download");
    assert_eq!(server.asked("/getnzb/").len(), 1, "the NZB is fetched once, when the download starts");
}

#[test]
fn private_indexer_links_never_reach_the_output() {
    let usenet = FakeServer::start();
    let posting = post(&usenet, "Ubuntu.Private", "ubuntu-private.iso", 100_000);
    let server = Server::start(|base| {
        let feed = String::from_utf8(feed(
            base,
            &[Item { title: "ubuntu-private.iso", size: 100_000, grabs: 4, link: "/getnzb/x.nzb?apikey=k3y" }],
        ))
        .unwrap()
        .replace("</title><guid", "</title><comments>https://indexer.example/details/7?passkey=s3cret</comments><guid");
        vec![
            ("/hydra/api", vec![ok(feed.into_bytes())]),
            ("/getnzb/", vec![status(404), status(404), ok(posting.nzb.clone())]),
        ]
    });
    let mut home = Home::searching(&[server.indexer("hydra", Some("TEST_HYDRA_KEY"))]).served_by(&usenet);
    home.set("TEST_HYDRA_KEY", "k3y");

    let plain = home.run(&["search", "ubuntu"]);
    let json = home.run(&["search", "ubuntu", "--json"]);
    let locator = locators(&json.json())[0].clone();
    let refused = home.run(&["get", &locator]);
    let refused_json = home.run(&["get", &locator, "--json"]);
    let started = home.run(&["get", &locator]);
    let job = started.stdout.split_whitespace().nth(1).unwrap_or_default().to_string();
    let status = home.run(&["status"]);
    let status_json = home.run(&["status", "--json"]);
    let files = home.run(&["files", &job]);
    let files_json = home.run(&["files", &job, "--json"]);

    assert_eq!(plain.code, 0, "{}", plain.stderr);
    assert_eq!(refused.code, 5);
    assert!(refused.stderr.contains("could not fetch the NZB: HTTP 404"), "{}", refused.stderr);
    assert_eq!(refused_json.json()["error"]["code"], "fetch_failed");
    assert_eq!(started.code, 0, "{}{}", started.stdout, started.stderr);
    assert_eq!(files_json.json()["files"]["name"], "ubuntu-private.iso");
    let runs = [&plain, &json, &refused, &refused_json, &started, &status, &status_json, &files, &files_json];
    for run in runs {
        let out = format!("{}{}", run.stdout, run.stderr);
        assert!(
            !out.contains("k3y") && !out.contains("apikey") && !out.contains("s3cret"),
            "a private link reached the output: {out}"
        );
    }
    assert_eq!(server.asked("/hydra/api"), ["/hydra/api?t=search&q=ubuntu&apikey=k3y"]);
    assert_eq!(server.asked("/getnzb/"), vec!["/getnzb/x.nzb?apikey=k3y"; 3]);
}

#[test]
fn a_private_locator_resolves_after_a_restart_with_the_key_from_the_environment() {
    let usenet = FakeServer::start();
    let posting = post(&usenet, "Ubuntu.Private", "ubuntu-private.iso", 100_000);
    let server = Server::start(|base| {
        let item = |link| Item { title: "ubuntu-private.iso", size: 100_000, grabs: 4, link };
        let items =
            [item("/getnzb/x.nzb?apikey=k3y"), item("/getnzb/y.nzb?apikey=k3y"), item("/getnzb/z.nzb?apikey=k3y")];
        vec![("/hydra/api", vec![ok(feed(base, &items))]), ("/getnzb/", vec![ok(posting.nzb.clone())])]
    });
    let mut home = Home::searching(&[server.indexer("hydra", Some("TEST_HYDRA_KEY"))]).served_by(&usenet);
    let config = std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap();
    home.set("TEST_HYDRA_KEY", "k3y");
    let name = |run: &Run| run.json()["files"]["name"].as_str().map(String::from);

    let found = home.run(&["search", "ubuntu", "--json"]).json();
    // Each step reads a result whose NZB no earlier step fetched, so none is answered from the cache.
    let [first, second, third] = <[String; 3]>::try_from(locators(&found)).unwrap();
    let before = home.run(&["files", &first, "--json"]);
    home.run(&["daemon", "stop"]);
    let on_disk: Vec<String> = walk(&home.path("th"))
        .into_iter()
        .filter(|path| std::fs::read(path).unwrap().windows(3).any(|w| w == b"k3y"))
        .map(|path| path.display().to_string())
        .collect();
    home.set("TEST_HYDRA_KEY", "n3w");
    let cached = home.run(&["files", &first, "--json"]);
    let after = home.run(&["files", &second, "--json"]);
    home.run(&["daemon", "stop"]);
    let fetched = server.asked("/getnzb/");
    home.write_config(&config.replace("      api_key_env: TEST_HYDRA_KEY\n", ""));
    let keyless = home.run(&["files", &third]);
    home.run(&["daemon", "stop"]);
    home.write_config(&config.replace(
        &server.indexer("hydra", Some("TEST_HYDRA_KEY")),
        "    - name: other\n      url: http://127.0.0.1:9/api\n",
    ));
    let removed = home.run(&["files", &third]);
    home.run(&["daemon", "stop"]);
    home.write_config(&config);
    home.unset("TEST_HYDRA_KEY");
    let unset = home.run(&["files", &third]);

    assert_eq!(name(&before), Some("ubuntu-private.iso".into()), "{}{}", before.stdout, before.stderr);
    assert_eq!(on_disk, Vec::<String>::new(), "the API key reached the disk");
    assert_eq!(name(&cached), Some("ubuntu-private.iso".into()), "{}{}", cached.stdout, cached.stderr);
    assert_eq!(name(&after), Some("ubuntu-private.iso".into()), "{}{}", after.stdout, after.stderr);
    let keys: Vec<(&str, &str)> = fetched.iter().filter_map(|path| path.split_once("?apikey=")).collect();
    assert!(
        matches!(keys.as_slice(), [(a, "k3y"), (b, "n3w")] if a != b),
        "an NZB already fetched is not fetched again, and a new one takes the key set now: {fetched:?}"
    );
    assert_eq!(
        (keyless.code, keyless.stderr.as_str()),
        (
            2,
            "error[usage]: this result's NZB link needs an API key for the hydra indexer, which has none\n\
             hint: give hydra its key with newsreel indexers add <its address> --key <key>, or search again\n"
        )
    );
    assert_eq!(
        (removed.code, removed.stderr.as_str()),
        (
            2,
            "error[usage]: this result's NZB link needs an API key for the hydra indexer, which is not \
             configured\nhint: add hydra back with newsreel indexers add, or search again\n"
        )
    );
    let config_file = home.path("th/config/config.yaml").display().to_string();
    assert_eq!(
        (unset.code, unset.stderr.as_str()),
        (
            2,
            &*format!(
                "error[usage]: {config_file} line 5: the \"hydra\" indexer takes its API key from TEST_HYDRA_KEY, \
                 which is not set for the engine; write the key as api_key instead\nhint: fix {config_file}\n"
            )
        )
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() { walk(&path) } else { vec![path] }
        })
        .collect()
}

#[test]
fn the_engine_refuses_a_config_that_does_not_parse() {
    let home = Home::searching(&[]);
    home.write_config("sources:\n  newznab: 3\n");

    let refused = home.run(&["status"]);
    let doctor = home.run(&["doctor", "--json"]).json();
    home.write_config(
        "sources:\n  disabled: [hydra]\n  newznab:\n    - name: hydra\n      url: http://127.0.0.1:9/api\n",
    );
    let fixed = home.run(&["status"]);
    let fixed_doctor = home.run(&["doctor", "--json"]).json();

    let file = home.path("th/config/config.yaml").display().to_string();
    let message = format!("{file} line 2: expected sequence start");
    assert_eq!(refused.code, 2, "{}{}", refused.stdout, refused.stderr);
    assert!(refused.stderr.contains(&message), "{}", refused.stderr);
    let config = |doctor: &Value| {
        let check = doctor["checks"].as_array().unwrap().iter().find(|c| c["check"] == "config").unwrap();
        (check["status"].as_str().unwrap().to_string(), check["detail"].as_str().unwrap().to_string())
    };
    assert_eq!(config(&doctor), ("fail".to_string(), message));
    assert_eq!(fixed.code, 0, "{}", fixed.stderr);
    assert_eq!(config(&fixed_doctor), ("ok".to_string(), file));
}

#[test]
fn doctor_network_asks_each_indexer() {
    let server = Server::start(|base| {
        let linux = feed(
            base,
            &[
                Item { title: "Linux.Foundation.Talks.2024.1080p", size: 2 * GIB, grabs: 10, link: "/getnzb/l1.nzb" },
                Item { title: "Linux.Basics.2023.720p", size: GIB, grabs: 3, link: "/getnzb/l2.nzb" },
            ],
        );
        vec![
            ("/down/api", vec![status(503)]),
            ("/empty/api", vec![ok(feed(base, &[]))]),
            ("/geek/api", vec![ok(linux)]),
        ]
    });
    let home =
        Home::searching(&[server.indexer("down", None), server.indexer("empty", None), server.indexer("geek", None)]);

    let doctor = home.run(&["doctor", "--network", "--json"]).json();

    let sources: Vec<(String, String, String)> = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["check"].as_str().unwrap().starts_with("source "))
        .map(|c| {
            let detail = c["detail"].as_str().unwrap();
            let (outcome, took) = detail.rsplit_once(", ").unwrap();
            assert!(took.ends_with(" ms") && took[..took.len() - 3].parse::<u32>().is_ok(), "{detail}");
            (c["check"].as_str().unwrap().to_string(), c["status"].as_str().unwrap().to_string(), outcome.to_string())
        })
        .collect();
    let expected =
        [("source down", "fail", "HTTP 503"), ("source empty", "ok", "0 results"), ("source geek", "ok", "2 results")];
    assert_eq!(sources, expected.map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string())));
    let summary = doctor["checks"].as_array().unwrap().iter().find(|c| c["check"] == "sources").unwrap();
    assert_eq!((&summary["status"], &summary["detail"]), (&"fail".into(), &"2 of 3 responded".into()));
    assert_eq!(server.asked("/geek/"), ["/geek/api?t=search&q=linux"]);
}

#[test]
fn the_machine_and_preferences_rank_a_film_search_and_say_so() {
    let server = Server::start(|base| {
        let films = [
            ("Night.of.the.Living.Dead.1968.2160p.UHD.BluRay.HDR.x265-GRP", 50 * GIB, 40),
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", 8 * GIB, 300),
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.HEVC.x265-GRP", 4 * GIB, 120),
            ("Night.of.the.Living.Dead.1968.1080p.WEB.H264-GRP", 1800 * MIB, 90),
            ("Night.of.the.Living.Dead.1968.720p.BluRay.x264-GRP", GIB, 60),
            ("Night.of.the.Living.Dead.1968.DVDRip.XviD-GRP", 700 * MIB, 20),
        ];
        let items: Vec<Item> = films
            .iter()
            .enumerate()
            .map(|(i, (title, size, grabs))| Item {
                title,
                size: *size,
                grabs: *grabs,
                link: ["/n/0", "/n/1", "/n/2", "/n/3", "/n/4", "/n/5"][i],
            })
            .collect();
        vec![("/hydra/api", vec![ok(feed(base, &items))])]
    });
    let home = Home::searching(&[server.indexer("hydra", None)]);
    let search = ["search", "night", "of", "the", "living", "dead", "1968", "--category", "video", "--limit", "6"];
    let shown = |run: &Run| -> Vec<String> {
        assert_eq!(run.code, 0, "{}", run.stderr);
        let lines: Vec<&str> = run.stdout.lines().collect();
        let header: Vec<&str> = lines[0].split('\t').collect();
        let columns: Vec<usize> =
            ["SIZE", "QUALITY", "NAME"].iter().filter_map(|c| header.iter().position(|h| h == c)).collect();
        let rows = lines.iter().take_while(|l| !l.starts_with('#')).map(|l| {
            let cells: Vec<&str> = l.split('\t').collect();
            columns.iter().map(|i| cells[*i]).collect::<Vec<_>>().join(" | ")
        });
        let summary = lines.iter().filter(|l| l.starts_with('#')).skip(1).map(|l| l.to_string());
        rows.chain(summary).collect()
    };

    let before = home.run(&search);
    assert_eq!(home.run(&["setup", "--screen", "laptop", "--max-size", "2GB", "--playback", "h264"]).code, 0);
    let after = home.run(&search);

    assert_eq!(
        shown(&before),
        [
            "SIZE | QUALITY | NAME",
            "8.0 GiB | 1080p h264 | Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
            "4.0 GiB | 1080p hevc | Night.of.the.Living.Dead.1968.1080p.BluRay.HEVC.x265-GRP",
            "1.8 GiB | 1080p h264 | Night.of.the.Living.Dead.1968.1080p.WEB.H264-GRP",
            "1.0 GiB | 720p h264 | Night.of.the.Living.Dead.1968.720p.BluRay.x264-GRP",
            "50.0 GiB | 2160p hevc hdr | Night.of.the.Living.Dead.1968.2160p.UHD.BluRay.HDR.x265-GRP",
            "700 MiB | sd | Night.of.the.Living.Dead.1968.DVDRip.XviD-GRP",
            "# ranked for this Mac: 1080p screen, HDR, HEVC, arm64, English",
        ]
    );
    assert_eq!(
        shown(&after),
        [
            "SIZE | QUALITY | NAME",
            "1.8 GiB | 1080p h264 | Night.of.the.Living.Dead.1968.1080p.WEB.H264-GRP",
            "1.0 GiB | 720p h264 | Night.of.the.Living.Dead.1968.720p.BluRay.x264-GRP",
            "700 MiB | sd | Night.of.the.Living.Dead.1968.DVDRip.XviD-GRP",
            "8.0 GiB | 1080p h264 | Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
            "4.0 GiB | 1080p hevc | Night.of.the.Living.Dead.1968.1080p.BluRay.HEVC.x265-GRP",
            "50.0 GiB | 2160p hevc hdr | Night.of.the.Living.Dead.1968.2160p.UHD.BluRay.HDR.x265-GRP",
            "# ranked for this Mac: 1080p screen, no HDR, H.264 only, up to 2 GB, arm64, English · overrides: screen \
             laptop, max_size 2GB, playback h264",
        ]
    );
}
