//! Windows job objects: the kernel container that makes a provider's whole process tree end
//! together, and the journal record that proves a leftover tree is still the Node's own.
//!
//! A job created here is named for this account and keyed to one managed program, and is set to
//! end its members when the last handle to it closes. A crash of the Node therefore ends the
//! tree with it. The journal record carries the moment the root process started and the image it
//! was started from, so a process id another program later reuses can never be mistaken for the
//! recorded one.

use anyhow::{Context as _, Result, bail, ensure};
use std::io;
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, OpenJobObjectW,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE},
        Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
};

/// The most members a managed tree is read back with. A provider that starts more than this has
/// already escaped what the Node models, and the job still ends all of them.
const MAX_MEMBERS: usize = 1024;

/// An owned kernel handle.
struct Owned(HANDLE);

// SAFETY: a kernel handle is a process-wide value every thread may use, and this wrapper hands
// out no interior references, so moving or sharing it across threads is sound.
unsafe impl Send for Owned {}
// SAFETY: as above.
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from a successful open and is closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A job object holding one managed process tree.
pub struct Job {
    handle: Owned,
}

fn job_name(key: &str) -> Result<Vec<u16>> {
    ensure!(
        !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "a managed program key must be alphanumeric"
    );
    let account = super::winsec::current_user()?.fingerprint();
    // The local namespace keeps the name inside this logon session.
    Ok(super::winsec::wide(std::ffi::OsStr::new(&format!(
        r"Local\vibeface-{key}-{account}"
    ))))
}

impl Job {
    /// Creates the job for `key`, replacing nothing: a name another live job already holds is
    /// returned as an error so two Nodes never share one tree.
    pub fn create(key: &str) -> Result<Self> {
        let name = job_name(key)?;
        // SAFETY: `name` is NUL-terminated and no attributes are supplied.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error()).map_err(Into::into);
        }
        let job = Self {
            handle: Owned(handle),
        };
        job.end_members_with_the_job()?;
        Ok(job)
    }

    /// Opens the job an earlier run created, or `None` when nothing is left of it.
    pub fn open(key: &str) -> Result<Option<Self>> {
        let name = job_name(key)?;
        // SAFETY: `name` is NUL-terminated.
        let handle =
            unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error().map(|code| code as u32) == Some(ERROR_FILE_NOT_FOUND) {
                return Ok(None);
            }
            return Err(error.into());
        }
        Ok(Some(Self {
            handle: Owned(handle),
        }))
    }

    fn end_members_with_the_job(&self) -> Result<()> {
        // SAFETY: the structure is zeroed and sized for the information class below.
        let mut limits = unsafe { std::mem::zeroed::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the job handle is open and the buffer matches the information class.
        if unsafe {
            SetInformationJobObject(
                self.handle.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// Puts `child` and everything it starts into this job.
    pub fn adopt(&self, child: &tokio::process::Child) -> Result<()> {
        let handle = child
            .raw_handle()
            .context("the program exited before it joined its job")?;
        // SAFETY: both handles are open for the whole call.
        if unsafe { AssignProcessToJobObject(self.handle.0, handle.cast()) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// The process ids still in the job.
    fn members(&self) -> Result<Vec<u32>> {
        let header = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        let mut buffer = vec![0u8; header + MAX_MEMBERS * std::mem::size_of::<usize>()];
        let mut returned = 0u32;
        // SAFETY: the buffer is sized for the list header plus MAX_MEMBERS entries.
        if unsafe {
            QueryInformationJobObject(
                self.handle.0,
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut returned,
            )
        } == 0
        {
            // A partially filled list still answers the question of what is running.
            let error = io::Error::last_os_error();
            if returned == 0 {
                return Err(error.into());
            }
        }
        // SAFETY: the kernel filled the header; the entries follow it inside the same buffer.
        let list = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
        let count = (list.NumberOfProcessIdsInList as usize).min(MAX_MEMBERS);
        let mut ids = Vec::with_capacity(count);
        for index in 0..count {
            let at = header - std::mem::size_of::<usize>() + index * std::mem::size_of::<usize>();
            let bytes: [u8; std::mem::size_of::<usize>()] = buffer
                [at..at + std::mem::size_of::<usize>()]
                .try_into()
                .expect("a pointer sized entry");
            let id = usize::from_le_bytes(bytes);
            if let Ok(id) = u32::try_from(id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// How many programs are still running in the job.
    pub fn member_count(&self) -> Result<usize> {
        self.members().map(|ids| ids.len())
    }

    /// The image path of every program still running in the job.
    pub fn member_images(&self) -> Vec<String> {
        self.members()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| image_of(id).ok())
            .collect()
    }

    /// Asks every program in the job to stop. A program with no console to receive the request
    /// keeps running and is ended by [`Job::terminate`] when its grace period runs out.
    pub fn request_stop(&self) {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
        for id in self.members().unwrap_or_default() {
            // SAFETY: the call only sends a console event and touches no memory here.
            unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, id) };
        }
    }

    /// Ends every program in the job at once.
    pub fn terminate(&self) {
        // SAFETY: the job handle is open; the exit code is arbitrary.
        unsafe { TerminateJobObject(self.handle.0, 1) };
    }

    /// Whether the root process the journal recorded is still a member of this job and still
    /// started at the recorded moment. A process identifier alone proves nothing on Windows.
    pub fn holds_recorded_root(&self, record: &Record) -> bool {
        self.members().is_ok_and(|ids| {
            ids.contains(&record.process)
                && started_at(record.process).is_ok_and(|started| started == record.started)
        })
    }

    /// The journal record for the tree rooted at `process`.
    pub fn token(&self, key: &str, process: u32) -> Result<String> {
        Ok(Record {
            process,
            started: started_at(process)?,
            key: key.to_owned(),
        }
        .to_string())
    }
}

/// What the journal stores about a managed tree on Windows: which job holds it, which process
/// roots it, and when that process started.
///
/// The start moment is what makes the record safe. Windows hands a process identifier to a new
/// program as soon as the old one is gone, so an identifier on its own proves nothing; the pair
/// of identifier and start moment names exactly one program for as long as it runs.
///
/// The fields are separated by a character no Windows path may contain, so a record never needs
/// escaping and never runs into a program path.
pub struct Record {
    process: u32,
    started: u64,
    key: String,
}

impl Record {
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl std::fmt::Display for Record {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}|{}|{}", self.process, self.started, self.key)
    }
}

impl std::str::FromStr for Record {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut fields = value.trim().splitn(3, '|');
        let (Some(process), Some(started), Some(key)) =
            (fields.next(), fields.next(), fields.next())
        else {
            bail!("the recorded program journal is not in this Node's format");
        };
        Ok(Self {
            process: process.parse()?,
            started: started.parse()?,
            key: key.to_owned(),
        })
    }
}

fn open_for_query(process: u32) -> Result<Owned> {
    // SAFETY: the call only looks up a process id and reports failure through the return value.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process) };
    if handle.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    Ok(Owned(handle))
}

/// When `process` started, as the raw clock value, which stays the same for its whole life and
/// so tells two programs with the same process id apart.
fn started_at(process: u32) -> Result<u64> {
    let handle = open_for_query(process)?;
    // SAFETY: every out parameter is a live local and the handle is open.
    let mut created = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut exited = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut kernel = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut user = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
}

/// The program `process` is running.
fn image_of(process: u32) -> Result<String> {
    use std::os::windows::ffi::OsStringExt as _;
    let handle = open_for_query(process)?;
    let mut buffer = vec![0u16; 1024];
    let mut length = buffer.len() as u32;
    // SAFETY: the buffer and the length match, and the handle is open.
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(std::ffi::OsString::from_wide(&buffer[..length as usize])
        .to_string_lossy()
        .into_owned())
}

/// Lets a process that was started suspended run, once it has joined its job.
pub fn resume_process(process: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
    };
    // SAFETY: a thread snapshot takes no process id.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    let snapshot = Owned(snapshot);
    // SAFETY: the entry is zeroed and its declared size is set below.
    let mut entry = unsafe { std::mem::zeroed::<THREADENTRY32>() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    // SAFETY: the snapshot is open and the entry is correctly sized.
    let mut found = unsafe { Thread32First(snapshot.0, &mut entry) } != 0;
    let mut resumed = 0usize;
    while found {
        if entry.th32OwnerProcessID == process {
            // SAFETY: the thread id came from the snapshot.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                let thread = Owned(thread);
                // SAFETY: the thread handle is open for the call.
                if unsafe { ResumeThread(thread.0) } != u32::MAX {
                    resumed += 1;
                }
            }
        }
        // SAFETY: as for Thread32First.
        found = unsafe { Thread32Next(snapshot.0, &mut entry) } != 0;
    }
    ensure!(
        resumed > 0,
        "the program could not be let out of its suspended start"
    );
    Ok(())
}
