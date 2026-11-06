//! Waiting for the operating system to ask this process to stop.
//!
//! Unix delivers a signal. Windows delivers a console control event, and a session ending or a
//! window closing is delivered the same way. Both arrive here as one value so the Node and the
//! terminal dashboard shut down through the same path.

use std::io;

/// Why the operating system asked this process to stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// A person interrupted it from the keyboard.
    Interrupted,
    /// Something asked it to end: a terminate signal, a closed console, or a session ending.
    Requested,
}

/// Resolves once the operating system asks this process to stop.
pub async fn requested() -> io::Result<Stop> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        Ok(tokio::select! {
            _ = tokio::signal::ctrl_c() => Stop::Interrupted,
            _ = terminate.recv() => Stop::Requested,
            _ = hangup.recv() => Stop::Requested,
        })
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};
        let mut interrupt_break = ctrl_break()?;
        let mut close = ctrl_close()?;
        let mut logoff = ctrl_logoff()?;
        let mut shutdown = ctrl_shutdown()?;
        Ok(tokio::select! {
            _ = tokio::signal::ctrl_c() => Stop::Interrupted,
            _ = interrupt_break.recv() => Stop::Interrupted,
            _ = close.recv() => Stop::Requested,
            _ = logoff.recv() => Stop::Requested,
            _ = shutdown.recv() => Stop::Requested,
        })
    }
}

/// Ends this process the way the operating system's own stop would have, so the shell that
/// started it reports an interruption rather than an ordinary failure.
pub fn exit_as_stopped(stop: Stop) -> ! {
    #[cfg(unix)]
    {
        let signal = match stop {
            Stop::Interrupted => libc::SIGINT,
            Stop::Requested => libc::SIGTERM,
        };
        // SAFETY: restoring the default action and raising it on this process is what the
        // operating system would have done without a handler installed.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        // The default action ended the process; reaching here means it was blocked elsewhere.
        std::process::exit(128 + signal);
    }
    #[cfg(windows)]
    {
        // The status the loader reports for a console program ended by its control handler.
        const STATUS_CONTROL_C_EXIT: i32 = 0xC000_013A_u32 as i32;
        std::process::exit(match stop {
            Stop::Interrupted => STATUS_CONTROL_C_EXIT,
            Stop::Requested => 1,
        });
    }
}
