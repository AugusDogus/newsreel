//! The engine's log: a line on stderr for each event, which launchd and systemd keep in a file.

/// Writes `message` to the log after the time.
pub fn log(message: &str) {
    eprintln!("{} {message}", newsreel_core::units::timestamp(newsreel_core::time::now()));
}

/// Whether the log goes to the file at `path`: stderr is that file, as the login service and an
/// engine a command starts set it up. `newsreel engine run` logs to wherever it was started: a
/// terminal or console, or a file it was sent to.
pub fn writes_to(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    let stderr = std::os::fd::AsFd::as_fd(&std::io::stderr()).try_clone_to_owned();
    #[cfg(windows)]
    let stderr = std::os::windows::io::AsHandle::as_handle(&std::io::stderr()).try_clone_to_owned();
    let Ok(stderr) = stderr else { return false };
    let stderr = std::fs::File::from(stderr);
    matches!(
        (crate::folder::file_identity(&stderr), crate::folder::identity(path)),
        (Ok(stderr), Ok(file)) if stderr == file
    )
}

/// The one policy for a write the engine can go on without: log what failed, and go on from what
/// is in memory. Says whether it worked. `what` reads after "cannot", such as "save the progress".
pub fn persist(what: &str, result: Result<(), impl std::fmt::Display>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            log(&format!("cannot {what}: {error}"));
            false
        }
    }
}
