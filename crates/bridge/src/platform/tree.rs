//! Starting a provider so its whole process tree can be ended, and ending a tree an earlier run
//! of this Node left behind.
//!
//! Unix puts the child in its own process group and signals the group. Windows puts the child in
//! a named job object which the kernel tears down when the last handle to it closes, so a crash
//! of this Node cannot leave the tree running. In both cases the record a journal keeps is only
//! acted on after the identity behind it is confirmed: a bare process id is never terminated,
//! because Windows reuses process ids and an unrelated program must never be killed.

use anyhow::{Context, Result, ensure};
use std::{path::Path, time::Duration};

/// How long a tree gets to exit after being asked, before it is ended outright.
const GRACE: Duration = Duration::from_secs(3);

/// A started process tree, kept for as long as the Node manages it.
pub struct Tree {
    /// The record a journal stores so a later run can find the same tree.
    token: String,
    /// The process at the root of the tree, reported where a coordinator records which process
    /// it started. Only ever compared or asked about, never signalled on its own.
    root: u32,
    #[cfg(unix)]
    group: i32,
    #[cfg(windows)]
    job: Option<super::winjob::Job>,
}

impl Tree {
    /// Starts `command` as the root of its own tree.
    ///
    /// `key` names the tree in the journal and, on Windows, in the job object namespace of this
    /// account, so one Node's App Server is never confused with another's.
    pub fn spawn(
        mut command: std::process::Command,
        key: &str,
        kill_on_drop: bool,
    ) -> Result<(Self, tokio::process::Child)> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            let _ = key;
            command.process_group(0);
            let child = tokio::process::Command::from(command)
                .kill_on_drop(kill_on_drop)
                .spawn()?;
            let group = child.id().context("the program exited at once")? as i32;
            Ok((
                Self {
                    token: group.to_string(),
                    root: group as u32,
                    group,
                },
                child,
            ))
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            use windows_sys::Win32::System::Threading::{
                CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED,
            };
            let job = super::winjob::Job::create(key)?;
            // The child starts suspended so it joins the job before it can start a program of
            // its own; nothing escapes the tree between the start and the assignment.
            command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
            let mut child = tokio::process::Command::from(command)
                .kill_on_drop(kill_on_drop)
                .spawn()?;
            let id = child.id().context("the program exited at once")?;
            let started = job
                .adopt(&child)
                .and_then(|()| job.token(id))
                .and_then(|token| super::winjob::resume_process(id).map(|()| token));
            match started {
                Ok(token) => Ok((
                    Self {
                        token,
                        root: id,
                        job: Some(job),
                    },
                    child,
                )),
                Err(error) => {
                    // The child is still suspended, so ending the job ends it before it runs.
                    job.terminate();
                    let _ = child.start_kill();
                    Err(error).context("start the program inside its own job")
                }
            }
        }
    }

    /// The journal record for this tree. Writing it lets a later run of this Node find and end
    /// the same tree after a crash.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The process at the root of the tree.
    pub fn root(&self) -> u32 {
        self.root
    }

    /// Whether any member of the tree is still running.
    pub fn is_running(&self) -> bool {
        #[cfg(unix)]
        {
            signal_group(self.group, 0)
        }
        #[cfg(windows)]
        {
            self.job
                .as_ref()
                .is_some_and(|job| job.member_count().unwrap_or(0) > 0)
        }
    }

    /// Ends the tree outright, used when the root has already exited and only the programs it
    /// started are left.
    pub fn kill(&mut self) {
        #[cfg(unix)]
        {
            signal_group(self.group, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            if let Some(job) = self.job.take() {
                job.terminate();
            }
        }
    }

    /// Asks the tree to stop and returns at once, for a caller that awaits the root itself and
    /// ends what is left with [`Tree::kill`].
    pub fn ask_to_stop(&self) {
        #[cfg(unix)]
        {
            signal_group(self.group, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            if let Some(job) = self.job.as_ref() {
                job.terminate();
            }
        }
    }

    /// Asks the tree to stop, then ends it if it outlives the grace period.
    ///
    /// Blocks for at most [`GRACE`]; it runs only while the Node is starting or shutting down.
    pub fn stop(&mut self) {
        #[cfg(unix)]
        {
            if !signal_group(self.group, libc::SIGTERM) {
                return;
            }
            for _ in 0..30 {
                if !signal_group(self.group, 0) {
                    return;
                }
                std::thread::sleep(GRACE / 30);
            }
            signal_group(self.group, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let Some(job) = self.job.take() else {
                return;
            };
            // The job handle bounds termination to this tree; console events are not job-scoped.
            job.terminate();
            for _ in 0..30 {
                if job.member_count().unwrap_or(0) == 0 {
                    return;
                }
                std::thread::sleep(GRACE / 30);
            }
            job.terminate();
        }
    }
}

/// A tree an earlier run recorded, found again from its journal record.
pub struct Leftover {
    #[cfg(unix)]
    group: i32,
    #[cfg(windows)]
    job: super::winjob::Job,
    #[cfg(windows)]
    recorded: super::winjob::Record,
}

impl Leftover {
    /// Looks up the tree `token` describes. `Ok(None)` means nothing of it is left, which is the
    /// normal case after a clean exit.
    pub fn find(token: &str) -> Result<Option<Self>> {
        #[cfg(unix)]
        {
            let group = token
                .trim()
                .parse::<i32>()
                .ok()
                .filter(|group| *group > 1)
                .context("invalid process-tree record")?;
            if !signal_group(group, 0) {
                return Ok(None);
            }
            Ok(Some(Self { group }))
        }
        #[cfg(windows)]
        {
            let recorded: super::winjob::Record = token.trim().parse()?;
            match super::winjob::Job::open(recorded.key())? {
                Some(job) => Ok(Some(Self { job, recorded })),
                None => Ok(None),
            }
        }
    }

    /// The programs still running in the tree, each as the path of its executable.
    ///
    /// On Unix this is the command line of the group leader, which is what the Node matched
    /// before this module existed. On Windows it is the image path of every member, read through
    /// a handle to the process the job itself named, so no process id is ever guessed.
    pub fn members(&self) -> Vec<String> {
        #[cfg(unix)]
        {
            std::process::Command::new("/bin/ps")
                .args(["-o", "command=", "-p", &self.group.to_string()])
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .map(|line| vec![line])
                .unwrap_or_default()
        }
        #[cfg(windows)]
        {
            self.job.member_images()
        }
    }

    /// Whether the record this was found from still describes the same program.
    ///
    /// Windows reuses process ids, so the record carries the moment the root started and the
    /// path it was started from; both must still match before anything is ended.
    pub fn matches_record(&self) -> bool {
        #[cfg(unix)]
        {
            true
        }
        #[cfg(windows)]
        {
            self.job.holds_recorded_root(&self.recorded)
        }
    }

    /// Ends the tree. Only call this once [`Leftover::members`] has confirmed it is the Node's
    /// own program.
    pub fn stop(self) {
        #[cfg(unix)]
        {
            let mut tree = Tree {
                token: self.group.to_string(),
                root: self.group as u32,
                group: self.group,
            };
            tree.stop();
        }
        #[cfg(windows)]
        {
            self.job.terminate();
            for _ in 0..30 {
                if self.job.member_count().unwrap_or(0) == 0 {
                    return;
                }
                std::thread::sleep(GRACE / 30);
            }
            self.job.terminate();
        }
    }
}

#[cfg(unix)]
fn signal_group(group: i32, signal: i32) -> bool {
    if group <= 1 {
        return false;
    }
    // SAFETY: kill(2) with a negative pid signals one process group and touches no memory.
    let result = unsafe { libc::kill(-group, signal) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Whether the process the scheduler recorded as the root of a coordination tree could still be
/// running. Nothing is signalled or ended here.
///
/// The answer leans towards "still running": a reused identifier, or a process this account may
/// not query, reads as alive. Every caller uses this to decide whether to refuse an action, so
/// leaning that way keeps a stale record from closing out work that is still going.
pub fn root_is_running(process: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(group) = libc::pid_t::try_from(process) else {
            return false;
        };
        // SAFETY: kill with signal 0 performs only the existence and permission check.
        let result = unsafe { libc::kill(-group, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        super::winjob::process_is_running(process)
    }
}

/// Whether a tree recorded under `token` could still be running, used where the Node only needs
/// to know whether to wait rather than to end anything.
pub fn token_is_running(token: &str) -> bool {
    Leftover::find(token).map_or(true, |found| found.is_some())
}

/// Ends the tree `token` names when it is still running and at least one program in it is one
/// the Node started. `accept` sees each member and answers whether it is the Node's own program.
///
/// `Ok(None)` means nothing was ended, either because nothing is left or because what is left
/// is not the recorded program. An error means the record could not be trusted, which is left
/// for the caller to report rather than acted on.
pub fn stop_recorded(token: &str, accept: impl Fn(&str) -> bool) -> Result<Option<Vec<String>>> {
    let Some(leftover) = Leftover::find(token)? else {
        return Ok(None);
    };
    ensure!(
        leftover.matches_record(),
        "the recorded program is gone and its identifier now belongs to something else"
    );
    let members = leftover.members();
    if members.is_empty() || !members.iter().any(|member| accept(member)) {
        return Ok(None);
    }
    leftover.stop();
    Ok(Some(members))
}

/// Whether `path` names the program the Node expects a leftover tree to be running.
pub fn names_program(member: &str, program: &Path) -> bool {
    let Some(name) = program.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    member.contains(name.as_ref())
}
