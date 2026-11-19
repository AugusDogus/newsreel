//! `newsreel indexers` against a stand-in Newznab indexer and a stand-in Prowlarr: the engine
//! checks an address before it writes it to config.yaml, and searches it at once.

use serde_json::{Value, json};

mod common;
use common::{Home, Server, fixture, hooks, ok, status};

const CAPS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <server title="NZBHydra 2" />
  <categories>
    <category id="2000" name="Movies"><subcat id="2040" name="Movies/HD" /></category>
    <category id="5000" name="TV" />
  </categories>
</caps>"#;
const BAD_KEY: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><error code="100" description="Invalid API Key" />"#;
const PAGE: &[u8] = b"<!doctype html><html><head><title>Prowlarr</title></head><body></body></html>";

/// A home whose engine searches no indexer yet.
fn home() -> Home {
    let home = Home::new().with(hooks::DISK, "roomy");
    home.write_config("# mine\nupdates: off\n");
    home
}

/// The code and message of a command that fails, and everything it printed.
fn refused(home: &Home, args: &[&str]) -> (i32, String, String, String) {
    let run = home.run(&[args, &["--json"]].concat());
    let printed = format!("{}{}", run.stdout, run.stderr);
    let error = &run.json()["error"];
    (run.code, error["code"].as_str().unwrap().into(), error["message"].as_str().unwrap().into(), printed)
}

fn names(list: &Value) -> Vec<&str> {
    list["indexers"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect()
}

#[test]
fn an_indexer_is_checked_then_searched_at_once_and_its_key_is_never_printed() {
    let server = Server::start(|_| {
        vec![
            ("/newznab/api?t=caps&apikey=k3y", vec![ok(CAPS.to_vec())]),
            ("/newznab/api?t=caps", vec![ok(BAD_KEY.to_vec())]),
            ("/newznab/api?t=search", vec![ok(fixture("newznab/hydra-sintel.xml"))]),
            ("/page", vec![ok(PAGE.to_vec())]),
        ]
    });
    let home = home();
    let endpoint = format!("{}/newznab/api", server.base);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let (missing, page, unreachable) =
        (format!("{}/missing", server.base), format!("{}/page", server.base), format!("http://{closed}/api"));
    let cases: [(&[&str], &str, String); 5] = [
        (&["indexers", "add", &endpoint], "usage", format!("{endpoint}: it needs an API key")),
        (&["indexers", "add", &endpoint, "--key", "n0pe"], "usage", format!("{endpoint}: it refused the API key")),
        (
            &["indexers", "add", &missing, "--key", "k3y"],
            "usage",
            format!("{missing} does not answer as a Newznab indexer: it answered HTTP 404"),
        ),
        (
            &["indexers", "add", &page, "--key", "k3y"],
            "usage",
            format!("{page} does not answer as a Newznab indexer: it answered a web page, not Newznab"),
        ),
        (
            &["indexers", "add", &unreachable, "--key", "k3y"],
            "fetch_failed",
            format!("{unreachable}: cannot reach it: "),
        ),
    ];
    for (args, code, message) in &cases {
        let (_, kind, said, printed) = refused(&home, args);
        assert_eq!(kind, *code, "{args:?}");
        assert!(said.starts_with(message.as_str()), "{args:?}: {said}");
        assert!(!printed.contains("n0pe") && !printed.contains("k3y"), "{args:?} printed a key: {printed}");
    }
    assert_eq!(std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap().matches("newznab").count(), 0);

    let added = home.run(&["indexers", "add", &endpoint, "--key", "k3y", "--name", "hydra", "--json"]);
    assert_eq!(added.code, 0, "{}{}", added.stdout, added.stderr);
    assert!(!added.stdout.contains("k3y"), "{}", added.stdout);
    assert_eq!(
        added.json()["indexers"],
        json!([{
            "name": "hydra",
            "url": endpoint,
            "key": "config",
            "check": {"state": "answers", "categories": [{"id": 2000, "name": "Movies"}, {"id": 5000, "name": "TV"}]},
        }])
    );
    assert_eq!(added.json()["added"], json!(["hydra"]));
    let config = std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap();
    assert_eq!(
        config,
        format!(
            "# mine\nupdates: off\n\nsources:\n  newznab:\n    - name: hydra\n      url: {endpoint}\n      api_key: k3y\n"
        )
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path("th/config/config.yaml")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only this user reads the key");
    }

    let search = home.json(&["search", "sintel", "--source", "hydra", "--json"]);
    let names_found: Vec<&str> =
        search["result"]["releases"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names_found.contains(&"Sintel.2010.1080p.BluRay.x264-GRP"), "{search:#}");
    let asked = server.asked("/newznab/api?t=search");
    assert!(asked.iter().all(|path| path.contains("apikey=k3y")), "{asked:?}");
    let plain = home.run(&["search", "sintel", "--source", "hydra"]);
    assert!(!plain.stdout.contains("k3y"), "{}", plain.stdout);

    let listed = home.run(&["indexers"]);
    assert_eq!(
        listed.stdout.lines().take(2).collect::<Vec<_>>(),
        ["NAME\tANSWERS\tKEY\tCATEGORIES\tURL", &format!("hydra\tyes\tconfig.yaml\tMovies, TV\t{endpoint}")]
    );
    let again = home.json(&["indexers", "add", &endpoint, "--key", "k3y", "--json"]);
    assert_eq!(
        (names(&again), &again["added"], &again["updated"]),
        (vec!["hydra"], &json!([]), &json!(["hydra: unchanged"])),
        "adding it again changes nothing, and says so"
    );

    let removed = home.json(&["indexers", "remove", "hydra", "--json"]);
    assert_eq!(removed["indexers"], json!([]));
    let (_, kind, said, _) = refused(&home, &["search", "sintel", "--source", "hydra"]);
    assert_eq!((kind.as_str(), said.as_str()), ("usage", "unknown source \"hydra\""));
    let (_, kind, said, _) = refused(&home, &["indexers", "remove", "hydra"]);
    assert_eq!((kind.as_str(), said.as_str()), ("not_found", "no indexer is named \"hydra\""));
    assert!(!home.log().contains("k3y"), "{}", home.log());
}

/// NZBHydra's address without `/api` answers its web page.
#[test]
fn an_indexers_base_address_is_added_as_its_api() {
    let server =
        Server::start(|_| vec![("/hydra/api?t=caps", vec![ok(CAPS.to_vec())]), ("/hydra", vec![ok(PAGE.to_vec())])]);
    let home = home();
    let base = format!("{}/hydra", server.base);

    let added = home.run(&["indexers", "add", &base, "--key", "k3y", "--json"]);

    assert_eq!(added.code, 0, "{}{}", added.stdout, added.stderr);
    assert_eq!(added.json()["indexers"][0]["url"], format!("{base}/api"));
    let config = std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap();
    assert!(config.contains(&format!("url: {base}/api\n")), "{config}");
}

/// `indexers add` with the address of an indexer already in config.yaml, written there without
/// `/api` and keyed from a variable, made a second entry with the key in plain text, and said only
/// "added".
#[test]
fn adding_an_indexer_already_there_keeps_its_name_and_key_variable_and_says_what_changed() {
    let server =
        Server::start(|_| vec![("/hydra/api?t=caps", vec![ok(CAPS.to_vec())]), ("/hydra", vec![ok(PAGE.to_vec())])]);
    let mut home = home();
    home.set("HYDRA_KEY", "k3y");
    let base = format!("{}/hydra", server.base);
    home.write_config(&format!(
        "# mine\nupdates: off\nsources:\n  newznab:\n    - name: hydrabase\n      url: {base}\n      api_key_env: HYDRA_KEY\n"
    ));
    let config = || std::fs::read_to_string(home.path("th/config/config.yaml")).unwrap();

    let same = home.run(&["indexers", "add", &base, "--key", "k3y"]);
    assert_eq!(same.code, 0, "{}{}", same.stdout, same.stderr);
    assert_eq!(config().matches("- name:").count(), 1, "{}", config());
    assert!(config().contains("name: hydrabase") && config().contains("api_key_env: HYDRA_KEY"), "{}", config());
    assert!(!config().contains("k3y"), "{}", config());
    assert!(!same.stdout.contains("# added"), "{}", same.stdout);
    assert!(same.stdout.contains(&format!("# updated hydrabase: its address is now {base}/api\n")), "{}", same.stdout);

    let other = home.run(&["indexers", "add", &format!("{base}/api"), "--key", "n3w", "--name", "hydra"]);
    assert_eq!(other.code, 0, "{}{}", other.stdout, other.stderr);
    assert!(config().contains("name: hydra\n") && config().contains("api_key: n3w"), "{}", config());
    assert!(
        other.stdout.contains(
            "# updated hydra: renamed from hydrabase, and its key is now written in config.yaml, in place of $HYDRA_KEY\n"
        ),
        "{}",
        other.stdout
    );
}

#[test]
fn prowlarrs_own_address_adds_each_enabled_usenet_indexer() {
    let listed = json!([
        {"id": 1, "name": "NZBgeek", "enable": true, "protocol": "usenet", "privacy": "private"},
        {"id": 7, "name": "DrunkenSlug", "enable": true, "protocol": "usenet", "privacy": "private"},
        {"id": 3, "name": "FileList.io", "enable": true, "protocol": "torrent", "privacy": "private"},
        {"id": 4, "name": "Old", "enable": false, "protocol": "usenet", "privacy": "public"},
    ]);
    let server = Server::start(|_| {
        vec![
            ("/prowlarr/api/v1/system/status?apikey=pk", vec![ok(br#"{"appName": "Prowlarr"}"#.to_vec())]),
            ("/prowlarr/api/v1/system/status", vec![status(401)]),
            ("/prowlarr/api/v1/indexer?apikey=pk", vec![ok(listed.to_string().into_bytes())]),
            ("/prowlarr/1/api?t=caps&apikey=pk", vec![ok(CAPS.to_vec())]),
            ("/prowlarr/7/api", vec![status(500)]),
            ("/prowlarr", vec![ok(PAGE.to_vec())]),
        ]
    });
    let home = home();
    let prowlarr = format!("{}/prowlarr", server.base);

    let (_, kind, said, printed) = refused(&home, &["indexers", "add", &prowlarr, "--key", "n0pe"]);
    assert_eq!((kind.as_str(), said), ("usage", format!("{prowlarr}: it refused the API key")));
    assert!(!printed.contains("n0pe"), "{printed}");

    let added = home.json(&["indexers", "add", &prowlarr, "--key", "pk", "--json"]);
    assert_eq!(added["added"], json!(["nzbgeek", "drunkenslug"]));
    let checks: Vec<(&str, &str, &Value)> = added["indexers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["name"].as_str().unwrap(), i["url"].as_str().unwrap(), &i["check"]))
        .collect();
    assert_eq!(
        checks,
        [
            (
                "nzbgeek",
                format!("{prowlarr}/1/api").as_str(),
                &json!({"state": "answers", "categories": [{"id": 2000, "name": "Movies"}, {"id": 5000, "name": "TV"}]})
            ),
            (
                "drunkenslug",
                format!("{prowlarr}/7/api").as_str(),
                &json!({"state": "fails", "reason": "it answered HTTP 500"})
            ),
        ]
    );
    let again = home.json(&["indexers", "add", &prowlarr, "--key", "pk", "--json"]);
    assert_eq!(names(&again), ["nzbgeek", "drunkenslug"], "a second import adds nothing new");
    let (_, kind, said, _) = refused(&home, &["indexers", "add", &prowlarr, "--key", "pk", "--name", "ro"]);
    assert_eq!(
        (kind.as_str(), said),
        ("usage", format!("Prowlarr at {prowlarr} has 2 indexers, and --name names one"))
    );
}
