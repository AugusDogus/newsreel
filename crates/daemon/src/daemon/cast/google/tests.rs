use std::net::{Ipv4Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::fake::FakeCast;
use super::*;

/// A web server that answers anything with a few bytes, as the engine answers a cast's URL.
async fn server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let answer = "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\nfilm";
                let _ = stream.write_all(answer.as_bytes()).await;
            });
        }
    });
    at
}

fn load(url: String, start_at: f64) -> Load {
    Load {
        url,
        mime: "video/mp4".into(),
        title: "Charade (1963)".into(),
        film: true,
        duration: Some(6800.0),
        start_at,
        hls: false,
    }
}

async fn playing(session: &mut Session) -> Status {
    for _ in 0..100 {
        let status = session.poll(true).await.unwrap();
        if status.player == Player::Playing {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the fake device never played");
}

#[tokio::test]
async fn a_cast_launches_the_default_media_receiver_and_loads_the_url_where_it_resumes() {
    let (fake, web) = (FakeCast::start(Ipv4Addr::LOCALHOST.into()).await, server().await);
    let mut session = Session::launch(fake.at, "Fake TV").await.unwrap();
    session.load(&load(format!("http://{web}/cast/abc/film.mp4"), 4372.0)).await.unwrap();
    let status = playing(&mut session).await;
    assert!(status.time.is_some_and(|t| t >= 4372.0), "{status:?}");
    assert_eq!(status.volume, Some(50));
    assert_eq!(status.content.as_deref(), Some(format!("http://{web}/cast/abc/film.mp4").as_str()));
    let load = fake.screen().loads[0].clone();
    assert_eq!(
        (&load["media"]["contentType"], &load["media"]["metadata"]["title"], &load["autoplay"]),
        (&json!("video/mp4"), &json!("Charade (1963)"), &json!(true))
    );
    assert_eq!(fake.screen().fetches, ["/cast/film.mp4 200"]);
    let asked: Vec<String> = fake.asked().into_iter().filter(|a| !a.starts_with("connection")).collect();
    assert_eq!(asked[0], "receiver LAUNCH");
    assert!(asked.contains(&"media LOAD".to_string()), "{asked:?}");
}

#[tokio::test]
async fn the_phone_pauses_seeks_resumes_sets_the_volume_and_stops_the_tv() {
    let (fake, web) = (FakeCast::start(Ipv4Addr::LOCALHOST.into()).await, server().await);
    let mut session = Session::launch(fake.at, "Fake TV").await.unwrap();
    session.load(&load(format!("http://{web}/film.mp4"), 0.0)).await.unwrap();
    playing(&mut session).await;
    session.pause().await.unwrap();
    assert_eq!(session.poll(false).await.unwrap().player, Player::Paused);
    session.seek(600.0).await.unwrap();
    let paused = session.poll(false).await.unwrap();
    assert_eq!((paused.player, paused.time.map(f64::round)), (Player::Paused, Some(600.0)));
    session.play().await.unwrap();
    session.volume(12).await.unwrap();
    assert_eq!(session.poll(true).await.unwrap().volume, Some(12));
    session.stop().await.unwrap();
    assert!(fake.screen().app.is_none(), "the Default Media Receiver is closed");
    let asked = fake.asked();
    let tail: Vec<&str> = asked
        .iter()
        .map(String::as_str)
        .filter(|a| !a.ends_with("GET_STATUS") && !a.starts_with("connection"))
        .collect();
    assert_eq!(
        tail,
        [
            "receiver LAUNCH",
            "media LOAD",
            "media PAUSE",
            "media SEEK",
            "media PLAY",
            "receiver SET_VOLUME",
            "media STOP",
            "receiver STOP"
        ]
    );
}

#[tokio::test]
async fn the_tvs_own_remote_shows_in_the_status_and_a_dropped_connection_is_opened_again() {
    let (fake, web) = (FakeCast::start(Ipv4Addr::LOCALHOST.into()).await, server().await);
    let mut session = Session::launch(fake.at, "Fake TV").await.unwrap();
    session.load(&load(format!("http://{web}/film.mp4"), 0.0)).await.unwrap();
    playing(&mut session).await;
    fake.remote("PAUSE", None);
    assert_eq!(session.poll(false).await.unwrap().player, Player::Paused);
    fake.remote("", Some(3000.0));
    assert_eq!(session.poll(false).await.unwrap().time.map(f64::round), Some(3000.0));

    fake.hang_up();
    // The first poll after may still find the old connection open; one after it must not.
    let mut polled = session.poll(false).await;
    for _ in 0..50 {
        if polled.as_ref().is_ok_and(|status| status.player == Player::Paused)
            && session.channel.as_ref().is_some_and(|c| !c.closed())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        polled = session.poll(false).await;
    }
    let status = polled.unwrap();
    assert_eq!((status.player, status.closed, status.time.map(f64::round)), (Player::Paused, false, Some(3000.0)));
    assert_eq!(fake.screen().sessions, 1, "the same session plays on");

    fake.remote("STOP", None);
    assert_eq!(session.poll(false).await.unwrap().player, Player::Idle(Idle::Cancelled));
}

#[tokio::test]
async fn a_receiver_that_cannot_play_the_video_says_so_and_another_sender_closes_the_app() {
    let (fake, web) = (FakeCast::start(Ipv4Addr::LOCALHOST.into()).await, server().await);
    fake.screen().refuse_loads = true;
    let mut session = Session::launch(fake.at, "Fake TV").await.unwrap();
    let refused = session.load(&load(format!("http://{web}/film.mkv"), 0.0)).await.err().map(Error::from).unwrap();
    assert_eq!(refused.message, "Fake TV could not play the video (load failed)");
    assert_eq!(session.poll(false).await.unwrap().failed.as_deref(), Some("load failed"));

    // Another phone's app takes the TV.
    fake.screen().app = Some(("session-other".into(), "web-other".into()));
    assert!(session.poll(true).await.unwrap().closed);
}
