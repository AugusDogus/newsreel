//! Play on TV against a fake Cast device on this machine's address on the home network: it speaks
//! CASTV2 over TLS, runs a Default Media Receiver, and fetches what it loads from a page on
//! Google's origin, as a Google TV does.

use std::time::{Duration, Instant};

use serde_json::json;

use newsreel_core::protocol::{Request, Response};
use newsreel_core::{Cast, CastAction, CastRequest, CastState, Error, ErrorCode, Target, TvList, VideoRef};

use super::super::cast::google;
use super::super::cast::google::Heard;
use super::super::cast::google::fake::FakeCast;
use super::super::fake;
use super::devices::lan;
use super::{GIB, Harness, MIB, Outside, prefix, release, written};
use crate::ffmpeg::Tools;

fn harness(devices: Vec<Heard>) -> Harness {
    Harness::with_outside(
        fake::Search::new(Vec::new()),
        Outside { cast_devices: google::Find::At(devices), ..Outside::default() },
    )
}

async fn tvs(h: &Harness, fresh: bool) -> TvList {
    match h.ask(Request::Tvs { fresh }).await {
        Response::Tvs { list } => list,
        other => panic!("not a TV list: {other:?}"),
    }
}

async fn cast(h: &Harness, request: CastRequest) -> Result<Cast, Error> {
    match h.ask_cli(Request::Cast(request)).await {
        Response::Cast { cast } => Ok(*cast),
        Response::Error { error } => Err(error),
        other => panic!("not a cast: {other:?}"),
    }
}

async fn control(h: &Harness, action: CastAction) -> Result<Cast, Error> {
    match h.ask(Request::CastControl { tv: None, action }).await {
        Response::Cast { cast } => Ok(*cast),
        Response::Error { error } => Err(error),
        other => panic!("not a cast: {other:?}"),
    }
}

async fn until(h: &Harness, what: &str, done: impl Fn(&Cast) -> bool) -> Cast {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let casts = tvs(h, false).await.casts;
        if let Some(cast) = casts.iter().find(|cast| done(cast)) {
            return cast.clone();
        }
        assert!(Instant::now() < deadline, "the cast never reached {what}: {casts:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn night() -> VideoRef {
    "tt0063350".parse().unwrap()
}

fn request(film: &fake::Posting) -> CastRequest {
    CastRequest {
        tv: None,
        target: Target::Prefix(prefix(film)),
        source: None,
        file: None,
        video: Some(night()),
        subtitle: None,
        start_at: None,
    }
}

fn kept(h: &Harness) -> Option<(f64, Option<f64>)> {
    let watched = h.running.ctx.titles.shelf.watched(&night().title).unwrap();
    watched.iter().find(|w| w.video == night()).map(|w| (w.position, w.duration))
}

fn near(position: Option<f64>, at: f64) -> bool {
    position.is_some_and(|position| (at..at + 5.0).contains(&position))
}

/// A release of one MP4 downloaded whole, and a harness that finds `tv`.
async fn downloaded(tv: &FakeCast, name: &str) -> (Harness, fake::Posting) {
    let h = harness(vec![tv.heard("Fake Google TV")]);
    let film = release(81, "Night", &[(name, 4 * MIB)]);
    written(&h.downloads().join(format!("Night/{name}")), 4 * MIB);
    h.network.set_rate(100 * GIB);
    h.get(h.nzb_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    (h, film)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cast_device_is_listed_by_its_name_beside_the_dlna_tvs_and_speakers_are_not() {
    let ip = lan();
    let tv = FakeCast::start(ip.into()).await;
    let speaker = Heard {
        name: "Kitchen speaker".into(),
        model: "Google Home Mini".into(),
        capabilities: 199172,
        ..tv.heard("")
    };
    let group = Heard {
        id: "group".into(),
        name: "Home Speakers".into(),
        model: "Google Cast Group".into(),
        capabilities: 199204,
        ..tv.heard("")
    };
    let h = harness(vec![tv.heard("Augie's Bedroom Google TV"), speaker, group]);
    let found = serde_json::to_value(&tvs(&h, true).await.tvs).unwrap();
    assert_eq!(
        found,
        json!([{
            "id": format!("cast:fake{}@{ip}", tv.at.port()), "name": "Augie's Bedroom Google TV", "protocol": "google_cast",
            "address": ip.to_string(), "plays": ["mp4", "webm"], "decodes": "h264", "volume": true,
        }])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mp4_casts_as_it_is_and_the_phone_and_the_tvs_own_remote_drive_it() {
    let ip = lan();
    let tv = FakeCast::start(ip.into()).await;
    let (h, film) = downloaded(&tv, "Night.mp4").await;
    let playing = cast(&h, request(&film)).await.unwrap();
    assert_eq!((playing.name.as_str(), &playing.state), ("Night of the Living Dead (1968)", &CastState::Playing));
    let load = tv.screen().loads[0].clone();
    let url = load["media"]["contentId"].as_str().unwrap().to_string();
    let port = h.running.ctx.http_port;
    assert!(url.starts_with(&format!("http://{ip}:{port}/cast/")) && url.ends_with("/Night.mp4"), "{url}");
    assert_eq!(
        (&load["media"]["contentType"], &load["media"]["streamType"]),
        (&json!("video/mp4"), &json!("BUFFERED"))
    );
    // From a page on another origin, which the engine lets read the file.
    assert_eq!(tv.screen().fetches, ["/cast/Night.mp4 206"]);

    tv.remote("PAUSE", None);
    until(&h, "paused by the TV's remote", |cast| cast.state == CastState::Paused).await;
    tv.remote("PLAY", None);
    until(&h, "playing again", |cast| cast.state == CastState::Playing).await;
    control(&h, CastAction::Seek { position: 1_800.0 }).await.unwrap();
    until(&h, "half an hour in", |cast| near(cast.position, 1_800.0)).await;
    assert_eq!(control(&h, CastAction::Volume { level: 30 }).await.unwrap().volume, Some(30));
    assert_eq!(tv.screen().volume, 0.3);
    assert_eq!(control(&h, CastAction::Pause).await.unwrap().state, CastState::Paused);
    let (position, duration) = kept(&h).expect("a pause keeps the position");
    assert!(near(Some(position), 1_800.0) && duration == Some(5_760.0), "{position} {duration:?}");

    // A seek on the TV's own remote, which newsreel hears and keeps.
    tv.remote("", Some(3_000.0));
    until(&h, "fifty minutes in", |cast| near(cast.position, 3_000.0)).await;
    let stopped = control(&h, CastAction::Stop).await.unwrap();
    assert_eq!(stopped.state, CastState::Ended { error: None });
    assert!(near(kept(&h).map(|k| k.0), 3_000.0), "a stop keeps the position: {:?}", kept(&h));
    assert!(tv.screen().app.is_none(), "the stop closes the Default Media Receiver");
    let fetched = reqwest::Client::new().get(&url).send().await;
    assert!(fetched.is_err() || fetched.is_ok_and(|answer| answer.status() == 404), "the grant ended with the cast");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tvs_own_remote_stops_the_cast_and_drops_its_grant() {
    let tv = FakeCast::start(lan().into()).await;
    let (h, film) = downloaded(&tv, "Night.mp4").await;
    cast(&h, request(&film)).await.unwrap();
    let url = tv.screen().loads[0]["media"]["contentId"].as_str().unwrap().to_string();
    tv.remote("", Some(600.0));
    until(&h, "ten minutes in", |cast| near(cast.position, 600.0)).await;
    tv.remote("STOP", None);
    let ended = until(&h, "ended", Cast::ended).await;
    assert_eq!(ended.state, CastState::Ended { error: None });
    assert!(near(kept(&h).map(|k| k.0), 600.0), "{:?}", kept(&h));
    let fetched = reqwest::Client::new().get(&url).send().await;
    assert!(fetched.is_err() || fetched.is_ok_and(|answer| answer.status() == 404), "the grant ended with the cast");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cast_starts_where_it_resumes_and_seeks_there_when_the_tv_starts_at_the_top() {
    let tv = FakeCast::start(lan().into()).await;
    let (h, film) = downloaded(&tv, "Night.mp4").await;
    let resumed = CastRequest { start_at: Some(4_372.0), ..request(&film) };
    cast(&h, resumed.clone()).await.unwrap();
    assert_eq!(tv.screen().loads[0]["currentTime"], json!(4_372.0));
    until(&h, "where it resumes", |cast| near(cast.position, 4_372.0)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(!tv.asked().contains(&"media SEEK".to_string()), "the TV started there, so no seek: {:?}", tv.asked());
    control(&h, CastAction::Stop).await.unwrap();

    // A TV that plays from the top whatever it is asked.
    tv.screen().starts_at_top = true;
    tokio::time::sleep(Duration::from_millis(100)).await;
    cast(&h, resumed).await.unwrap();
    tv.until("a seek to where it resumes", |screen| screen.asked.iter().any(|a| a == "media SEEK")).await;
    until(&h, "where it resumes", |cast| cast.state == CastState::Playing && near(cast.position, 4_372.0)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_connection_is_opened_again_and_the_cast_plays_on() {
    let tv = FakeCast::start(lan().into()).await;
    let (h, film) = downloaded(&tv, "Night.mp4").await;
    cast(&h, request(&film)).await.unwrap();
    let connects = |tv: &FakeCast| tv.asked().iter().filter(|a| *a == "connection CONNECT").count();
    let before = connects(&tv);
    tv.hang_up();
    tv.until("the engine connecting again", |screen| {
        screen.asked.iter().filter(|a| *a == "connection CONNECT").count() >= before + 2
    })
    .await;
    tv.remote("", Some(900.0));
    let cast = until(&h, "fifteen minutes in", |cast| near(cast.position, 900.0)).await;
    assert_eq!(cast.state, CastState::Playing);
    assert_eq!(tv.screen().sessions, 1, "the same session plays on");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_matroska_release_needs_ffmpeg_for_a_cast_device() {
    let tv = FakeCast::start(lan().into()).await;
    let (h, film) = downloaded(&tv, "Night.mkv").await;
    let error = cast(&h, request(&film)).await.unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (ErrorCode::Usage, "Fake Google TV does not play MKV files, only MP4 and WEBM")
    );
    let install = format!("{} to convert it for the TV, or play it here", crate::ffmpeg::INSTALL);
    assert_eq!(error.hint.as_deref(), Some(install.as_str()));
}

/// A 60-second Matroska film `size` large: H.264 with a keyframe every 2 seconds, and AC-3,
/// which a Cast device is sent as AAC.
fn matroska(dir: &std::path::Path, tools: &Tools, size: &str) -> Option<Vec<u8>> {
    let made = std::process::Command::new(tools.ffmpeg.as_ref()?)
        .current_dir(dir)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("testsrc2=size={size}:rate=24:duration=60")])
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=r=48000:d=60",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .args(["-g", "48", "-c:a", "ac3", "-b:a", "96k", "made.mkv"])
        .status()
        .is_ok_and(|status| status.success());
    made.then(|| std::fs::read(dir.join("made.mkv")).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an ffmpeg on the PATH that encodes H.264 and AC-3; run it with --ignored"]
async fn a_matroska_film_casts_as_hls_cut_on_its_keyframes_and_the_tvs_remote_seeks_far_ahead() {
    let tv = FakeCast::start(lan().into()).await;
    let h = harness(vec![tv.heard("Fake Google TV")]);
    let tools = Tools::find(&crate::env::Env::from_process());
    let made = tools
        .both()
        .and_then(|_| matroska(h.dir.path(), &tools, "160x90"))
        .expect("this machine's ffmpeg makes the film");
    *h.outside.tools.lock().unwrap() = tools;
    h.network.set_rate(100 * GIB);
    let film = release(82, "Night", &[("Night.mkv", made.len() as u64)]);
    std::fs::create_dir_all(h.downloads().join("Night")).unwrap();
    std::fs::write(h.downloads().join("Night/Night.mkv"), &made).unwrap();
    h.get(h.nzb_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;

    cast(&h, request(&film)).await.unwrap();
    let load = tv.screen().loads[0].clone();
    let url = load["media"]["contentId"].as_str().unwrap().to_string();
    assert!(url.ends_with("/film.m3u8"), "{url}");
    assert_eq!(
        (&load["media"]["contentType"], &load["media"]["hlsSegmentFormat"], &load["media"]["duration"]),
        (&json!("application/vnd.apple.mpegurl"), &json!("fmp4"), &json!(60.0))
    );
    let playlist = reqwest::get(&url).await.unwrap().text().await.unwrap();
    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD") && playlist.ends_with("#EXT-X-ENDLIST\n"), "{playlist}");
    let segments = playlist.lines().filter(|line| line.ends_with(".m4s")).count();
    assert_eq!(segments, 10, "six-second segments on two-second keyframes: {playlist}");
    assert_eq!(tv.screen().fetches, ["/cast/film.m3u8 200", "/cast/init.mp4 200", "/cast/0.m4s 200"]);

    // The TV's own remote, far past what has been made.
    tv.remote_seek(50.0).await;
    assert_eq!(tv.screen().fetches[3..], ["/cast/film.m3u8 200", "/cast/init.mp4 200", "/cast/8.m4s 200"]);
    until(&h, "fifty seconds in", |cast| near(cast.position, 50.0)).await;
    let segment = reqwest::get(url.replace("film.m3u8", "8.m4s")).await.unwrap().bytes().await.unwrap();
    assert_eq!(&segment[4..8], b"moof", "a segment is fragments alone; the init segment holds the moov");

    control(&h, CastAction::Stop).await.unwrap();
    // The stop answers first, and the cast lets go of its film after.
    let folder = h.dir.path().join("plays/.cast");
    let left = || -> Vec<_> { std::fs::read_dir(&folder).map(|dir| dir.flatten().collect()).unwrap_or_default() };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !left().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(left().is_empty(), "the film's segments go with the cast: {:?}", left());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an ffmpeg on the PATH that encodes H.264 and AC-3; run it with --ignored"]
async fn a_tv_that_refuses_a_film_taller_than_its_screen_gets_one_at_720p_and_so_does_its_next_cast() {
    let tv = FakeCast::start(lan().into()).await;
    tv.screen().tallest_film = Some(720);
    let h = harness(vec![tv.heard("Fake Google TV")]);
    let tools = Tools::find(&crate::env::Env::from_process());
    let made = tools
        .both()
        .and_then(|_| matroska(h.dir.path(), &tools, "1280x800"))
        .expect("this machine's ffmpeg makes the film");
    *h.outside.tools.lock().unwrap() = tools;
    h.network.set_rate(100 * GIB);
    let film = release(83, "Night", &[("Night.mkv", made.len() as u64)]);
    std::fs::create_dir_all(h.downloads().join("Night")).unwrap();
    std::fs::write(h.downloads().join("Night/Night.mkv"), &made).unwrap();
    h.get(h.nzb_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;

    let playing = cast(&h, request(&film)).await.unwrap();
    assert_eq!(playing.state, CastState::Playing);
    let loads = tv.screen().loads.clone();
    assert_eq!(loads.len(), 2, "the film at its own size, then at 720p");
    assert_ne!(loads[0]["media"]["contentId"], loads[1]["media"]["contentId"], "each from a grant of its own");
    let init = reqwest::get(loads[1]["media"]["contentId"].as_str().unwrap().replace("film.m3u8", "init.mp4"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let tkhd = init.windows(4).position(|w| w == b"tkhd").unwrap();
    let size = u32::from_be_bytes(init[tkhd - 4..tkhd].try_into().unwrap()) as usize;
    let height = u32::from_be_bytes(init[tkhd - 4 + size - 4..tkhd - 4 + size].try_into().unwrap()) >> 16;
    assert_eq!(height, 720, "1280x800 fits 1152x720");

    control(&h, CastAction::Stop).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    cast(&h, request(&film)).await.unwrap();
    assert_eq!(tv.screen().loads.len(), 3, "the next cast goes straight to 720p");
}
