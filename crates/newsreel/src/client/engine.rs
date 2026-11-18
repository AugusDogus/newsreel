//! `newsreel engine`: whether the engine runs, stopping it, and its login service.

use newsreel_core::Job;
use newsreel_core::protocol::Request;
use newsreel_core::{Error, ErrorCode};
use serde::Serialize;

use super::remote::Remotes;
use super::{connect, emit, launch};
use crate::cli::DaemonCommand;
use crate::output::theme::Token;
use crate::output::{self, Body, Cell, Doc, DocKind, Mode, Typed};
use newsreel_daemon::env::Env;
use newsreel_daemon::paths::Paths;
use newsreel_daemon::service;

pub fn run(command: DaemonCommand, env: &Env, mode: Mode) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    match command {
        DaemonCommand::Status => {
            let chosen = Remotes::using(&paths)?;
            let found = match &chosen {
                Some(chosen) => Some(connect::remote(chosen, env)?),
                None => connect::running(&paths, env)?,
            };
            let local = chosen.is_none();
            let log = local.then(|| paths.log().display().to_string());
            // The login service starts this machine's engine, so it says nothing of one on another.
            let service_binary = if local { service::starts_another(env, &paths) } else { None };
            let this_version = env.version();
            let report = match found {
                Some(mut conn) => {
                    let jobs = conn.ask::<Vec<Job>>(&Request::Status { refs: Vec::new() });
                    let downloads = jobs.ok().map(|jobs| jobs.len());
                    let version = local
                        .then(|| {
                            connect::mismatch(Some(&conn.engine_version), service_binary.as_deref(), &this_version)
                        })
                        .flatten();
                    // The engine writes it while changes to config.yaml wait for a restart.
                    let restart = local.then(|| std::fs::read_to_string(paths.restart_needed()).ok()).flatten();
                    let notice = match (version, restart) {
                        (Some(version), Some(restart)) => Some(format!("{version}; {restart}")),
                        (version, restart) => version.or(restart),
                    };
                    DaemonReport {
                        engine: "running",
                        pid: Some(conn.pid),
                        notice,
                        version: Some(conn.engine_version.clone()),
                        address: Some(conn.engine.address().to_string()),
                        device: chosen.map(|chosen| chosen.device),
                        log,
                        downloads,
                        this_version,
                        service_binary,
                    }
                }
                None => DaemonReport {
                    engine: "stopped",
                    pid: None,
                    version: None,
                    address: None,
                    device: None,
                    log,
                    downloads: None,
                    notice: connect::mismatch(None, service_binary.as_deref(), &this_version),
                    this_version,
                    service_binary,
                },
            };
            Ok(emit(mode, &daemon_status(&report), &Typed { kind: DocKind::Daemon, body: &report }))
        }
        DaemonCommand::Stop => {
            let pid = connect::stop_asked(&paths, env)?;
            let result = if pid.is_some() { Stop::Stopped } else { Stop::AlreadyStopped };
            #[derive(Serialize)]
            struct Stopped {
                result: Stop,
                pid: Option<u32>,
            }
            let mut fields = vec![("result", output::Cell::new(result.as_str()))];
            if let Some(pid) = pid {
                fields.push(("pid", output::Cell::new(pid.to_string())));
            }
            let doc = output::Doc {
                body: output::Body::Fields { title: Vec::new(), fields },
                summary: Vec::new(),
                next: None,
            };
            Ok(emit(mode, &doc, &Typed { kind: DocKind::DaemonStop, body: Stopped { result, pid } }))
        }
        DaemonCommand::Install => {
            // The service starts its own engine, so the one running now stops first, and starts
            // again if the service could not be installed.
            let stopped = match connect::running(&paths, env)? {
                Some(conn) => connect::stop(conn, connect::Stop::Asked).map(|_| true)?,
                None => false,
            };
            // Whether an engine runs after a failed install, which the hint says with the rest.
            let hinted = |e: Error, said: &dyn Fn(&str) -> String| Error {
                hint: Some(said(e.hint.as_deref().unwrap_or_default())),
                ..e
            };
            match service::install(env, &paths) {
                Err(e) if stopped => Err(match launch::start(&paths, env) {
                    Ok(_) => hinted(e, &|hint| format!("the engine it stopped runs again; {hint}")),
                    Err(start) => Error {
                        message: format!("{}; the engine it stopped did not start again: {}", e.message, start.message),
                        ..e
                    },
                }),
                Err(e) if e.code == ErrorCode::ServiceUnavailable => {
                    Err(hinted(e, &|hint| format!("{hint}; newsreel status starts the engine meanwhile")))
                }
                installed => service_report(installed?, mode),
            }
        }
        DaemonCommand::Uninstall => {
            let report = service::uninstall(env, &paths)?;
            // Task Scheduler leaves running the engine its task started.
            if matches!(
                (report.manager, report.result),
                (service::Manager::TaskScheduler, service::Change::Uninstalled)
            ) {
                connect::stop_asked(&paths, env)?;
            }
            service_report(report, mode)
        }
        DaemonCommand::Run { .. } => unreachable!("main runs the engine itself"),
        DaemonCommand::Launch => Ok(launch::launch_stage(env)),
        DaemonCommand::Interrupt { .. } => unreachable!("main sends the interrupt itself"),
    }
}

newsreel_core::words! {
    enum Stop {
        Stopped = "stopped",
        AlreadyStopped = "already_stopped",
    }
}

fn service_report(report: service::Report, mode: Mode) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Payload {
        result: service::Change,
        manager: &'static str,
        unit: String,
    }
    let unit = report.unit.display().to_string();
    let fields = vec![
        ("result", output::Cell::new(report.result.as_str())),
        ("manager", output::Cell::new(report.manager.name())),
        ("unit", output::Cell::new(unit.clone())),
    ];
    let next = matches!(report.result, service::Change::Installed | service::Change::AlreadyInstalled)
        .then(|| "newsreel engine status".to_string());
    let doc = output::Doc { body: output::Body::Fields { title: Vec::new(), fields }, summary: Vec::new(), next };
    let payload = Payload { result: report.result, manager: report.manager.name(), unit };
    Ok(emit(mode, &doc, &Typed { kind: DocKind::DaemonService, body: payload }))
}

#[derive(Clone, PartialEq, Debug, serde::Serialize)]
pub struct DaemonReport {
    pub engine: &'static str,
    pub pid: Option<u32>,
    pub version: Option<String>,
    /// Where the engine serves the web app and this newsreel's requests.
    pub address: Option<String>,
    /// This machine's device ID on the engine on another machine that `newsreel connect` chose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The log of this machine's engine. An engine on another machine keeps its own there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    pub downloads: Option<usize>,
    /// The version of this newsreel, which the engine should run.
    pub this_version: String,
    /// The binary the login service starts, when it is not this newsreel.
    pub service_binary: Option<String>,
    /// Why the engine may not run this newsreel, and how to make it, in plain words.
    pub notice: Option<String>,
}

pub fn daemon_status(report: &DaemonReport) -> Doc {
    let running = report.pid.is_some();
    let mut fields = vec![("engine", Cell::styled(report.engine, if running { Token::Mint } else { Token::Muted }))];
    if let Some(pid) = report.pid {
        fields.push(("pid", Cell::new(pid.to_string())));
    }
    if let Some(version) = &report.version {
        fields.push(("version", Cell::new(version.clone())));
    }
    if let Some(downloads) = report.downloads {
        fields.push(("downloads", Cell::new(downloads.to_string())));
    }
    if let Some(address) = &report.address {
        fields.push(("address", Cell::new(address.clone())));
    }
    let mut summary: Vec<String> = report.notice.iter().cloned().collect();
    if let Some(device) = &report.device {
        fields.push(("device", Cell::new(device.clone())));
        summary.push("an engine on another machine; newsreel connect --local drives this machine's".to_string());
    }
    if let Some(service) = &report.service_binary {
        fields.push(("service", Cell::new(service.clone())));
    }
    if let Some(log) = &report.log {
        fields.push(("log", Cell::new(log.clone())));
    }
    let next = if report.service_binary.is_some() { "newsreel engine install" } else { "newsreel status" };
    Doc { body: Body::Fields { title: Vec::new(), fields }, summary, next: Some(next.to_string()) }
}
