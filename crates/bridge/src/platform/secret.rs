//! The two ways a passphrase reaches this process: typed at the console, or handed over on a
//! channel the caller already opened.
//!
//! Neither way lets the secret reach a file, an argument list, or the environment. The console
//! read turns echo off for as long as it lasts and turns it back on afterwards, on both
//! families. A handed-over channel is refused unless only this account can read or write it:
//! Unix names it by descriptor number and checks the owning uid and mode, Windows names it by
//! handle value, checks a file's access control list, and checks that a pipe's connected peer
//! belongs to this account.

use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{self, Read, Write},
};

/// The console a person types a secret into, with echo off for as long as this is held.
pub struct Console {
    file: File,
    restore: Restore,
}

#[cfg(unix)]
struct Restore {
    descriptor: i32,
    previous: libc::termios,
}

#[cfg(windows)]
struct Restore {
    handle: std::os::windows::io::RawHandle,
    previous: u32,
}

impl Console {
    /// Opens the console of this session and turns echo off.
    ///
    /// Fails when the process has no console, which is what tells the caller to ask for the
    /// passphrase on a handed-over channel instead.
    pub fn open() -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .context("this command needs a terminal")?;
            let descriptor = file.as_raw_fd();
            // SAFETY: the settings buffer is a live local and the descriptor is open.
            let mut previous = std::mem::MaybeUninit::<libc::termios>::uninit();
            // SAFETY: as above.
            ensure!(
                unsafe { libc::tcgetattr(descriptor, previous.as_mut_ptr()) } == 0,
                "the terminal settings are unavailable"
            );
            // SAFETY: tcgetattr reported success, so the buffer is initialised.
            let previous = unsafe { previous.assume_init() };
            let mut hidden = previous;
            hidden.c_lflag &= !(libc::ECHO | libc::ECHONL);
            // SAFETY: the settings come from this very terminal and the descriptor is open.
            ensure!(
                unsafe { libc::tcsetattr(descriptor, libc::TCSAFLUSH, &hidden) } == 0,
                "the terminal cannot turn echo off"
            );
            Ok(Self {
                file,
                restore: Restore {
                    descriptor,
                    previous,
                },
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::System::Console::{
                ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, GetConsoleMode, SetConsoleMode,
            };
            // `CONIN$` is this session's console input however the standard streams are
            // redirected, which is what makes a prompt possible from inside a pipeline.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("CONIN$")
                .context("this command needs a console")?;
            let handle = file.as_raw_handle();
            let mut previous = 0_u32;
            // SAFETY: the handle is open and the mode is a live local.
            ensure!(
                unsafe { GetConsoleMode(handle.cast(), &mut previous) } != 0,
                "the console settings are unavailable"
            );
            // Line input is dropped with echo so each keystroke arrives as it is typed, which
            // is what the Unix side gets from turning off canonical echo.
            let hidden = previous & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT);
            // SAFETY: as above.
            ensure!(
                unsafe { SetConsoleMode(handle.cast(), hidden) } != 0,
                "the console cannot turn echo off"
            );
            Ok(Self {
                file,
                restore: Restore { handle, previous },
            })
        }
    }

    /// Writes `text` where the person typing can read it.
    pub fn prompt(&mut self, text: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.file.write_all(text.as_bytes())?;
            self.file.flush()
        }
        #[cfg(windows)]
        {
            // Console input cannot be written to, so the prompt goes to the console's own
            // output rather than to a standard stream the caller may have redirected.
            let mut output = std::fs::OpenOptions::new().write(true).open("CONOUT$")?;
            output.write_all(text.as_bytes())?;
            output.flush()
        }
    }

    /// Reads the next byte the person typed.
    pub fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0_u8; 1];
        match self.file.read(&mut byte)? {
            0 => Ok(None),
            _ => Ok(Some(byte[0])),
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: the settings were read from this descriptor, which is still open.
        unsafe {
            libc::tcsetattr(
                self.restore.descriptor,
                libc::TCSAFLUSH,
                &self.restore.previous,
            );
        }
        #[cfg(windows)]
        // SAFETY: the mode was read from this handle, which is still open.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(
                self.restore.handle.cast(),
                self.restore.previous,
            );
        }
    }
}

/// Opens the channel a caller handed over, refusing one another account could read or write.
///
/// `reference` is a descriptor number on Unix and a handle value on Windows. Either way the
/// caller opened it, so the secret never passes through a file the Node creates.
pub fn handed_over(reference: i64) -> Result<File> {
    ensure!(reference >= 0, "the handed-over channel is invalid");
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd as _;
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        let descriptor = i32::try_from(reference)
            .ok()
            .context("the handed-over channel is invalid")?;
        // The duplicate is moved above the standard streams and closes on exec, so nothing the
        // Node starts inherits the channel the secret arrives on.
        // SAFETY: fcntl only duplicates a descriptor and reports failure in its return value.
        let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
        ensure!(duplicate >= 0, "the handed-over channel cannot be read");
        // SAFETY: the duplicate is a fresh descriptor this File now owns alone.
        let file = unsafe { File::from_raw_fd(duplicate) };
        let metadata = file.metadata()?;
        // SAFETY: geteuid reads the calling process and touches no memory.
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && (metadata.is_file() || metadata.file_type().is_fifo()),
            "the handed-over channel must be an owner-only regular file or pipe"
        );
        Ok(file)
    }
    #[cfg(windows)]
    {
        super::winsec::handed_over(reference as usize).map_err(Into::into)
    }
}
