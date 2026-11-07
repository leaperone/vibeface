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

/// Everything that counts as being asked to stop, registered once.
///
/// A caller with a loop of its own holds one of these for its whole run, so an event that
/// arrives between two turns of the loop is still delivered.
#[cfg(unix)]
pub struct Stops {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(windows)]
pub struct Stops {
    interrupt: tokio::signal::windows::CtrlC,
    interrupt_break: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    logoff: tokio::signal::windows::CtrlLogoff,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

impl Stops {
    /// Registers interest in every stop the operating system can report.
    pub fn listen() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
                hangup: signal(SignalKind::hangup())?,
            })
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{
                ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown,
            };
            Ok(Self {
                interrupt: ctrl_c()?,
                interrupt_break: ctrl_break()?,
                close: ctrl_close()?,
                logoff: ctrl_logoff()?,
                shutdown: ctrl_shutdown()?,
            })
        }
    }

    /// Resolves once the operating system asks this process to stop.
    pub async fn next(&mut self) -> Stop {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Stop::Interrupted,
                _ = self.terminate.recv() => Stop::Requested,
                _ = self.hangup.recv() => Stop::Requested,
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Stop::Interrupted,
                _ = self.interrupt_break.recv() => Stop::Interrupted,
                _ = self.close.recv() => Stop::Requested,
                _ = self.logoff.recv() => Stop::Requested,
                _ = self.shutdown.recv() => Stop::Requested,
            }
        }
    }
}

/// Resolves once the operating system asks this process to stop.
pub async fn requested() -> io::Result<Stop> {
    Ok(Stops::listen()?.next().await)
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
