//! The local wall clock, used only for the names a person reads on set-aside data directories.

/// The current local time as `YYYYMMDD-HHMMSS`.
pub fn local_stamp() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 32];
        // SAFETY: `localtime_r` fills a zeroed `tm` this frame owns, and `strftime` writes at
        // most `buffer.len()` bytes into a buffer of that length.
        let written = unsafe {
            let now = libc::time(std::ptr::null_mut());
            let mut parts: libc::tm = std::mem::zeroed();
            libc::localtime_r(&now, &mut parts);
            libc::strftime(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                c"%Y%m%d-%H%M%S".as_ptr(),
                &parts,
            )
        };
        String::from_utf8_lossy(&buffer[..written]).into_owned()
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        // SAFETY: the structure is zeroed and `GetLocalTime` only writes into it.
        let mut parts = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe { GetLocalTime(&mut parts) };
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            parts.wYear, parts.wMonth, parts.wDay, parts.wHour, parts.wMinute, parts.wSecond
        )
    }
}
