// Some imports serve only the rendering tests that run on Unix.
#![cfg_attr(windows, allow(unused_imports))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use newsreel_core::error::Details;
use newsreel_core::protocol::{
    Change, ChangeResult, DownloadPlan, FileListSource, FilesReply, GetOutcome, GetReply, Kept, PlayReply, Response,
    Stream, TitlePlayed,
};
use newsreel_core::{
    DownloadId, Error, ErrorCode, FileRisk, Job, JobStatus, Progress, Release, ReleaseFile, ResultId, SearchQuery,
    SearchResult, Selection, SourceOutcome, SourceReport,
};

use super::changes::changes;
use super::doctor::{Check, CheckStatus, Checked, doctor, skill_check};
use super::engine::{DaemonReport, daemon_status};
use super::files::{Files, files, keep_matching};
use super::get::get;
use super::play::{Hold, Playing, hold_line, play, playing_line};
use super::search::search;
use super::skill::{skill_changes, skill_status};
use super::titles::view::*;
use super::wait::{Printer, WaitEnd, wait_row, watched};
use crate::output::{Body, Doc, DocKind, Mode, Typed, render};
use newsreel_core::protocol::JobAction;

const NOW: i64 = 1_792_314_723;
const DAY: i64 = 86_400;
const GIB: u64 = 1024 * 1024 * 1024;
const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

fn tid(hex: &str) -> DownloadId {
    format!("{hex}{}", "0".repeat(40 - hex.len())).parse().unwrap()
}

fn rid(hex: &str) -> ResultId {
    ResultId::from_bytes(*tid(hex).as_bytes())
}

fn both(doc: &Doc, payload: &impl serde::Serialize) -> String {
    let json = render(Mode::Json, doc, payload);
    assert_eq!(json.matches('\n').count(), 1, "a JSON document is one line");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    format!("{}\n--- json\n{}\n", render(Mode::Plain, doc, payload), serde_json::to_string_pretty(&value).unwrap())
}

fn release(id: ResultId, name: &str) -> Release {
    Release {
        id,
        name: name.into(),
        size: None,
        grabs: None,
        published: None,
        category: None,
        flags: BTreeSet::new(),
        sources: vec!["hydra".into()],
        nzb_url: None,
        span: None,
        failed: None,
    }
}

fn mac() -> newsreel_core::Profile {
    use newsreel_core::{Arch, Display, Language, Machine, Os, Profile};
    let display = Some(Display { width: 3024, height: 1964, hdr: true });
    let machine = Machine { os: Os::Macos, display, arch: Some(Arch::Arm64), language: Language::from_locale("en-IN") };
    Profile { detected: Some(machine), room: Some(400 * GIB), ..Profile::default() }
}

fn search_result(releases: Vec<Release>, total: u32, sources: Vec<SourceReport>) -> SearchResult {
    SearchResult {
        query: SearchQuery { profile: mac(), ..SearchQuery::new("arch linux iso") },
        releases,
        total,
        hidden_adult: 0,
        sources,
        elapsed_ms: 1432,
    }
}

fn report(source: &str, outcome: SourceOutcome) -> SourceReport {
    SourceReport { source: source.into(), outcome, elapsed_ms: 300 }
}

#[test]
fn search_with_a_failed_source() {
    let arch = Release {
        size: Some(1_640_497_152),
        grabs: Some(412),
        published: Some(NOW - 17 * DAY),
        sources: vec!["geek".into(), "hydra".into()],
        ..release(rid(ARCH), "archlinux-2026.10.01-x86_64.iso")
    };
    let older = Release {
        size: Some(1_503_238_553),
        grabs: Some(37),
        published: Some(NOW - 47 * DAY),
        ..release(rid("51e0c7"), "archlinux-2026.09.01-x86_64.iso")
    };
    let unknown = Release {
        sources: vec!["planet".into()],
        ..release(ResultId::from_bytes([0xd4; 20]), "Arch Linux 2026.10.01 x86_64 ISO + checksums")
    };
    let hostile = Release {
        grabs: Some(3),
        size: Some(2 * GIB),
        published: Some(NOW - 3 * 365 * DAY),
        ..release(rid("9b5213"), "arch\u{1b}[31m linux\tISO\u{202e}gpj.exe")
    };
    let mut result = search_result(
        vec![arch, older, unknown, hostile],
        14,
        vec![
            report("geek", SourceOutcome::Ok { results: 1 }),
            report("hydra", SourceOutcome::Ok { results: 12 }),
            report("dognzb", SourceOutcome::Http { status: 503 }),
            report("slowznab", SourceOutcome::Timeout),
        ],
    );
    result.hidden_adult = 1;
    let doc = search(&result, NOW);
    insta::assert_snapshot!(both(&doc, &Response::Search { result }));
}

#[test]
fn search_for_a_film_shows_the_quality_its_names_give() {
    let film = |hex: &str, name: &str, size: u64| Release {
        size: Some(size),
        grabs: Some(40),
        published: Some(NOW - 30 * DAY),
        category: Some(newsreel_core::Category::Video),
        ..release(rid(hex), name)
    };
    let mut result = search_result(
        vec![
            film("b42ef9", "Night of the Living Dead (1968)", 1_173_905_628),
            film("5c7c3c", "Night of the Living Dead (1968) 2160p UHD BluRay HDR10 x265", 59 * GIB),
            film("082e09", "Night Of The Living Dead 720p 1968 x264", 1_112_690_010),
        ],
        3,
        vec![report("geek", SourceOutcome::Ok { results: 3 })],
    );
    result.query = SearchQuery { profile: mac(), ..SearchQuery::new("night of the living dead 1968") };
    let doc = search(&result, NOW);
    insta::assert_snapshot!(render(Mode::Plain, &doc, &Response::Search { result }));
}

#[test]
fn film_and_software_searches_say_what_they_ranked_for() {
    use newsreel_core::{Category, Preferences, Profile, Screen};
    let film =
        Release { category: Some(Category::Video), ..release(rid("082e09"), "Night Of The Living Dead 720p 1968") };
    let iso = release(rid(ARCH), "archlinux-2026.10.01-x86_64.iso");
    let book = release(rid("5c7c3c"), "Ubuntu Linux Bible, 11th Edition [PDF]");
    let detected = mac();
    let tv = Preferences { screen: Some(Screen::Tv4k), max_size: Some(8_000_000_000), ..Preferences::default() };
    let summary = |releases: Vec<Release>, category: Option<Category>, profile: &Profile| {
        let mut result = search_result(releases, 1, vec![report("geek", SourceOutcome::Ok { results: 1 })]);
        result.query.category = category;
        result.query.profile = profile.clone();
        search(&result, NOW).summary[1..].to_vec()
    };

    assert_eq!(
        summary(vec![film.clone()], None, &detected),
        ["ranked for this Mac: 1080p screen, HDR, HEVC, arm64, English"]
    );
    assert_eq!(
        summary(vec![film], None, &Profile { preferences: tv, ..detected.clone() }),
        ["ranked for this Mac: 2160p screen, up to 8 GB, arm64, English · overrides: screen tv-4k, max_size 8GB"]
    );
    assert_eq!(
        summary(Vec::new(), Some(Category::Video), &Profile::default()),
        ["filtered by --category video", "ranked for this machine, not detected yet"]
    );
    assert_eq!(summary(vec![iso], None, &detected), ["ranked for this Mac: arm64"]);
    assert_eq!(summary(vec![book], None, &detected), Vec::<String>::new());
}

#[test]
fn search_next_opens_the_top_result() {
    let top = release(rid(ARCH), "archlinux-2026.10.01-x86_64.iso");
    let other = release(rid("51e0c7"), "archlinux-2026.09.01-x86_64.iso");
    let result = search_result(vec![top, other], 2, vec![report("hydra", SourceOutcome::Ok { results: 2 })]);
    assert_eq!(search(&result, NOW).next.as_deref(), Some("newsreel files u9b5212"));
}

#[test]
fn a_search_with_no_results_drops_its_filters_or_skips_the_cache() {
    let cached = vec![report("hydra", SourceOutcome::Cached { results: 0 })];
    let mut filtered = search_result(Vec::new(), 0, cached.clone());
    filtered.query.sources = vec!["hydra".into()];
    filtered.query.min_grabs = Some(50);
    let doc = search(&filtered, NOW);
    assert_eq!(doc.summary, ["no results · 1 of 1 source ok · 1.4s", "filtered by --source hydra --min-grabs 50"]);
    assert_eq!(doc.next.as_deref(), Some("newsreel search arch linux iso"));

    let doc = search(&search_result(Vec::new(), 0, cached), NOW);
    assert_eq!(doc.summary, ["no results · 1 of 1 source ok · 1.4s"]);
    assert_eq!(doc.next.as_deref(), Some("newsreel search arch linux iso --fresh"));
}

fn file(index: u32, path: &str, size: u64) -> ReleaseFile {
    ReleaseFile { index, path: path.into(), size, selected: false, verified: 0, risk: None }
}

fn film_files() -> FilesReply {
    let mut installer = file(3, "Codec Installer.exe", 1_200_000);
    installer.risk = Some(FileRisk::Executable);
    FilesReply {
        id: "6bc1af".to_string() + &"0".repeat(34),
        name: "night_of_the_living_dead_dvd".into(),
        total_size: 5_046_586_573,
        files: vec![
            file(0, "Night.asr.srt", 52_633),
            file(1, "Night.mp4", 596_399_542),
            file(2, "VTS_01_1.mp4", 262_668_288),
            installer,
        ],
        source: FileListSource::NzbUrl,
        elapsed_ms: 412,
    }
}

#[test]
fn files_from_an_nzb_url_mark_executables() {
    let reply = film_files();
    insta::assert_snapshot!(both(&files(&reply, None), &Response::Files { files: reply }));
}

#[test]
fn files_matching_globs_list_only_those() {
    let mut reply = film_files();
    let patterns = vec!["*.MP4".to_string(), "Night.asr.srt".to_string()];
    let matched = keep_matching(&mut reply, &patterns).unwrap();
    let doc = files(&reply, Some(&matched));
    insta::assert_snapshot!(both(
        &doc,
        &Typed { kind: DocKind::Files, body: Files { files: reply, matched: Some(matched) } }
    ));
}

#[test]
fn a_match_summary_counts_the_matches_and_their_size() {
    let mut reply = film_files();
    let matched = keep_matching(&mut reply, &["night.mp4".to_string()]).unwrap();
    let doc = files(&reply, Some(&matched));
    assert_eq!(
        doc.summary,
        ["1 of 4 files in night_of_the_living_dead_dvd match · 569 MiB · file list from the NZB URL in 0.4s"]
    );
    assert_eq!(doc.next.as_deref(), Some("newsreel get 6bc1af --files night.mp4"));
    let mut reply = film_files();
    let matched = keep_matching(&mut reply, &["Codec Installer.exe".to_string()]).unwrap();
    assert_eq!(
        files(&reply, Some(&matched)).next.as_deref(),
        Some("newsreel get 6bc1af --files 'Codec Installer.exe'")
    );
}

#[test]
fn files_of_a_download_show_progress_per_file() {
    let selected = |f: ReleaseFile, verified| ReleaseFile { selected: true, verified, ..f };
    let reply = FilesReply {
        id: ARCH.into(),
        name: "debian-13.1.0-amd64-DVD".into(),
        total_size: 12 * GIB,
        files: vec![
            selected(file(0, "debian-13.1.0-amd64-DVD-1.iso", 4 * GIB), 4 * GIB),
            selected(file(1, "debian-13.1.0-amd64-DVD-2.iso", 4 * GIB), GIB),
            file(2, "debian-13.1.0-amd64-DVD-3.iso", 4 * GIB),
        ],
        source: FileListSource::Download,
        elapsed_ms: 3,
    };
    insta::assert_snapshot!(both(&files(&reply, None), &Response::Files { files: reply }));
}

#[test]
fn a_one_file_release_counts_one_file() {
    let reply = FilesReply {
        id: ARCH.into(),
        name: "archlinux-2026.10.01-x86_64.iso".into(),
        total_size: GIB,
        files: vec![file(0, "archlinux-2026.10.01-x86_64.iso", GIB)],
        source: FileListSource::Cache,
        elapsed_ms: 900,
    };
    assert_eq!(
        files(&reply, None).summary,
        ["1 file in archlinux-2026.10.01-x86_64.iso · 1.0 GiB · file list from cache in 0.9s"]
    );
}

fn job(hex: &str, name: &str, status: JobStatus, progress: Progress) -> Job {
    let id = tid(hex);
    let dir = PathBuf::from("/Users/ada/Downloads/newsreel");
    Job {
        id,
        name: name.into(),
        path: dir.join(name),
        dir,
        status,
        progress,
        selection: Selection::All,
        added_at: NOW - 3600,
        title: None,
        place: newsreel_core::Place::Folder,
    }
}

fn downloading() -> Progress {
    Progress {
        size: 1_640_497_152,
        verified: 50_855_411,
        piece_map: None,
        down_rate: 32_715_571,
        connections: 48,
        eta: Some(49),
    }
}

fn get_reply(outcome: GetOutcome, job: Job, plan: Option<DownloadPlan>) -> GetReply {
    GetReply { outcome, job, plan, previous_selection: None }
}

fn plan(conflicts: Vec<PathBuf>) -> DownloadPlan {
    DownloadPlan {
        selected_files: 2,
        total_files: 255,
        selected_size: 596_452_175,
        destination: "/Users/ada/Movies/night_of_the_living_dead_dvd".into(),
        free_space: 410 * GIB,
        conflicts,
        max_size: Some(50 * GIB),
    }
}

// Its expected output holds Unix paths, which Windows joins with a backslash.
#[cfg(unix)]
#[test]
fn a_dry_run_shows_the_size_cap_beside_the_size_and_free_space() {
    let film = job("6bc1af", "night_of_the_living_dead_dvd", JobStatus::Checking, Progress::default());
    let reply = get_reply(GetOutcome::Planned, film, Some(DownloadPlan { max_size: Some(8 * GIB), ..plan(vec![]) }));
    let fields: Vec<String> = match get(&reply, None).body {
        Body::Fields { fields, .. } => fields.into_iter().map(|(k, v)| format!("{k}: {}", v.text)).collect(),
        Body::Table { .. } => unreachable!("get shows fields"),
    };
    assert_eq!(
        fields,
        [
            "files: all (2 of 255, 569 MiB)",
            "path: /Users/ada/Downloads/newsreel/night_of_the_living_dead_dvd",
            "free: 410 GiB",
            "cap: 8 GiB",
            "conflicts: none"
        ]
    );
}

// Its expected output holds Unix paths, which Windows joins with a backslash.
#[cfg(unix)]
#[test]
fn get_outcomes() {
    let movies = |mut j: Job| {
        j.dir = "/Users/ada/Movies".into();
        j.path = j.dir.join(&j.name);
        j.selection = Selection::Patterns(vec!["Night.mp4".into(), "*.srt".into()]);
        j
    };
    let film = || movies(job("6bc1af", "night_of_the_living_dead_dvd", JobStatus::Checking, Progress::default()));
    let cases = [
        (
            "started from an NZB",
            get_reply(
                GetOutcome::Started,
                job(ARCH, "archlinux-2026.10.01-x86_64.iso", JobStatus::Checking, Progress::default()),
                None,
            ),
        ),
        (
            "queued",
            get_reply(
                GetOutcome::Queued,
                movies(job("6bc1af", "night_of_the_living_dead_dvd", JobStatus::Queued, Progress::default())),
                Some(plan(vec![])),
            ),
        ),
        (
            "existing",
            get_reply(
                GetOutcome::Existing,
                job(ARCH, "archlinux-2026.10.01-x86_64.iso", JobStatus::Downloading, downloading()),
                None,
            ),
        ),
        (
            "existing and done",
            get_reply(
                GetOutcome::Existing,
                job(ARCH, "archlinux-2026.10.01-x86_64.iso", JobStatus::Done { finished_at: NOW }, downloading()),
                None,
            ),
        ),
        (
            "updated",
            GetReply {
                previous_selection: Some(Selection::All),
                ..get_reply(GetOutcome::Updated, film(), Some(plan(vec![])))
            },
        ),
        ("planned in the default folder", get_reply(GetOutcome::Planned, film(), Some(plan(vec![])))),
        (
            "planned with conflicts",
            get_reply(
                GetOutcome::Planned,
                film(),
                Some(plan(vec!["/Users/ada/Movies/night_of_the_living_dead_dvd/Night.mp4".into()])),
            ),
        ),
    ];
    let mut out = String::new();
    for (name, reply) in cases {
        out.push_str(&format!(
            "=== {name}\n{}",
            both(&get(&reply, None), &Response::Get { reply: Box::new(reply.clone()) })
        ));
    }
    let reply = get_reply(GetOutcome::Planned, film(), Some(plan(vec![])));
    let doc = get(&reply, Some(Path::new("/Users/ada/Movies")));
    out.push_str(&format!("=== planned with --dir\n{}", both(&doc, &Response::Get { reply: Box::new(reply) })));
    insta::assert_snapshot!(out);
}

fn fleet() -> Vec<Job> {
    let full = |size| Progress { size, verified: size, ..Progress::default() };
    vec![
        job(ARCH, "archlinux-2026.10.01-x86_64.iso", JobStatus::Downloading, downloading()),
        job(
            "c32cdd",
            "stackexchange",
            JobStatus::Checking,
            Progress { size: 99_535_000_000, verified: 79_130_000_000, ..Progress::default() },
        ),
        job("6bc1af", "night_of_the_living_dead_dvd", JobStatus::Done { finished_at: NOW - 600 }, full(596_452_175)),
        job(
            "e3a1f0",
            "ubuntu-24.04.5-desktop-amd64.iso",
            JobStatus::Error {
                error: newsreel_daemon::daemon::admission::check_disk(
                    newsreel_daemon::disk::Space { device: 1, total: 250 * GIB, free: 3 * GIB },
                    6_345_887_744,
                    0,
                )
                .unwrap_err(),
            },
            Progress { size: 6_345_887_744, ..Progress::default() },
        ),
        job("d40a19", "Sintel.2010.1080p.mkv", JobStatus::Checking, Progress { connections: 4, ..Progress::default() }),
        job(
            "51e0c7",
            "archlinux-2026.09.01-x86_64.iso",
            JobStatus::Queued,
            Progress { size: 1_503_238_553, ..Progress::default() },
        ),
        job(
            "a282c6",
            "Night Of The Living Dead",
            JobStatus::Paused,
            Progress { size: 2_899_102_924, verified: 1_000_000_000, ..Progress::default() },
        ),
    ]
}

// Its expected output holds Unix paths, which Windows joins with a backslash.
#[cfg(unix)]
#[test]
fn status_lists_every_state() {
    let jobs = fleet();
    insta::assert_snapshot!(both(&super::status::jobs(&jobs), &Response::Jobs { jobs }));
}

#[test]
fn status_offers_resume_for_failed_downloads() {
    let next = |jobs: &[Job]| {
        let doc = super::status::jobs(jobs);
        (doc.summary[1..].to_vec(), doc.next)
    };
    let fleet = fleet();
    let (summary, wait) = next(&fleet);
    assert_eq!(wait.as_deref(), Some("newsreel wait 9b5212 c32cdd d40a19 51e0c7"));
    assert_eq!(summary.last().unwrap(), "to retry failed downloads: newsreel resume e3a1f0");

    let stopped: Vec<Job> = fleet.iter().filter(|j| j.status.is_stopped()).cloned().collect();
    assert_eq!(next(&stopped).1.as_deref(), Some("newsreel resume e3a1f0"));

    let paused: Vec<Job> = fleet.iter().filter(|j| j.status == JobStatus::Paused).cloned().collect();
    assert_eq!(next(&paused), (Vec::new(), Some("newsreel resume a282c6".to_string())));
}

fn change(hex: &str, name: &str, result: ChangeResult, path: Option<&str>) -> Change {
    Change { path: path.map(PathBuf::from), ..Change::new(tid(hex).to_hex(), Some(name.into()), result) }
}

#[test]
fn pause_resume_rm_and_verify() {
    let cases = [
        (
            JobAction::Pause,
            vec![
                change(ARCH, "archlinux-2026.10.01-x86_64.iso", ChangeResult::Paused, None),
                change("c32cdd", "stackexchange", ChangeResult::AlreadyPaused, None),
            ],
        ),
        (
            JobAction::Resume,
            vec![
                change("c32cdd", "stackexchange", ChangeResult::Resumed, None),
                change("6bc1af", "night_of_the_living_dead_dvd", ChangeResult::AlreadyDone, None),
            ],
        ),
        (
            JobAction::Remove,
            vec![
                Change {
                    trashed: vec![PathBuf::from("/Users/ada/isos/archlinux-2026.10.01-x86_64.iso")],
                    kept: vec![Kept {
                        path: "/Users/ada/isos/archlinux-2026.10.01-x86_64.iso.sig".into(),
                        reason: "another download uses it".into(),
                    }],
                    ..change(
                        ARCH,
                        "archlinux-2026.10.01-x86_64.iso",
                        ChangeResult::Removed,
                        Some("/Users/ada/isos/archlinux-2026.10.01-x86_64.iso"),
                    )
                },
                change(
                    "c32cdd",
                    "stackexchange",
                    ChangeResult::AlreadyRemoved,
                    Some("/Users/ada/datasets/stackexchange"),
                ),
                Change {
                    deleted: vec![PathBuf::from("/Users/ada/.local/share/newsreel/plays/6bc1af0000000000-0")],
                    ..change(
                        "6bc1af",
                        "night_of_the_living_dead_dvd",
                        ChangeResult::Removed,
                        Some("/Users/ada/.local/share/newsreel/plays/6bc1af0000000000-0/night_of_the_living_dead_dvd"),
                    )
                },
                change(
                    "d4d4d4",
                    "pioneer_one",
                    ChangeResult::Removed,
                    Some("/Users/ada/Downloads/newsreel/pioneer_one"),
                ),
            ],
        ),
        (JobAction::Verify, vec![change("6bc1af", "night_of_the_living_dead_dvd", ChangeResult::Checking, None)]),
    ];
    let mut out = String::new();
    for (command, list) in cases {
        out.push_str(&format!(
            "=== {command:?}\n{}",
            both(&changes(command, &list), &Response::Changed { changes: list.clone() })
        ));
    }
    insta::assert_snapshot!(out);
}

fn wait_transcript(mode: Mode, end: WaitEnd, frames: &[(u64, Job)], elapsed: u64) -> String {
    let printer = Printer { mode };
    let jobs: Vec<Job> = {
        let mut latest: Vec<Job> = Vec::new();
        for (_, job) in frames {
            match latest.iter_mut().find(|j| j.id == job.id) {
                Some(j) => *j = job.clone(),
                None => latest.push(job.clone()),
            }
        }
        latest
    };
    let latest = watched(jobs);
    let mut out = printer.header();
    for (at, job) in frames {
        let id = &latest.iter().find(|w| w.job.id == job.id).unwrap().id;
        out.push_str(&printer.row(Duration::from_secs(*at), id, job));
    }
    out.push_str(&printer.end(&end, Duration::from_secs(elapsed), &latest));
    out
}

// Its expected output holds Unix paths, which Windows joins with a backslash.
#[cfg(unix)]
#[test]
fn wait_until_done_timeout_error_and_stop() {
    let arch = |status, progress| job(ARCH, "archlinux-2026.10.01-x86_64.iso", status, progress);
    let done = Progress { size: 1_640_497_152, verified: 1_640_497_152, ..Progress::default() };
    let later = Progress { verified: 1_240_000_000, eta: Some(12), ..downloading() };
    let failed = JobStatus::Error {
        error: Error::new(
            ErrorCode::DiskFull,
            "needs 1.5 GiB more on this disk, but only 900.0 MiB is free and 2.0 GiB is kept in reserve",
        )
        .with_hint("free some space, or choose another disk with --dir"),
    };
    let cases = [
        (
            "done",
            WaitEnd::Done,
            vec![
                (0, arch(JobStatus::Downloading, downloading())),
                (44, arch(JobStatus::Done { finished_at: NOW }, done)),
            ],
            44,
        ),
        (
            "timeout",
            WaitEnd::Timeout,
            vec![(0, arch(JobStatus::Downloading, downloading())), (60, arch(JobStatus::Downloading, later))],
            100,
        ),
        (
            "error",
            WaitEnd::Failed { id: "9b5212".into() },
            vec![(0, arch(JobStatus::Checking, Progress::default())), (3, arch(failed, Progress::default()))],
            3,
        ),
        ("stopped", WaitEnd::Stopped { id: "9b5212".into() }, vec![(0, arch(JobStatus::Paused, later))], 0),
    ];
    let mut out = String::new();
    for (name, end, frames, elapsed) in cases {
        let exit = end.exit_code();
        out.push_str(&format!(
            "=== {name}, exit {exit}\n{}",
            wait_transcript(Mode::Plain, end.clone(), &frames, elapsed)
        ));
        out.push_str(&format!("--- ndjson\n{}", wait_transcript(Mode::Json, end, &frames, elapsed)));
    }
    insta::assert_snapshot!(out);
}

#[test]
fn daemon_status_running_and_stopped() {
    let running = DaemonReport {
        engine: "running",
        pid: Some(4242),
        version: Some("0.1.0".into()),
        address: Some("http://127.0.0.1:47919".into()),
        log: "/Users/ada/.local/state/newsreel/engine.log".into(),
        downloads: Some(2),
    };
    let stopped =
        DaemonReport { engine: "stopped", pid: None, version: None, address: None, downloads: None, ..running.clone() };
    let mut out = String::new();
    for report in [running, stopped] {
        out.push_str(&both(&daemon_status(&report), &crate::output::Typed { kind: DocKind::Daemon, body: &report }));
    }
    insta::assert_snapshot!(out);
}

#[test]
fn doctor_report() {
    let checks = vec![
        Check { check: Checked::Sandbox, status: CheckStatus::Fail, detail: "CODEX_SANDBOX_NETWORK_DISABLED=1".into() },
        Check {
            check: Checked::Disk,
            status: CheckStatus::Ok,
            detail: "410.2 GiB free in /Users/ada/Downloads/newsreel".into(),
        },
        Check { check: Checked::State, status: CheckStatus::Ok, detail: "/Users/ada/.local/state/newsreel".into() },
        Check {
            check: Checked::Engine,
            status: CheckStatus::Ok,
            detail: "not running; the next command starts it".into(),
        },
        Check { check: Checked::Sources, status: CheckStatus::Fail, detail: "1 of 2 responded".into() },
        Check { check: Checked::Source("geek".into()), status: CheckStatus::Ok, detail: "12 results, 380 ms".into() },
        Check { check: Checked::Source("dognzb".into()), status: CheckStatus::Fail, detail: "HTTP 503, 211 ms".into() },
    ];
    let fix = Some(
        "in interactive Codex, rerun with sandbox_permissions \"require_escalated\" and prefix_rule [\"newsreel\"]"
            .into(),
    );
    insta::assert_snapshot!(both(
        &doctor(&checks, fix),
        &crate::output::Typed { kind: DocKind::Doctor, body: serde_json::json!({"checks": checks}) }
    ));
}

#[test]
fn one_failed_source_is_one_problem() {
    let check = |check: Checked, status| Check { check, status, detail: String::new() };
    let probed = [
        check(Checked::Sources, CheckStatus::Fail),
        check(Checked::Source("geek".into()), CheckStatus::Ok),
        check(Checked::Source("dognzb".into()), CheckStatus::Fail),
    ];
    assert_eq!(doctor(&probed, None).summary, ["1 problem"]);
    let unprobed = [check(Checked::Config, CheckStatus::Fail), check(Checked::Sources, CheckStatus::Skipped)];
    assert_eq!(doctor(&unprobed, None).summary, ["1 problem"]);
    let failed = [check(Checked::Sources, CheckStatus::Fail)];
    assert_eq!(doctor(&failed, None).summary, ["1 problem"]);
}

#[test]
fn schema_document() {
    use clap::CommandFactory;
    let schema = crate::schema::build(&crate::cli::Cli::command(), "0.1.0".into());
    let value: serde_json::Value = serde_json::from_str(&crate::output::json_document(&schema, None)).unwrap();
    insta::assert_snapshot!(serde_json::to_string_pretty(&value).unwrap());
}

fn sample(code: ErrorCode) -> Error {
    use newsreel_daemon::daemon::admission::{self, Plan};
    use newsreel_daemon::daemon::fake;
    use newsreel_daemon::disk::Space;
    let film = || {
        fake::Posting {
            id: DownloadId::from_bytes([0x6b; 20]),
            name: "night".into(),
            files: vec![("Night.mp4".into(), 70 * GIB)],
        }
        .layout()
        .unwrap()
    };
    let disk = Space { device: 1, total: 100 * GIB, free: 3 * GIB };
    let plan = |conflicts| Plan {
        wanted: vec![true],
        selected_size: 70 * GIB,
        allocated_size: 0,
        destination: "/Users/ada/Movies/night".into(),
        links: Vec::new(),
        conflicts,
        space: disk,
    };
    let ubuntu = |hex: &str| Release { name: "ubuntu-24.04.5-desktop-amd64.iso".into(), ..release(rid(hex), "") };
    let given = |code, message: &str, hint: &str| Error::new(code, message).with_hint(hint);
    match code {
        ErrorCode::Usage => crate::usage_error(
            &<crate::cli::Cli as clap::Parser>::try_parse_from(["newsreel", "wait", "9b5212", "--timeout", "soon"])
                .unwrap_err(),
            &["newsreel".into(), "wait".into()],
        ),
        ErrorCode::ConfirmationRequired => given(code, "deleting files needs confirmation", "add --yes to confirm"),
        ErrorCode::InvalidRef => newsreel_core::Target::parse("9b5", Path::new("/")).unwrap_err(),
        ErrorCode::InvalidRelease => fake::parse(b"<html>").unwrap_err(),
        ErrorCode::NotFound => {
            let prefix = "9b52".parse().unwrap();
            newsreel_daemon::daemon::resolve::prefix(
                &prefix,
                &[],
                &[],
                newsreel_daemon::daemon::resolve::Scope::Downloads,
                "pause",
            )
            .unwrap_err()
        }
        ErrorCode::AmbiguousRef => {
            let results =
                [ubuntu("e3a1f0"), Release { name: "Ubuntu 24.04.5 LTS Desktop (repack)".into(), ..ubuntu("e3a1c4") }];
            let prefix = "ue3a1".parse().unwrap();
            newsreel_daemon::daemon::resolve::prefix(
                &prefix,
                &[],
                &results,
                newsreel_daemon::daemon::resolve::Scope::DownloadsAndResults,
                "get",
            )
            .unwrap_err()
        }
        ErrorCode::JobConflict => {
            given(code, "d40a19 is busy saving its state", "try newsreel verify d40a19 again in a few seconds")
        }
        ErrorCode::PathConflict => {
            admission::check_conflicts(&plan(vec!["/Users/ada/Movies/night/Night.mp4".into()])).unwrap_err()
        }
        ErrorCode::ProvidersUnavailable => given(
            code,
            "all sources failed (hydra, dognzb)",
            "check the network connection, or retry with a longer --timeout",
        )
        .with_details(Details::Sources(vec![
            report("hydra", SourceOutcome::Http { status: 502 }),
            report("dognzb", SourceOutcome::Timeout),
        ])),
        ErrorCode::NetworkUnavailable => {
            given(code, "no route to news.example.com:563", "check the network connection, then run the command again")
        }
        ErrorCode::FetchFailed => given(
            code,
            "could not fetch the NZB: HTTP 404",
            "check the network connection and the indexer, or search again",
        ),
        ErrorCode::SandboxNetworkBlocked => crate::client::launch::sandbox_error(
            crate::client::launch::Blocked::Network { signal: "CODEX_SANDBOX_NETWORK_DISABLED=1".into() },
            &newsreel_daemon::env::Env::from_pairs([("CODEX_THREAD_ID", "t-1")]),
        ),
        ErrorCode::SandboxWriteBlocked => given(
            code,
            "cannot write /Users/ada/Library/LaunchAgents/dev.newsreel.engine.plist: operation not permitted",
            "run newsreel engine install from a terminal outside any sandbox",
        ),
        ErrorCode::DaemonUnavailable => crate::client::connect::lost(
            std::io::ErrorKind::TimedOut.into(),
            "/Users/ada/.local/state/newsreel/engine.log",
        ),
        ErrorCode::DaemonVersion => given(
            code,
            "the engine uses protocol version 2, but this client uses 1",
            "run newsreel engine stop, then run the command again",
        )
        .with_details(Details::Versions { client: "0.1.0".into(), daemon: "0.2.0".into() })
        .with_next("newsreel engine stop"),
        ErrorCode::StateCorrupt => given(
            code,
            "the state database /Users/ada/.local/state/newsreel/state.db is damaged: file is not a database",
            "the state is never reset automatically; move /Users/ada/.local/state/newsreel/state.db aside to start fresh, or restore a backup",
        ),
        ErrorCode::DiskFull => admission::check_disk(disk, 70 * GIB, 0).unwrap_err(),
        ErrorCode::TooLarge => {
            admission::check_size(plan(vec![]).selected_size, admission::Cap::new(None, None)).unwrap_err()
        }
        ErrorCode::NoSelection => {
            newsreel_daemon::daemon::files::wanted(&film(), &Selection::Patterns(vec!["*.mkv".into()])).unwrap_err()
        }
        ErrorCode::StorageError => given(
            code,
            "the disk containing /Users/ada/.local/state/newsreel/state.db is full",
            "free some space on that disk",
        ),
        ErrorCode::VerificationFailed => given(
            code,
            "3 damaged pieces in the selected files",
            "run newsreel verify 6bc1af --repair to download them again",
        )
        .with_next("newsreel verify 6bc1af --repair"),
        ErrorCode::NoSource => given(
            code,
            "no server has sent the remaining 2 pieces (488.8 KiB) for 20m",
            "run newsreel resume 770e77 to try again; the 33.0 MiB already verified is kept",
        )
        .with_next("newsreel resume 770e77"),
        ErrorCode::Interrupted => given(code, "interrupted", "run the command again"),
        ErrorCode::NotPaired => newsreel_daemon::daemon::devices::Refused::WrongCode.error(),
        ErrorCode::LocalOnly => {
            given(code, "only the Mac running newsreel can add or remove devices", "open newsreel on the Mac to do it")
        }
        ErrorCode::Internal => Error::internal("the engine thread stopped"),
    }
}

#[test]
fn every_error_class() {
    let mut out = String::new();
    for code in ErrorCode::ALL {
        let error = sample(code);
        assert_eq!(error.code, code);
        let exit = newsreel_daemon::exit::code(code);
        assert_ne!(exit, newsreel_daemon::exit::PENDING);
        let plain = crate::output::error(Mode::Plain, &error);
        let json = crate::output::error(Mode::Json, &error);
        assert!(plain.stdout.is_empty() && json.stderr.is_empty(), "plain errors use stderr, JSON errors stdout");
        let value: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
        out.push_str(&format!(
            "=== {code}, exit {exit}\n{}--- json\n{}\n",
            plain.stderr,
            serde_json::to_string_pretty(&value).unwrap()
        ));
    }
    insta::assert_snapshot!(out);
}

#[test]
fn skill_status_suggests_install_only_when_it_loses_no_edits() {
    use crate::skill::{Installed, State, Target};
    let row = |target: Target, state, version: Option<&str>| Installed {
        target,
        path: PathBuf::from(format!("/Users/ada/.{}/skills/newsreel", target.name())),
        state,
        version: version.map(String::from),
    };
    let stale = [row(Target::Agents, State::Outdated, Some("0.0.9")), row(Target::Claude, State::Missing, None)];
    assert_eq!(
        skill_status(&stale, "0.1.0", "newsreel skill install").plain(),
        "TARGET\tSTATE\tVERSION\tPATH\n\
         agents\toutdated\t0.0.9\t/Users/ada/.agents/skills/newsreel\n\
         claude\tmissing\t-\t/Users/ada/.claude/skills/newsreel\n\
         # this binary includes the skill from newsreel 0.1.0\n\
         next: newsreel skill install\n"
    );
    let edited = [row(Target::Agents, State::Modified, Some("0.1.0")), row(Target::Claude, State::Foreign, None)];
    assert_eq!(
        skill_status(&edited, "0.1.0", "newsreel skill install").plain(),
        "TARGET\tSTATE\tVERSION\tPATH\n\
         agents\tmodified\t0.1.0\t/Users/ada/.agents/skills/newsreel\n\
         claude\tforeign\t-\t/Users/ada/.claude/skills/newsreel\n\
         # this binary includes the skill from newsreel 0.1.0\n\
         # agents was edited after newsreel wrote it\n\
         # claude was not written by newsreel\n\
         # to replace them: newsreel skill install --force\n"
    );
    let check = |status, detail: &str| Check { check: Checked::Skill, status, detail: detail.into() };
    assert_eq!(
        skill_check(&stale),
        (check(CheckStatus::Warn, "agents outdated, claude missing"), Some("newsreel skill install".into()))
    );
    assert_eq!(skill_check(&edited), (check(CheckStatus::Warn, "agents modified, claude foreign"), None));
}

// Its expected output holds Unix paths, which Windows joins with a backslash.
#[cfg(unix)]
#[test]
fn a_tab_in_a_path_stays_inside_its_column() {
    use crate::skill::{self, AgentArg, Change as SkillChange, Installed, Scope, State, Target, TargetArgs};
    let project = Path::new("/src/my\trepo");
    let path = project.join(".agents/skills/newsreel");
    let install =
        skill::command("install", &TargetArgs { agent: AgentArg::All, scope: Scope::Project }, false, project);
    let installed = [Installed { target: Target::Agents, path: path.clone(), state: State::Missing, version: None }];
    let changes = [SkillChange { target: Target::Agents, path, result: "installed" }];
    let report = DaemonReport {
        engine: "stopped",
        pid: None,
        version: None,
        address: None,
        log: "/state/my\nnewsreel/engine.log".into(),
        downloads: None,
    };
    let named = job("9b5212", "arch\tlinux.iso", JobStatus::Paused, Progress::default());

    assert_eq!(
        skill_status(&installed, "0.1.0", &install).plain(),
        "TARGET\tSTATE\tVERSION\tPATH\n\
         agents\tmissing\t-\t/src/my repo/.agents/skills/newsreel\n\
         # this binary includes the skill from newsreel 0.1.0\n\
         next: cd '/src/my repo' && newsreel skill install --scope project\n"
    );
    assert_eq!(
        skill_changes(&changes).plain(),
        "TARGET\tRESULT\tPATH\nagents\tinstalled\t/src/my repo/.agents/skills/newsreel\n"
    );
    assert_eq!(
        daemon_status(&report).plain(),
        "engine: stopped\nlog:    /state/my newsreel/engine.log\nnext:   newsreel status\n"
    );
    let row: Vec<String> = wait_row(Duration::from_secs(3), "9b5212", &named).into_iter().map(|c| c.text).collect();
    assert_eq!(row.join("\t"), "3s\t9b5212\tpaused\t100%\t0 B\t-\t-\t-\tarch linux.iso");
}

#[test]
fn play_shows_the_files_it_plays_and_the_runtime_once_known() {
    let stream = |index, path: &str, size| Stream {
        index,
        path: path.into(),
        size,
        url: format!("http://127.0.0.1:47919/6bc1af{}/{index}/{path}", "0".repeat(34)),
    };
    let film = job("6bc1af", "night_of_the_living_dead_dvd", JobStatus::Downloading, downloading());
    let reply = PlayReply {
        job: film.clone(),
        streams: vec![stream(1, "Night.mp4", 596_399_542), stream(0, "Night.asr.srt", 52_633)],
    };
    let bare = PlayReply { job: film.clone(), streams: vec![stream(2, "VTS_01_1.mp4", 262_668_288)] };
    let named = |name: &str, reply: &PlayReply| Playing { name: name.into(), reply: reply.clone(), title: None };
    let title = TitlePlayed {
        video: "tt0063350".parse().unwrap(),
        name: "Night of the Living Dead (1968)".into(),
        release: release(rid("6bc1af"), "Night of the Living Dead (1968) [DVD]"),
        tried: Vec::new(),
        resume_at: None,
    };
    let titled = Playing { title: Some(title), ..named("Night of the Living Dead (1968)", &bare) };
    let out = [
        play(&named("Night of the Living Dead (1968)", &reply), Some(Duration::from_secs(5772))).plain(),
        play(&named("night_of_the_living_dead_dvd", &bare), None).plain(),
        play(&titled, None).plain(),
    ];
    assert_eq!(
        out,
        [
            "6bc1af Night of the Living Dead (1968)\nvideo:     Night.mp4 (569 MiB)\nsubtitles: Night.asr.srt\n\
             runtime:   1:36:12\nnext:      newsreel status 6bc1af\n",
            "6bc1af night_of_the_living_dead_dvd\nvideo: VTS_01_1.mp4 (250 MiB)\nnext:  newsreel status 6bc1af\n",
            "6bc1af Night of the Living Dead (1968)\nsource: Night of the Living Dead (1968) [DVD]\nvideo:  VTS_01_1.mp4 \
             (250 MiB)\nnext:   newsreel status 6bc1af\n",
        ]
    );
    assert_eq!(
        playing_line("IINA", &film),
        "\u{1b}[38;2;153;255;228m▶\u{1b}[0m Playing in IINA. The download continues: newsreel status 6bc1af\n"
    );
}

#[test]
fn a_held_start_says_why_and_for_how_long() {
    let lines = [
        hold_line(Hold::Reading),
        hold_line(Hold::Buffering(Duration::from_secs(4))),
        hold_line(Hold::Countdown(Duration::from_secs(29 * 60 + 40))),
        hold_line(Hold::Stalled),
    ];
    assert_eq!(
        lines,
        [
            "Buffering…",
            "Buffering… ready in 0:04",
            "Starts in 29:40 so playback does not pause. Press Enter to start now",
            "Waiting for the servers",
        ]
    );
}

#[test]
fn a_title_play_says_each_step_and_why_it_skipped_a_source_in_plain_words() {
    use newsreel_core::{AttemptEvent, Check, Kind, Skip, TitleCard};
    let card = TitleCard {
        id: "tt9999901".parse().unwrap(),
        kind: Kind::Series,
        name: "Harbor Lights".into(),
        year: Some(2019),
        rating: None,
    };
    let mut pack = release(rid("a1b2c3"), "Harbor.Lights.S01.1080p.WEB.x264");
    pack.size = Some(9 * GIB);
    let id = pack.id;
    let skipped = |reason| AttemptEvent::Skipped { release: id, reason };
    let full = Error::new(ErrorCode::DiskFull, "not enough space for Harbor.Lights.S01");
    let steps = [
        AttemptEvent::Resolved { video: "tt9999901:1:2".parse().unwrap(), title: card, episode: None },
        AttemptEvent::Searched { found: 1, sources: Vec::new() },
        AttemptEvent::Trying { rank: 1, of: 1, release: pack.clone() },
        AttemptEvent::Check { check: Check::FileList },
        skipped(Skip::NoFileList { waited_s: 20 }),
        skipped(Skip::NoVideo),
        skipped(Skip::Unplayable { container: "mkv".into() }),
        skipped(Skip::TooLarge { error: full }),
        skipped(Skip::NoData { waited_s: 30 }),
        skipped(Skip::Stalled),
        AttemptEvent::Playing {
            video: "tt9999901:1:2".parse().unwrap(),
            release: pack,
            job: tid("a1b2c3"),
            file: 1,
            resume_at: None,
        },
    ];
    let lines: Vec<Option<String>> = steps.iter().map(|s| step(s, "Playing")).collect();
    assert_eq!(
        lines,
        [
            Some("Finding sources of Harbor Lights S01E02"),
            Some("Found 1 source"),
            Some("Trying 1 of 1: ua1b2c3 Harbor.Lights.S01.1080p.WEB.x264, 1080p h264, 9.0 GiB"),
            None,
            Some("Skipped ua1b2c3: its NZB did not arrive in 20s"),
            Some("Skipped ua1b2c3: none of its files is the video"),
            Some("Skipped ua1b2c3: its .mkv file needs ffmpeg, which is not installed"),
            Some("Skipped ua1b2c3: not enough space for Harbor.Lights.S01"),
            Some("Skipped ua1b2c3: no data arrived in 30s"),
            Some("Skipped ua1b2c3: it stalled"),
            Some("Playing ua1b2c3 Harbor.Lights.S01.1080p.WEB.x264, 1080p h264, 9.0 GiB"),
        ]
        .map(|line| line.map(String::from))
    );
}

#[test]
fn a_title_list_shows_ratings_only_when_a_title_has_one() {
    use newsreel_core::{Kind, TitleCard};
    let card = |id: &str, name: &str, rating| TitleCard {
        id: id.parse().unwrap(),
        kind: Kind::Film,
        name: name.into(),
        year: Some(2010),
        rating,
    };
    let unrated = [card("tt1727587", "Sintel", None)];
    assert_eq!(
        render(Mode::Plain, &titles(&unrated, "sintel"), &()),
        "ID\tKIND\tYEAR\tNAME\ntt1727587\tfilm\t2010\tSintel\n# 1 title\nnext: newsreel title tt1727587\n"
    );
    let mixed = [card("tt1727587", "Sintel", Some(7.4)), card("tt0000001", "Sintel Two", None)];
    assert_eq!(
        render(Mode::Plain, &titles(&mixed, "sintel"), &()),
        "ID\tKIND\tYEAR\tRATING\tNAME\ntt1727587\tfilm\t2010\t7.4\tSintel\ntt0000001\tfilm\t2010\t?\tSintel Two\n\
         # 2 titles\nnext: newsreel title tt1727587\n"
    );
}
