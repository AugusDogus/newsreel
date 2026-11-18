//! Changes to config.yaml while the engine runs, by hand or by another program. The engine reads
//! its preferences, update policy, and the player afresh each time it needs them, and can search a
//! new list of indexers at once ([`SearchPort::reconfigure`](super::search_port::SearchPort)). The
//! rest it reads once, at start: the Usenet servers' connections, the folders and the network
//! address are set up then. For those it logs, and `newsreel engine status` says, that a restart
//! is needed.

use std::sync::Arc;
use std::time::Duration;

use super::ctx::Ctx;
use crate::config::{self, Config};
use crate::env::Env;
use crate::fsutil;
use crate::log::log;
use crate::paths::Paths;

/// How often the engine looks at config.yaml.
const LOOK_EVERY: Duration = Duration::from_secs(2);

/// Settings the engine reads only at start that differ between `started`, what it runs with, and
/// `now`, by their names in config.yaml.
pub fn needs_restart(started: &Config, now: &Config) -> Vec<&'static str> {
    let changed = [
        ("downloads.dir", started.downloads != now.downloads),
        ("usenet.servers", started.usenet != now.usenet),
        ("subtitles", started.subtitles != now.subtitles),
        ("catalog", started.catalog != now.catalog),
        ("ffmpeg", started.ffmpeg != now.ffmpeg),
        ("plays.max", started.plays_max != now.plays_max),
        ("server", started.exposure.server != now.exposure.server),
        ("public_address", started.exposure.public_address != now.exposure.public_address),
    ];
    changed.into_iter().filter(|(_, changed)| *changed).map(|(name, _)| name).collect()
}

/// What `newsreel engine status` says while changes to `settings` wait for a restart.
pub fn notice(settings: &[&str]) -> String {
    format!(
        "config.yaml changed {} since the engine started; stop it with newsreel engine stop, and the next newsreel command starts it with them",
        settings.join(", ")
    )
}

/// Looks at config.yaml every couple of seconds, until the engine stops. `started` is what the
/// engine runs with, and `server` whether it was started as a server whatever the file says.
pub async fn follow(ctx: Arc<Ctx>, paths: Paths, env: Env, started: Config, server: bool) {
    let notice_file = paths.restart_needed();
    crate::log::persist(&format!("remove {}", notice_file.display()), fsutil::remove(&notice_file));
    let read = |paths: &Paths| config::file(paths).ok().and_then(|file| std::fs::read(file).ok());
    let mut seen = read(&paths);
    let mut searching = started.search.clone();
    let mut waiting: Vec<&'static str> = Vec::new();
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        let text = read(&paths);
        if text == seen {
            continue;
        }
        seen = text;
        let mut now = match Config::load(&paths, &env) {
            Ok(now) => now,
            Err(error) => {
                log(&format!(
                    "config.yaml changed but does not load, so the engine keeps its settings: {}",
                    error.message
                ));
                continue;
            }
        };
        now.exposure.server |= server;
        if now.search != searching {
            match ctx.search.reconfigure(&now.search) {
                Ok(()) => {
                    let count = newsreel_core::units::count(now.search.indexers.len(), "indexer");
                    log(&format!("config.yaml changed its indexers, so searches ask its {count} now"));
                    searching = now.search.clone();
                }
                Err(error) => log(&format!("cannot search the indexers config.yaml lists now: {}", error.message)),
            }
        }
        let changed = needs_restart(&started, &now);
        if changed != waiting {
            if changed.is_empty() {
                log("config.yaml is back to the settings the engine started with");
                crate::log::persist(&format!("remove {}", notice_file.display()), fsutil::remove(&notice_file));
            } else {
                let notice = notice(&changed);
                log(&notice);
                let written = fsutil::write_atomic(&notice_file, notice.as_bytes());
                crate::log::persist(&format!("write {}", notice_file.display()), written);
            }
            waiting = changed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_the_engine_reads_once_needs_a_restart() {
        let started = Config::default();
        let mut now = Config { preferences: newsreel_core::Preferences::default(), ..Config::default() };
        now.updates = config::Policy::Off;
        assert!(needs_restart(&started, &now).is_empty(), "updates are read when they are checked");
        now.search.disabled = vec!["hydra".into()];
        assert!(needs_restart(&started, &now).is_empty(), "searches take new indexers at once");
        now.downloads = Some("/media/films".into());
        now.exposure.server = true;
        assert_eq!(needs_restart(&started, &now), ["downloads.dir", "server"]);
    }
}
