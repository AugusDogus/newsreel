use newsreel_core::Error;
use newsreel_core::time::now;
use serde::Serialize;

use crate::cli::UpdateArgs;
use crate::output::{self, Body, Cell, Doc, DocKind, Mode, Typed};
use newsreel_daemon::config::Policy;
use newsreel_daemon::env::Env;
use newsreel_daemon::fsutil::write_atomic;
use newsreel_daemon::paths::Paths;
use newsreel_daemon::update::release::{self, Outcome, Releases};
use newsreel_daemon::update::{Audience, Checked, UPDATED, Updated, Version, due, read_json};

#[derive(Serialize)]
struct CheckReport<'a> {
    current: &'a str,
    /// None while no release is published.
    latest: Option<String>,
    update_available: bool,
    policy: Policy,
}

#[derive(Serialize)]
struct UpdateReport<'a> {
    current: &'a str,
    /// None while no release is published.
    latest: Option<String>,
    result: Installed,
}

newsreel_core::words! {
    enum Installed {
        Updated = "updated",
        UpToDate = "up_to_date",
        Unpublished = "unpublished",
    }
}

pub fn run(args: UpdateArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let releases = Releases::required(env)?;
    let runtime = release::runtime()?;
    let current = env.version();
    if args.check {
        let policy = newsreel_daemon::config::updates(&paths)?;
        let latest = runtime.block_on(release::check(&releases, &paths, now()))?;
        let update_available = latest.is_some_and(|latest| latest.newer_than(&current));
        let fields = vec![
            ("current", Cell::new(&current)),
            ("latest", Cell::new(latest.map_or_else(|| "none published".to_string(), |latest| latest.to_string()))),
            ("policy", Cell::new(policy.as_str())),
        ];
        let summary = match latest {
            Some(_) => Vec::new(),
            None => vec!["no release of newsreel is published yet, so there is nothing to update".to_string()],
        };
        let doc = Doc {
            body: Body::Fields { title: Vec::new(), fields },
            summary,
            next: update_available.then(|| "newsreel update".to_string()),
        };
        let latest = latest.map(|latest| latest.to_string());
        let body = CheckReport { current: &current, latest, update_available, policy };
        return Ok(super::emit(mode, &doc, &Typed { kind: DocKind::UpdateCheck, body }));
    }
    let (latest, result) = match runtime.block_on(release::install(&releases, &paths, env, now()))? {
        Outcome::Updated(latest) => (Some(latest), Installed::Updated),
        Outcome::Latest(latest) => (Some(latest), Installed::UpToDate),
        Outcome::Unpublished => (None, Installed::Unpublished),
    };
    let shown = latest.map_or_else(String::new, |latest| latest.to_string());
    match mode {
        Mode::Json => {
            let body = UpdateReport { current: &current, latest: latest.map(|latest| latest.to_string()), result };
            print!("{}", output::json_document(&Typed { kind: DocKind::Update, body }, None));
        }
        _ if result == Installed::Updated => println!("Updated newsreel {current} → {shown}."),
        _ if result == Installed::Unpublished => {
            println!("No release of newsreel is published yet, so there is nothing to install; {current} stays.")
        }
        _ => println!("newsreel {current} is up to date."),
    }
    Ok(0)
}

pub fn available(paths: &Paths, env: &Env) -> Option<Version> {
    if newsreel_daemon::config::updates(paths).ok()? == Policy::Off {
        return None;
    }
    Checked::read(paths).newer(&env.version())
}

pub fn notice(paths: &Paths, env: &Env, audience: Audience, now: i64) -> Option<String> {
    let updated = paths.state.join(UPDATED);
    if let Some(Updated { from, to }) = read_json(&updated)
        && std::fs::remove_file(&updated).is_ok()
    {
        return Some(audience.updated(&from, &to));
    }
    let latest = available(paths, env)?;
    let file = audience.notice_file(paths);
    let last = std::fs::read_to_string(&file).ok().and_then(|text| text.trim().parse().ok());
    if !due(last, now) {
        return None;
    }
    let _ = write_atomic(&file, now.to_string().as_bytes());
    Some(audience.out(&env.version(), latest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use newsreel_daemon::update::{DAY, HOUR};

    #[test]
    fn each_audience_hears_once_a_day_and_off_silences_both() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let env = Env::from_pairs([("HOME", home.as_str()), ("NEWSREEL_HOME", home.as_str())]);
        let paths = Paths::resolve(&env, 501, None).unwrap();
        let t = 1_790_000_000;
        Checked { latest: Some("9.0.0".into()), checked_at: Some(t), ..Checked::default() }.write(&paths);
        let heard = |audience, now| notice(&paths, &env, audience, now).is_some();

        assert_eq!(
            [heard(Audience::Person, t), heard(Audience::Person, t + HOUR), heard(Audience::Agent, t + HOUR)],
            [true, false, true]
        );
        assert!(heard(Audience::Person, t + DAY));
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(paths.config.join("config.yaml"), "updates: off\n").unwrap();
        assert!(!heard(Audience::Person, t + 3 * DAY));
    }
}
