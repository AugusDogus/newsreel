//! Downloads and streams through the real binary from loopback NNTP servers.
// Its locked folders use Unix permission bits.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use newsreel_core::Target;
use newsreel_core::protocol::{PlayRequest, Request};
use newsreel_usenet::fake::{FakeServer, Posting};
use serde_json::Value;
use sha2::Digest;

mod common;
use common::{Home, alive, eventually, hooks, kill};

/// Bytes per article.
const PART: usize = 16 * 1024;
/// Bytes per second across a slow server's connections.
const SLOW: u64 = 256 * 1024;
const PASSWORD_ENV: &str = "NEWSREEL_TEST_NNTP_PASSWORD";

impl Home {
    fn served(server: &FakeServer) -> Self {
        Self::served_by(&[(server, 0)])
    }

    /// Servers and their priority; lower is asked first.
    fn served_by(servers: &[(&FakeServer, i32)]) -> Self {
        let mut config = String::from("usenet:\n  servers:\n");
        for (server, priority) in servers {
            let s = server.config(4, *priority);
            config.push_str(&format!(
                "    - host: {}\n      port: {}\n      tls: false\n      username: {}\n      password_env: {PASSWORD_ENV}\n      connections: 4\n      priority: {priority}\n",
                s.host, s.port, s.username
            ));
        }
        let home = Home::new().with(hooks::DISK, "roomy").with(PASSWORD_ENV, newsreel_usenet::fake::PASSWORD);
        home.write_config(&config);
        home
    }

    fn ok(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        self.json(&args)
    }

    fn job(&self, id: &str) -> Value {
        self.ok(&["status", id])["jobs"][0].clone()
    }

    fn until(&self, id: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let job = self.job(id);
            if done(&job) {
                return job;
            }
            assert!(Instant::now() < deadline, "timed out waiting until {what}: {job:#}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn kill_engine(&self) {
        let pid = self.ok(&["daemon", "status"])["pid"].as_i64().expect("the engine runs") as i32;
        kill(pid);
        eventually("the killed engine is gone", Duration::from_secs(5), || !alive(pid));
    }

    fn downloads(&self) -> PathBuf {
        self.path("dl")
    }

    fn get(&self, target: &str, extra: &[&str]) -> Value {
        let dir = self.downloads().display().to_string();
        let mut args = vec!["get", target, "--dir", &dir];
        args.extend(extra);
        self.ok(&args)
    }
}

/// A release's files, as posted to a server and written to an NZB file.
struct Release {
    dir: tempfile::TempDir,
    name: &'static str,
    files: BTreeMap<&'static str, Vec<u8>>,
    posting: Posting,
}

impl Release {
    fn new(name: &'static str, files: &[(&'static str, usize)], seed: u64) -> Self {
        let mut state = seed | 1;
        let mut contents = BTreeMap::new();
        for &(file, size) in files {
            let bytes: Vec<u8> = (0..size)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect();
            contents.insert(file, bytes);
        }
        let posted: Vec<(&str, &[u8])> = contents.iter().map(|(f, b)| (*f, b.as_slice())).collect();
        let posting = Posting::new(name, &posted, PART);
        Self { dir: tempfile::tempdir().unwrap(), name, files: contents, posting }
    }

    fn on(self, server: &FakeServer) -> Self {
        server.add(&self.posting);
        self
    }

    /// Puts only each file's first article on `server`, which is all that resolving the NZB reads.
    fn first_articles_on(self, server: &FakeServer) -> Self {
        server.add(&self.posting);
        for (_, ids) in &self.posting.files {
            for id in &ids[1..] {
                server.remove(id);
            }
        }
        self
    }

    fn id(&self) -> String {
        hex(&newsreel_usenet::Nzb::parse(&self.posting.nzb).unwrap().sha1)
    }

    fn short(&self) -> String {
        self.id()[..6].to_string()
    }

    fn nzb_file(&self) -> String {
        let path = self.dir.path().join(format!("{}.nzb", self.name));
        std::fs::write(&path, &self.posting.nzb).unwrap();
        path.display().to_string()
    }

    fn total(&self) -> u64 {
        self.files.values().map(|b| b.len() as u64).sum()
    }

    /// The message-IDs of a posted file's articles.
    fn articles(&self, file: &str) -> &[String] {
        &self.posting.files.iter().find(|(name, _)| name == file).unwrap().1
    }

    fn downloaded(&self, home: &Home, file: &str) -> PathBuf {
        home.downloads().join(self.name).join(file)
    }

    fn intact(&self, home: &Home, file: &str) -> bool {
        std::fs::read(self.downloaded(home, file)).is_ok_and(|b| sha256(&b) == sha256(&self.files[file]))
    }
}

fn slow() -> FakeServer {
    let server = FakeServer::start();
    server.set_rate(SLOW);
    server
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn verified(job: &Value) -> u64 {
    job["progress"]["verified"].as_u64().unwrap()
}

fn state(job: &Value) -> &str {
    job["state"].as_str().unwrap()
}

/// Articles holding bytes a download has not verified, at most.
fn unverified_articles(release: &Release, verified: u64) -> u64 {
    let parts: u64 = release.files.values().map(|b| b.len().div_ceil(PART) as u64).sum();
    let whole = verified / PART as u64;
    // Each file can end in a short article, so verified bytes can span a few more.
    parts - whole.saturating_sub(release.files.len() as u64)
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, [u8; 32])> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            if meta.is_dir() {
                pending.push(path);
            } else {
                files.insert(
                    path.clone(),
                    (meta.len(), meta.modified().unwrap(), sha256(&std::fs::read(&path).unwrap())),
                );
            }
        }
    }
    files
}

const SET: [(&str, usize); 3] = [("a.bin", 300_000), ("b.bin", 200_000), ("docs/readme.txt", 70_000)];
const BIG: [(&str, usize); 2] = [("a.bin", 1_200_000), ("b.bin", 900_000)];

#[test]
fn an_nzb_file_downloads_and_every_file_matches_its_post() {
    let server = FakeServer::start();
    let release = Release::new("set", &SET, 1).on(&server);
    let home = Home::served(&server);
    let get = home.get(&release.nzb_file(), &[]);
    assert_eq!(get["reply"]["outcome"], "started", "{get:#}");
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let job = home.job(&release.id());
    assert_eq!((state(&job), verified(&job)), ("done", release.total()));
    for file in release.files.keys() {
        assert!(release.intact(&home, file), "{file} matches its post");
    }
}

/// Serves one NZB over HTTP and counts the requests for it.
fn serve_nzb(nzb: Vec<u8>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/get/release.nzb", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-nzb\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                nzb.len()
            );
            let _ = stream.write_all(reply.as_bytes()).and_then(|()| stream.write_all(&nzb));
        }
    });
    url
}

#[test]
fn an_nzb_url_downloads_and_every_file_matches_its_post() {
    let server = FakeServer::start();
    let release = Release::new("set", &SET, 2).on(&server);
    let home = Home::served(&server);
    let get = home.get(&serve_nzb(release.posting.nzb.clone()), &[]);
    assert_eq!(get["reply"]["outcome"], "started", "{get:#}");
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    for file in release.files.keys() {
        assert!(release.intact(&home, file), "{file} matches its post");
    }
}

#[test]
fn a_files_glob_downloads_only_the_matching_files() {
    let server = FakeServer::start();
    let release = Release::new("set", &SET, 3).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &["--files", "*.bin"]);
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let job = home.job(&release.id());
    assert_eq!((state(&job), job["progress"]["size"].as_u64()), ("done", Some(500_000)));
    assert!(release.intact(&home, "a.bin") && release.intact(&home, "b.bin"));
    assert!(!release.intact(&home, "docs/readme.txt"), "the unselected file was not downloaded");
    let readme: Vec<u32> = release.articles("docs/readme.txt").iter().map(|id| server.served(id)).collect();
    assert_eq!(readme, [1, 0, 0, 0, 0], "only resolving read the unselected file, and only its first article");
    let files = home.ok(&["files", &release.short()]);
    let selected: Vec<(&str, bool)> = files["files"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["path"].as_str().unwrap(), f["selected"].as_bool().unwrap()))
        .collect();
    assert_eq!(selected, [("a.bin", true), ("b.bin", true), ("docs/readme.txt", false)]);
}

#[test]
fn articles_missing_on_the_primary_come_from_the_backup() {
    let (primary, backup) = (FakeServer::start(), FakeServer::start());
    let release = Release::new("set", &SET, 19).on(&primary).on(&backup);
    let gone = &release.articles("b.bin")[2];
    primary.remove(gone);
    let home = Home::served_by(&[(&primary, 0), (&backup, 1)]);
    home.get(&release.nzb_file(), &[]);
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(release.files.keys().all(|f| release.intact(&home, f)));
    assert_eq!((backup.served(gone), backup.bodies()), (1, 1), "the backup sent only what the primary lacked");
}

#[test]
fn a_removed_download_reuses_completed_files_while_the_server_is_down() {
    let server = FakeServer::start();
    let release = Release::new("set", &[("a.bin", 4 * PART), ("b.bin", 4 * PART)], 21).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &["--files", "a.bin"]);
    assert_eq!(home.run(&["wait", &release.short()]).code, 0);
    home.ok(&["rm", &release.short()]);
    home.ok(&["daemon", "stop"]);
    let before = snapshot(&home.downloads());
    let fetched = server.bodies();
    // Resolving the NZB reads the first article of each file, so only that stays up.
    for file in ["a.bin", "b.bin"] {
        for id in &release.articles(file)[1..] {
            server.remove(id);
        }
    }

    home.get(&release.nzb_file(), &["--files", "a.bin"]);
    let wait = home.run(&["wait", &release.short(), "--timeout", "20s"]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(verified(&home.job(&release.id())), release.files["a.bin"].len() as u64);
    assert_eq!(snapshot(&home.downloads()), before, "the files were reused without rewriting them");
    assert_eq!(server.bodies() - fetched, 2, "only resolving read articles");
    assert!(!release.downloaded(&home, "b.bin").exists());
}

#[test]
fn a_removed_partial_download_reuses_pieces_with_a_new_file_selection() {
    let server = slow();
    let release =
        Release::new("set", &[("a.bin", 1_200_000), ("b.bin", 900_000), ("docs/readme.txt", 70_000)], 22).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &["--files", "*.bin"]);
    home.until(&release.id(), "a quarter is verified", |j| verified(j) >= 500_000);
    home.ok(&["pause", &release.short()]);
    let before = verified(&home.job(&release.id()));
    let selected = (release.files["a.bin"].len() + release.files["b.bin"].len()) as u64;
    assert!(before < selected, "paused before completion");
    home.ok(&["rm", &release.short()]);
    home.ok(&["daemon", "stop"]);
    server.set_rate(0);
    let fetched = server.bodies();

    home.get(&release.nzb_file(), &["--files", "*.bin", "--files", "docs/readme.txt"]);
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let again = server.bodies() - fetched;
    let probes = release.files.len() as u64;
    assert!(
        again <= unverified_articles(&release, before) + probes,
        "the new download fetched {again} articles, more than the ones it had not verified plus {probes} to resolve"
    );
    for file in release.files.keys() {
        assert!(release.intact(&home, file), "{file} matches its post");
    }
}

#[test]
fn pause_stops_writes_and_resume_finishes() {
    let server = slow();
    let release = Release::new("big", &BIG, 4).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    home.until(&release.id(), "a quarter is verified", |j| verified(j) >= release.total() / 4);
    let pause = home.ok(&["pause", &release.short()]);
    assert_eq!(pause["changes"][0]["result"], "paused");
    let paused = home.job(&release.id());
    let before = snapshot(&home.downloads());
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(snapshot(&home.downloads()), before, "no file changed after pause returned");
    let still = home.job(&release.id());
    assert_eq!((state(&still), verified(&still)), ("paused", verified(&paused)));
    assert!(verified(&still) < release.total(), "paused mid-download");
    let map = paused["progress"]["piece_map"].as_str().expect("a paused download shows its piece map");
    assert_eq!(still["progress"]["piece_map"], map);
    assert!(map.len() == 64 && map.bytes().all(|b| b.is_ascii_digit()), "every run holds selected pieces: {map}");
    let filled = map.bytes().map(|b| f64::from(b - b'0')).sum::<f64>() / (8.0 * 64.0);
    let share = verified(&paused) as f64 / release.total() as f64;
    assert!((filled - share).abs() < 0.07, "{map} fills {filled:.2}, but {share:.2} is verified");
    assert_eq!(home.ok(&["resume", &release.short()])["changes"][0]["result"], "resumed");
    let wait = home.run(&["wait", &release.short(), "--timeout", "60s"]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(home.job(&release.id())["progress"].get("piece_map"), None, "a done download has no map");
    for file in release.files.keys() {
        assert!(release.intact(&home, file), "{file} matches its post");
    }
}

#[test]
fn a_paused_download_keeps_its_file_progress_across_an_engine_restart() {
    let server = slow();
    let release = Release::new("big", &BIG, 9).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    home.until(&release.id(), "a third is verified", |j| verified(j) >= release.total() / 3);
    assert_eq!(home.ok(&["pause", &release.short()])["changes"][0]["result"], "paused");
    let paused = verified(&home.job(&release.id()));
    let file_bytes = |files: Value| -> Vec<u64> {
        files["files"]["files"].as_array().unwrap().iter().map(|f| f["verified"].as_u64().unwrap()).collect()
    };
    let before = file_bytes(home.ok(&["files", &release.short()]));
    assert_eq!(before.iter().sum::<u64>(), paused, "the files hold the verified bytes: {before:?}");
    home.ok(&["daemon", "stop"]);
    let after = file_bytes(home.ok(&["files", &release.short()]));
    assert_eq!(after, before, "a new engine that has not loaded the paused download reports the same bytes");
    assert_eq!(state(&home.job(&release.id())), "paused");
}

#[test]
fn a_killed_engine_resumes_without_downloading_verified_pieces_again() {
    let server = slow();
    let release = Release::new("big", &BIG, 5).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    let job = home.until(&release.id(), "a third is verified", |j| verified(j) >= release.total() / 3);
    let before_kill = verified(&job);
    std::thread::sleep(Duration::from_secs(1));
    home.kill_engine();
    let fetched = server.bodies();

    // The next engine carries the download on by itself.
    assert_eq!(state(&home.job(&release.id())), "downloading");
    assert_eq!(home.ok(&["resume", &release.short()])["changes"][0]["result"], "already_running");
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    let again = server.bodies() - fetched;
    assert!(
        again <= unverified_articles(&release, before_kill),
        "the restarted engine fetched {again} articles, more than the ones it had not verified"
    );
    for file in release.files.keys() {
        assert!(release.intact(&home, file), "{file} matches its post");
    }
}

#[test]
fn a_killed_engine_trusts_its_last_periodic_save_instead_of_checking_the_files() {
    let server = slow();
    let release = Release::new("big", &BIG, 8).on(&server);
    let home = Home::served(&server).with(hooks::SAVE_RESUME_EVERY, "1s");
    home.get(&release.nzb_file(), &[]);
    home.until(&release.id(), "a third is verified", |j| verified(j) >= release.total() / 3);
    server.set_down(true);
    std::thread::sleep(Duration::from_secs(3));
    let saved = verified(&home.job(&release.id()));
    home.kill_engine();
    for file in release.files.keys() {
        let path = release.downloaded(&home, file);
        if let Ok(meta) = std::fs::metadata(&path) {
            std::fs::write(&path, vec![0; meta.len() as usize]).unwrap();
        }
    }

    let job = home.job(&release.id());
    assert_eq!((state(&job), verified(&job)), ("downloading", saved), "the save outlived the engine");
    let files = home.ok(&["files", &release.short()]);
    let in_engine: u64 =
        files["files"]["files"].as_array().unwrap().iter().map(|f| f["verified"].as_u64().unwrap()).sum();
    assert_eq!(in_engine, saved, "the engine kept the saved pieces, which a check of the zeroed files would reject");
}

#[test]
fn a_download_whose_articles_expired_stops_with_no_source_and_keeps_what_it_verified() {
    let server = FakeServer::start();
    let release = Release::new("big", &BIG, 10).on(&server);
    // Resolving reads the first article, so the rest of b.bin is what expired.
    for id in &release.articles("b.bin")[1..] {
        server.remove(id);
    }
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    let wait = home.run(&["wait", &release.short(), "--timeout", "60s", "--json"]);
    assert_eq!(wait.code, 7, "{}{}", wait.stdout, wait.stderr);
    let result: Value = serde_json::from_str(wait.stdout.lines().last().unwrap()).unwrap();
    assert_eq!(result["outcome"], "error");
    let job = home.job(&release.id());
    assert_eq!((state(&job), job["error"]["code"].as_str()), ("error", Some("no_source")), "{job:#}");
    let a_and_first = (release.files["a.bin"].len() + PART) as u64;
    assert_eq!(verified(&job), a_and_first, "nothing unverified counts, and nothing verified is lost");
    home.ok(&["daemon", "stop"]);
    assert_eq!(verified(&home.job(&release.id())), a_and_first, "what it verified outlived the engine");
}

struct Locked {
    path: PathBuf,
    restore: u32,
}

impl Locked {
    /// `None` as root, which reads and writes past any mode.
    fn new(path: PathBuf, mode: u32) -> Option<Self> {
        if unsafe { libc::geteuid() } == 0 {
            return None;
        }
        let restore = std::fs::metadata(&path).unwrap().permissions().mode();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        Some(Self { path, restore })
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.restore));
    }
}

#[test]
fn after_a_storage_error_resume_downloads_what_never_reached_the_disk() {
    let server = slow();
    let release = Release::new("set", &[("a.bin", 4_000_000), ("b.bin", 300_000)], 14).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &["--files", "a.bin"]);
    home.until(&release.id(), "part of a.bin is verified", |j| verified(j) >= 512 * 1024);
    let Some(locked) = Locked::new(home.downloads().join(release.name), 0o555) else { return };
    home.get(&release.nzb_file(), &["--files", "*.bin"]);
    let wait = home.run(&["wait", &release.short(), "--timeout", "60s"]);
    drop(locked);
    assert_eq!(wait.code, 7, "the engine cannot create b.bin: {}{}", wait.stdout, wait.stderr);
    let job = home.job(&release.id());
    assert_eq!((state(&job), job["error"]["code"].as_str()), ("error", Some("storage_error")), "{job:#}");
    home.ok(&["daemon", "stop"]);
    server.set_rate(0);
    home.ok(&["resume", &release.short()]);
    let wait = home.run(&["wait", &release.short(), "--timeout", "60s"]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(release.intact(&home, "a.bin"), "a.bin matches its post");
    assert!(release.intact(&home, "b.bin"), "b.bin pieces that arrived but failed to write came again");
}

#[test]
fn a_check_that_cannot_read_a_file_fails_and_keeps_what_was_verified() {
    let server = FakeServer::start();
    let release = Release::new("set", &SET, 15).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    assert_eq!(home.run(&["wait", &release.short()]).code, 0);
    let Some(locked) = Locked::new(release.downloaded(&home, "a.bin"), 0o000) else { return };
    assert_eq!(home.ok(&["verify", &release.short()])["changes"][0]["result"], "checking");
    let job = home.until(&release.id(), "the check stops", |j| state(j) != "checking");
    drop(locked);
    assert_eq!(job["error"]["code"], "storage_error", "{job:#}");
    home.ok(&["daemon", "stop"]);
    assert_eq!(verified(&home.job(&release.id())), release.total(), "a check that stopped took nothing back");
    home.ok(&["verify", &release.short()]);
    let job = home.until(&release.id(), "the check passes", |j| state(j) != "checking");
    assert_eq!(state(&job), "done", "{job:#}");
}

#[test]
fn a_download_whose_only_server_goes_down_keeps_waiting() {
    let server = slow();
    let release = Release::new("big", &BIG, 12).on(&server);
    let home = Home::served(&server).with(hooks::STALL_AFTER, "2s");
    home.get(&release.nzb_file(), &[]);
    home.until(&release.id(), "a third is verified", |j| verified(j) >= release.total() / 3);
    server.set_down(true);
    let wait = home.run(&["wait", &release.short(), "--timeout", "8s"]);
    assert_eq!(wait.code, 8, "an unreachable server is not an expired article: {}{}", wait.stdout, wait.stderr);
    assert_eq!(state(&home.job(&release.id())), "downloading");
    server.set_down(false);
    server.set_rate(0);
    let wait = home.run(&["wait", &release.short(), "--timeout", "120s"]);
    assert_eq!(wait.code, 0, "the download finishes once the server is back: {}{}", wait.stdout, wait.stderr);
}

fn damage(release: &Release, home: &Home, file: &str, offset: u64) {
    let mut out = OpenOptions::new().write(true).open(release.downloaded(home, file)).unwrap();
    out.seek(SeekFrom::Start(offset)).unwrap();
    out.write_all(&[!release.files[file][offset as usize]]).unwrap();
}

#[test]
fn verify_reports_a_corrupted_byte_and_repair_fixes_it() {
    let server = FakeServer::start();
    let release = Release::new("set", &SET, 6).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    assert_eq!(home.run(&["wait", &release.short()]).code, 0);
    damage(&release, &home, "a.bin", 3 * PART as u64 + 100);

    assert_eq!(home.ok(&["verify", &release.short()])["changes"][0]["result"], "checking");
    let job = home.until(&release.id(), "the check reports", |j| state(j) != "checking");
    assert_eq!(job["error"]["code"], "verification_failed", "{job:#}");
    assert_eq!(job["error"]["message"], "1 damaged piece in the selected files");
    assert_eq!(verified(&job), release.total() - PART as u64, "the damaged piece no longer counts");
    assert!(!release.intact(&home, "a.bin"), "verify alone repairs nothing");

    let fetched = server.bodies();
    assert_eq!(home.ok(&["verify", &release.short(), "--repair"])["changes"][0]["result"], "checking");
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(release.intact(&home, "a.bin"), "repair downloaded the damaged piece again");
    assert_eq!(server.bodies() - fetched, 1, "only the damaged article came again");
}

#[test]
fn a_failed_check_outlives_the_engine_and_resume_repairs_the_damage() {
    let server = FakeServer::start();
    let release = Release::new("set", &SET, 11).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    assert_eq!(home.run(&["wait", &release.short()]).code, 0);
    damage(&release, &home, "a.bin", 3 * PART as u64 + 100);
    assert_eq!(home.ok(&["verify", &release.short()])["changes"][0]["result"], "checking");
    home.until(&release.id(), "the check reports", |j| state(j) == "error");

    home.ok(&["daemon", "stop"]);
    let job = home.job(&release.id());
    let short = release.total() - PART as u64;
    assert_eq!((state(&job), verified(&job)), ("error", short), "the check outlived the engine: {job:#}");
    let files = home.ok(&["files", &release.short()]);
    let bytes: u64 = files["files"]["files"].as_array().unwrap().iter().map(|f| f["verified"].as_u64().unwrap()).sum();
    assert_eq!(bytes, short, "the files hold what the check verified");
    assert_eq!(home.ok(&["resume", &release.short()])["changes"][0]["result"], "resumed");
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert!(release.intact(&home, "a.bin"), "resume downloaded the damaged piece again");
}

#[test]
fn wait_exits_8_on_a_short_timeout_and_0_when_done() {
    let server = slow();
    let release = Release::new("big", &BIG, 7).on(&server);
    let home = Home::served(&server);
    home.get(&release.nzb_file(), &[]);
    let short = home.run(&["wait", &release.short(), "--timeout", "1s"]);
    assert_eq!(short.code, 8, "{}{}", short.stdout, short.stderr);
    let wait = home.run(&["wait", &release.short()]);
    assert_eq!(wait.code, 0, "{}{}", wait.stdout, wait.stderr);
    assert_eq!(state(&home.job(&release.id())), "done");
}

#[test]
fn a_symbolic_link_inside_the_download_folder_is_refused_and_nothing_lands_where_it_points() {
    let server = FakeServer::start();
    let release = Release::new("set", &SET, 16).on(&server);
    let home = Home::served(&server);
    let outside = home.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(home.downloads()).unwrap();
    let link = home.downloads().join("set");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let dir = home.downloads().display().to_string();
    let get = home.run(&["get", &release.nzb_file(), "--dir", &dir, "--json"]);
    std::thread::sleep(Duration::from_secs(2));
    let landed: Vec<PathBuf> = snapshot(&outside).into_keys().collect();
    assert_eq!((get.code, landed), (4, Vec::new()), "{}{}", get.stdout, get.stderr);
    let error = &serde_json::from_str::<Value>(&get.stdout).unwrap()["error"];
    assert_eq!(error["code"], "path_conflict");
    assert_eq!(
        error["message"],
        format!("{} is a symbolic link, and newsreel writes only inside {dir}", link.display())
    );
    assert_eq!(error["hint"], "remove the link, or choose another folder with --dir");
    assert_eq!(home.ok(&["status"])["jobs"], Value::Array(Vec::new()));
}

#[test]
fn a_download_to_files_a_queued_download_will_write_is_refused() {
    let server = FakeServer::start();
    let pack = [("same.bin", 300_000), ("notes.txt", 2_000)];
    let first = Release::new("pack", &pack, 17).on(&server);
    let second = Release::new("pack", &pack, 18).on(&server);
    // Three downloads from a trickling backup hold every active slot. Their first articles are
    // on the quick server too, so starting each does not wait behind the others' downloads.
    let trickle = FakeServer::start();
    trickle.set_rate(16 * 1024);
    let blockers: Vec<Release> = ["one", "two", "three"]
        .into_iter()
        .zip(30..)
        .map(|(name, seed)| {
            Release::new(name, &[("x.bin", 8 << 20), ("y.txt", 100)], seed).on(&trickle).first_articles_on(&server)
        })
        .collect();
    let home = Home::served_by(&[(&server, 0), (&trickle, 1)]);
    for blocker in &blockers {
        assert_eq!(home.get(&blocker.nzb_file(), &[])["reply"]["outcome"], "started");
    }
    assert_eq!(home.get(&first.nzb_file(), &[])["reply"]["outcome"], "queued");
    let dir = home.downloads().display().to_string();
    let refused = home.run(&["get", &second.nzb_file(), "--dir", &dir, "--json"]);
    for blocker in &blockers {
        home.ok(&["rm", &blocker.short()]);
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let jobs = loop {
        let jobs = home.ok(&["status"])["jobs"].as_array().unwrap().clone();
        if jobs.iter().all(|j| state(j) == "done") || Instant::now() > deadline {
            break jobs;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let finished: Vec<(&str, &str)> = jobs.iter().map(|j| (j["id"].as_str().unwrap(), state(j))).collect();
    assert_eq!(
        (refused.code, finished, first.intact(&home, "same.bin")),
        (4, vec![(first.id().as_str(), "done")], true),
        "{}{}",
        refused.stdout,
        refused.stderr
    );
    let error = &serde_json::from_str::<Value>(&refused.stdout).unwrap()["error"];
    assert_eq!(error["code"], "path_conflict");
    assert_eq!(
        error["message"],
        format!("2 files under {dir}/pack are already files of download {} (pack)", first.short())
    );
    assert_eq!(
        error["hint"],
        format!("choose another folder with --dir, or remove {0} with newsreel rm {0}", first.short())
    );
}

/// The status line and headers, and the body, of one GET with a `Range` header.
fn get_range(url: &str, range: &str) -> (String, Vec<u8>) {
    let (host, path) = url.strip_prefix("http://").unwrap().split_once('/').unwrap();
    let mut stream = std::net::TcpStream::connect(host).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(stream, "GET /{path} HTTP/1.1\r\nHost: {host}\r\nRange: {range}\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8(response[..end].to_vec()).unwrap(), response[end + 4..].to_vec())
}

/// Starts playing a file of an NZB and returns its stream URL.
fn play(home: &Home, nzb: String, file: u32) -> String {
    home.ok(&["status"]);
    let play = PlayRequest { target: Target::NzbFile(nzb.into()), dir: Some(home.downloads()), files: vec![file] };
    let reply = home.ask(&Request::Play(play));
    reply["reply"]["streams"][0]["url"].as_str().unwrap_or_else(|| panic!("{reply:#}")).to_string()
}

#[test]
fn a_player_reads_the_middle_of_a_file_long_before_the_download_finishes() {
    let server = slow();
    let release = Release::new("film", &[("film.mkv", 20 << 20), ("film.srt", 3_000)], 23).on(&server);
    let home = Home::served(&server);
    let url = play(&home, release.nzb_file(), 0);
    assert!(url.ends_with(&format!("/{}/0/film.mkv", release.id())), "{url}");

    let (head, body) = get_range(&url, "bytes=10000000-10099999");
    assert!(head.starts_with("HTTP/1.1 206 Partial Content\r\n"), "{head}");
    assert!(head.contains("\r\nContent-Range: bytes 10000000-10099999/20971520\r\n"), "{head}");
    assert!(body == release.files["film.mkv"][10_000_000..10_100_000], "the 100 KB from the middle match the post");
    let job = home.job(&release.id());
    assert!(verified(&job) < release.total() / 2, "{} of {} bytes verified", verified(&job), release.total());
}

#[test]
fn a_film_stored_in_rar_volumes_streams_from_the_middle_without_extracting() {
    let server = slow();
    let film: Vec<u8> = (0..(6u32 << 20)).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let posting = Posting::stored_rar("Film.2024", "Film.2024.mkv", &film, 1 << 20, PART);
    server.add(&posting);
    let home = Home::served(&server);
    let dir = tempfile::tempdir().unwrap();
    let nzb = dir.path().join("film.nzb");
    std::fs::write(&nzb, &posting.nzb).unwrap();
    let id = hex(&newsreel_usenet::Nzb::parse(&posting.nzb).unwrap().sha1);
    let url = play(&home, nzb.display().to_string(), 1);
    assert!(url.ends_with(&format!("/{id}/1/Film.2024.mkv")), "the film, not the sample, plays: {url}");

    let (head, body) = get_range(&url, "bytes=3000000-3099999");
    assert!(head.starts_with("HTTP/1.1 206 Partial Content\r\n"), "{head}");
    assert!(body == film[3_000_000..3_100_000], "the bytes inside the volumes match the film");
    let job = home.job(&id);
    assert!(verified(&job) < film.len() as u64 / 2, "{} of {} bytes verified", verified(&job), film.len());
    assert!(!snapshot(&home.downloads()).keys().any(|p| p.extension().is_some_and(|e| e == "rar")), "no volume lands");
}
