//! Cells, rows and short IDs that more than one command prints.

use newsreel_core::id::shortest_unique_prefix;
use newsreel_core::text::sanitize;
use newsreel_core::units::format_size;
use newsreel_core::{Category, DownloadId, Job, JobStatus, Profile, Release, ResultId, Selection};

use crate::output::Cell;
use crate::output::format::{self, NONE};
use crate::output::theme::Token;

pub use newsreel_core::id::SHORT_ID;

pub fn short_ids(ids: &[DownloadId]) -> Vec<String> {
    ids.iter().map(|id| shortest_unique_prefix(id, ids, SHORT_ID)).collect()
}

/// The shortest `u` prefix that tells each search result apart from the others shown.
pub fn release_ids(releases: &[Release]) -> Vec<String> {
    let as_hex_id = |id: &ResultId| DownloadId::from_bytes(*id.as_bytes());
    let all: Vec<DownloadId> = releases.iter().map(|r| as_hex_id(&r.id)).collect();
    all.iter().map(|id| format!("u{}", shortest_unique_prefix(id, &all, SHORT_ID))).collect()
}

pub fn short_ref(id: &str) -> String {
    let (lead, hex) = id.strip_prefix('u').map_or(("", id), |rest| ("u", rest));
    format!("{lead}{}", &hex[..hex.len().min(SHORT_ID)])
}

/// A search result as a row: its ID, grabs, size, quality, age, source, flags, and name.
pub fn release_row(r: &Release, id: &str, video: bool, now: i64) -> Vec<Cell> {
    let source = match r.sources.as_slice() {
        [] => NONE.to_string(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first}+{}", rest.len()),
    };
    let flags: Vec<&str> = r.flags.iter().map(|f| f.as_str()).collect();
    let quality = r.quality().to_string();
    let unknown = if video || r.category == Some(Category::Video) { format::UNKNOWN } else { NONE };
    vec![
        Cell::styled(id, Token::Accent),
        Cell::new(r.grabs.map_or_else(|| format::UNKNOWN.to_string(), |g| g.to_string())),
        Cell::new(format::size(r.size)),
        if quality.is_empty() { Cell::styled(unknown, Token::Muted) } else { Cell::new(quality) },
        Cell::new(format::age(r.published, now)),
        Cell::styled(source, Token::Muted),
        if flags.is_empty() { Cell::styled(NONE, Token::Muted) } else { Cell::styled(flags.join(","), Token::Mint) },
        Cell::new(&r.name),
    ]
}

pub fn ranked_for(profile: &Profile) -> String {
    match profile.overrides() {
        Some(overrides) => format!("{} · overrides: {overrides}", profile.describe()),
        None => profile.describe(),
    }
}

pub fn selection(selection: &Selection) -> String {
    match selection {
        Selection::All => "all".to_string(),
        Selection::Patterns(patterns) => patterns.iter().map(|p| sanitize(p)).collect::<Vec<_>>().join(" "),
    }
}

pub fn state_cell(status: &JobStatus) -> Cell {
    let token = match status {
        JobStatus::Downloading | JobStatus::Checking => Token::Mint,
        JobStatus::Done { .. } => Token::Mint,
        JobStatus::Queued | JobStatus::Paused => Token::Accent,
        JobStatus::Error { .. } => Token::Error,
    };
    Cell::styled(status.name(), token)
}

pub fn next_for_stopped(status: &JobStatus, id: &str) -> Option<String> {
    match status {
        JobStatus::Error { error } if error.next.is_some() => error.next.as_deref().cloned(),
        JobStatus::Paused | JobStatus::Error { .. } => Some(format!("newsreel resume {id}")),
        _ => None,
    }
}

pub fn job_row(id: &str, job: &Job) -> Vec<Cell> {
    let active = matches!(job.status, JobStatus::Checking | JobStatus::Downloading);
    let downloading = job.status == JobStatus::Downloading;
    vec![
        Cell::styled(id, Token::Accent),
        state_cell(&job.status),
        Cell::new(format::percent(&job.progress)),
        Cell::new(format_size(job.progress.size)),
        Cell::new(if downloading { format::rate(job.progress.down_rate) } else { NONE.to_string() }),
        Cell::new(if active { job.progress.connections.to_string() } else { NONE.to_string() }),
        Cell::new(if downloading { format::eta(job.progress.eta) } else { NONE.to_string() }),
        Cell::new(&job.name),
    ]
}

pub fn rerun_get(id: &str, selection: &Selection, dir: Option<String>) -> String {
    let mut words = vec!["newsreel".to_string(), "get".to_string(), id.to_string()];
    if let Selection::Patterns(patterns) = selection {
        for p in patterns {
            words.push("--files".to_string());
            words.push(format::shell_word(&sanitize(p)));
        }
    }
    if let Some(dir) = dir {
        words.push("--dir".to_string());
        words.push(dir);
    }
    words.join(" ")
}
