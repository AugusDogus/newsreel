use clap_complete::engine::CompletionCandidate;
use newsreel_core::Job;
use newsreel_core::protocol::Request;

use crate::cli::Shell;
use crate::client::connect;
use crate::client::rows::short_ids;
use newsreel_daemon::env::Env;
use newsreel_daemon::paths::Paths;

pub fn complete_download_ids() -> Vec<CompletionCandidate> {
    let env = Env::from_process();
    let Ok(paths) = Paths::from_process(&env) else { return Vec::new() };
    let Ok(Some(mut conn)) = connect::existing(&paths, &env) else { return Vec::new() };
    let Ok(jobs) = conn.ask::<Vec<Job>>(&Request::Status { refs: Vec::new() }) else { return Vec::new() };
    let ids = short_ids(&jobs.iter().map(|j| j.id).collect::<Vec<_>>());
    jobs.iter()
        .zip(ids)
        .map(|(job, id)| {
            let help = newsreel_core::text::sanitize(&job.name);
            CompletionCandidate::new(id).help(Some(help.into()))
        })
        .collect()
}

pub struct Setup {
    pub shell: &'static str,
    pub rc_file: &'static str,
    pub line: &'static str,
}

pub fn setup(shell: Shell) -> Setup {
    match shell {
        Shell::Bash => Setup { shell: "bash", rc_file: "~/.bashrc", line: "source <(COMPLETE=bash newsreel)" },
        Shell::Zsh => Setup { shell: "zsh", rc_file: "~/.zshrc", line: "source <(COMPLETE=zsh newsreel)" },
        Shell::Fish => {
            Setup { shell: "fish", rc_file: "~/.config/fish/config.fish", line: "COMPLETE=fish newsreel | source" }
        }
        Shell::Elvish => Setup {
            shell: "elvish",
            rc_file: "~/.config/elvish/rc.elv",
            line: "eval (E:COMPLETE=elvish newsreel | slurp)",
        },
        Shell::Powershell => Setup {
            shell: "powershell",
            rc_file: "$PROFILE",
            line: "$env:COMPLETE = \"powershell\"; newsreel | Out-String | Invoke-Expression; Remove-Item Env:\\COMPLETE",
        },
    }
}

pub fn script(setup: &Setup) -> String {
    format!("# add this line to {}\n{}\n", setup.rc_file, setup.line)
}
