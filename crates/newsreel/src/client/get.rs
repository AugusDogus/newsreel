//! `newsreel get`: starts a download, or plans one with `--dry-run`.

use std::path::Path;

use newsreel_core::protocol::{GetOutcome, GetReply, GetRequest, Request, Response, TitlePlayed, TitleRequest};
use newsreel_core::text::sanitize;
use newsreel_core::units::{format_limit, format_size};
use newsreel_core::{Error, Job, Target};
use serde::Serialize;

use super::connect::{self, Conn};
use super::emit;
use super::rows::{next_for_stopped, rerun_get, selection, short_ids, state_cell};
use super::{absolute, titles, wait};
use crate::cli::GetArgs;
use crate::output::format;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};
use newsreel_daemon::env::Env;
use newsreel_daemon::paths::Paths;

pub fn run(mut conn: Conn, target: Target, args: GetArgs, mode: Mode, cwd: &Path, env: &Env) -> Result<u8, Error> {
    if args.episode.is_some() {
        return Err(titles::for_a_title("an episode"));
    }
    if args.source.is_some() {
        return Err(titles::for_a_title("--source"));
    }
    if !args.files.is_empty() {
        newsreel_daemon::selection::compile(&args.files)?;
    }
    let dir = args.dir.map(|d| absolute(&d, cwd, env));
    let dry_run = args.dry_run;
    let request =
        Request::Get(GetRequest { target, dir: dir.clone(), files: args.files, max_size: args.max_size, dry_run });
    let reply: Box<GetReply> = conn.ask(&request)?;
    let exit = emit(mode, &get(&reply, dir.as_deref()), &Response::Get { reply: reply.clone() });
    if dry_run {
        return Ok(exit);
    }
    follow(exit, &reply.job, mode, env)
}

/// Downloads a film or an episode from the first of its sources that starts. The flags that pick
/// files and folders take a release.
pub fn title(engine: impl FnOnce() -> Result<Conn, Error>, args: GetArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    let video = titles::video(&args.reference, args.episode.as_deref())?;
    let release_only = [
        (!args.files.is_empty(), "--files"),
        (args.dir.is_some(), "--dir"),
        (args.max_size.is_some(), "--max-size"),
        (args.dry_run, "--dry-run"),
    ];
    if let Some((_, flag)) = release_only.into_iter().find(|(given, _)| *given) {
        return Err(newsreel_core::target::not_a_release(&video, flag));
    }
    let (exit, job) = request_title(&mut engine()?, TitleRequest { video, source: args.source }, mode)?;
    follow(exit, &job, mode, env)
}

/// Downloads a film or an episode from the first of its sources that starts, and answers the exit
/// code and the download.
fn request_title(conn: &mut Conn, request: TitleRequest, mode: Mode) -> Result<(u8, Job), Error> {
    let video = request.video.clone();
    let shown = titles::Shown::new(mode);
    let answered = titles::steps(conn, &Request::GetTitle(request), &video, |step| shown.step(step, "Downloading"));
    drop(shown);
    let (reply, title): (Box<GetReply>, Box<TitlePlayed>) = answered?;
    let (reply, title) = (*reply, *title);
    #[derive(Serialize)]
    struct Got<'a> {
        reply: &'a GetReply,
        title: &'a TitlePlayed,
    }
    let doc = get_title(&reply, &title.name);
    let exit = emit(mode, &doc, &Typed { kind: DocKind::Get, body: Got { reply: &reply, title: &title } });
    Ok((exit, reply.job))
}

/// In a terminal, follows a download `get` started until it finishes.
fn follow(exit: u8, job: &Job, mode: Mode, env: &Env) -> Result<u8, Error> {
    if mode != Mode::Human || job.status.is_stopped() {
        return Ok(exit);
    }
    eprintln!("Press Ctrl-C to stop watching. The download continues in the background.");
    let paths = Paths::from_process(env)?;
    let following = (connect::ensure(&paths, env)?, wait::Engine { paths, env: env.clone() });
    wait::attach(following, vec![job.id.to_hex()], None)
}

pub fn get(reply: &GetReply, dir: Option<&Path>) -> Doc {
    let job = &reply.job;
    let id = short_ids(&[job.id]).remove(0);
    let (word, token) = match reply.outcome {
        GetOutcome::Started => ("started", Token::Mint),
        GetOutcome::Queued => ("queued", Token::Accent),
        GetOutcome::Existing => ("existing", Token::Muted),
        GetOutcome::Updated => ("updated", Token::Accent),
        GetOutcome::Planned => ("planned", Token::Accent),
    };
    let title = vec![Cell::styled(word, token), Cell::styled(id.clone(), Token::Accent), Cell::new(&job.name)];
    let mut fields = Vec::new();
    if reply.outcome != GetOutcome::Planned {
        fields.push(("state", state_cell(&job.status)));
    }
    let files = match &reply.plan {
        Some(plan) => format!(
            "{} ({} of {}, {})",
            selection(&job.selection),
            plan.selected_files,
            plan.total_files,
            format_size(plan.selected_size)
        ),
        None => format!("{} ({})", selection(&job.selection), format_size(job.progress.size)),
    };
    fields.push(("files", Cell::new(files)));
    if let Some(previous) = &reply.previous_selection {
        fields.push(("was", Cell::new(selection(previous))));
    }
    fields.push(("path", Cell::new(job.path.display().to_string())));
    let mut summary = Vec::new();
    if reply.outcome == GetOutcome::Planned
        && let Some(plan) = &reply.plan
    {
        fields.push(("free", Cell::new(format_size(plan.free_space))));
        if let Some(cap) = plan.max_size {
            fields.push(("cap", Cell::new(format_limit(cap))));
        }
        let conflicts = if plan.conflicts.is_empty() { "none".to_string() } else { plan.conflicts.len().to_string() };
        fields.push(("conflicts", Cell::new(conflicts)));
        summary
            .extend(plan.conflicts.iter().take(5).map(|p| format!("exists: {}", sanitize(&p.display().to_string()))));
    }
    let next = match reply.outcome {
        GetOutcome::Planned if reply.plan.as_ref().is_some_and(|p| !p.conflicts.is_empty()) => {
            Some(rerun_get(&id, &job.selection, Some("<EMPTY DIR>".into())))
        }
        GetOutcome::Planned => {
            Some(rerun_get(&id, &job.selection, dir.map(|d| format::shell_word(&sanitize(&d.display().to_string())))))
        }
        _ if job.status.is_stopped() => next_for_stopped(&job.status, &id),
        _ => Some(format!("newsreel wait {id}")),
    };
    Doc { body: Body::Fields { title, fields }, summary, next }
}

/// A download a title play started, as `get` shows one, under the title's name.
pub fn get_title(reply: &GetReply, name: &str) -> Doc {
    let mut doc = get(reply, None);
    if let Body::Fields { fields, .. } = &mut doc.body {
        fields.insert(0, ("title", Cell::new(name)));
    }
    doc
}
