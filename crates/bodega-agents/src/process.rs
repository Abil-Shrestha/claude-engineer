//! Spawning agent processes so they (and everything they start) can be
//! stopped reliably.

use std::time::Duration;

use tokio::process::Command;
use tokio::sync::watch;

/// A command that runs in its own process group and dies with its handle.
pub(crate) fn command(program: &std::ffi::OsStr) -> Command {
    let mut cmd = Command::new(program);
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Waits for `exited` to become true for up to `grace`, then stops the
/// process group led by `pid`: SIGTERM, a short wait, then SIGKILL.
pub(crate) async fn stop_group(
    pid: Option<u32>,
    mut exited: watch::Receiver<bool>,
    grace: Duration,
) {
    let wait_exit = |mut rx: watch::Receiver<bool>, limit: Duration| async move {
        tokio::time::timeout(limit, rx.wait_for(|done| *done))
            .await
            .is_ok()
    };
    if wait_exit(exited.clone(), grace).await {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = pid {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let group = Pid::from_raw(pid as i32);
        let _ = killpg(group, Signal::SIGTERM);
        if wait_exit(exited.clone(), Duration::from_secs(2)).await {
            return;
        }
        let _ = killpg(group, Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = tokio::time::timeout(Duration::from_secs(2), exited.wait_for(|done| *done)).await;
}
