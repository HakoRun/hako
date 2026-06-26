//! Signal a process by host pid — the kernel side of the CLI's
//! `/containers/<name>/proc/<pid>/ctl` control node.
//!
//! Security note: this signals whatever process currently holds `pid`. The
//! caller is responsible for verifying that `pid` belongs to the target
//! container (PID-namespace scoping) immediately before calling, so a recycled
//! or out-of-container pid is never signaled.

use crate::RuntimeError;

/// Send signal number `sig` to `pid`.
#[cfg(target_os = "linux")]
pub fn signal(pid: u32, sig: i32) -> Result<(), RuntimeError> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let signal =
        Signal::try_from(sig).map_err(|_| RuntimeError::Other(format!("invalid signal {sig}")))?;
    kill(Pid::from_raw(pid as i32), signal)
        .map_err(|e| RuntimeError::Other(format!("signal pid {pid}: {e}")))
}

#[cfg(not(target_os = "linux"))]
pub fn signal(_pid: u32, _sig: i32) -> Result<(), RuntimeError> {
    Err(RuntimeError::UnsupportedPlatform {
        operation: "signal a container process",
        hint: "Signalling a process happens on the Linux runtime host. \
               On Windows/macOS, manage processes from inside the WSL2 distro / Lima VM.",
    })
}
