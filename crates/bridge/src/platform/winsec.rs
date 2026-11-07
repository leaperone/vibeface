//! Windows access control behind the owner-only guarantees of this module.
//!
//! Everything here is expressed against the SID of the token running this process. A path or
//! kernel object counts as private only when its DACL is present, protected, and grants access
//! to no other account; a missing DACL means every account has access and is refused.

use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree},
    Security::{
        ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        Authorization::{
            EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE,
            SE_FILE_OBJECT, SE_KERNEL_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
            TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
        },
        CheckTokenMembership, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetLengthSid, GetTokenInformation, InitializeSecurityDescriptor, NO_INHERITANCE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::FILE_ALL_ACCESS,
    System::{
        SystemServices::{
            ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

/// The access a private object grants its single trustee. Pipes and files both accept the file
/// access mask, so one value covers every object this module creates.
pub const OWNER_ACCESS: u32 = FILE_ALL_ACCESS;

/// Wide, NUL-terminated form of `path` for the `W` entry points.
pub fn wide(path: &OsStr) -> Vec<u16> {
    path.encode_wide().chain(std::iter::once(0)).collect()
}

fn win32_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn checked(code: u32) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(win32_error(code))
    }
}

/// An owned copy of a security identifier.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sid(Vec<u32>);

impl Sid {
    /// Copies the SID at `raw`, which must point at a valid SID owned by the caller.
    unsafe fn copy_from(raw: PSID) -> io::Result<Self> {
        if raw.is_null() {
            return Err(io::Error::other("security identifier is absent"));
        }
        // SAFETY: the caller guarantees `raw` points at a valid SID.
        let length = unsafe { GetLengthSid(raw) } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if !length.is_multiple_of(std::mem::size_of::<u32>()) {
            return Err(io::Error::other(
                "security identifier has an invalid length",
            ));
        }
        let mut words = vec![0_u32; length / std::mem::size_of::<u32>()];
        // SAFETY: SID storage is DWORD-aligned and covers the exact byte length reported above.
        unsafe {
            std::ptr::copy_nonoverlapping(raw.cast::<u8>(), words.as_mut_ptr().cast(), length)
        };
        Ok(Self(words))
    }

    pub fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }

    /// A stable short identifier for this account, used to keep per-user names apart in the
    /// machine-wide named pipe namespace.
    pub fn fingerprint(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.0.iter().flat_map(|word| word.to_le_bytes()) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        format!("{hash:016x}")
    }
}

/// The account this process runs as.
pub fn current_user() -> io::Result<Sid> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: both arguments are valid for the duration of the call.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    token_user(OwnedHandle(token).0)
}

/// Whether the token running this process holds `sid`, so an object owned by `sid` is owned by
/// this account even when the owner is a group such as Administrators under elevation.
pub fn token_holds(sid: &Sid) -> io::Result<bool> {
    let mut member = 0;
    // SAFETY: a null token handle asks about the calling thread's effective token.
    if unsafe { CheckTokenMembership(std::ptr::null_mut(), sid.as_psid(), &mut member) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(member != 0)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from a successful open and is closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// An access control list allocated by `SetEntriesInAclW`.
struct LocalAcl(*mut ACL);

impl Drop for LocalAcl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the list came from SetEntriesInAclW and is freed exactly once.
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }
}

/// A security descriptor granting the current account full access and no other account any
/// access, with inheritance blocked so a permissive parent cannot widen it.
pub struct OwnerOnly {
    // SECURITY_DESCRIPTOR stores this SID by pointer; it must outlive kernel object creation.
    _owner: Sid,
    acl: LocalAcl,
    descriptor: Box<SECURITY_DESCRIPTOR>,
}

impl OwnerOnly {
    /// `container` marks a directory, whose single entry is inherited by the files created in
    /// it so they start private as well.
    pub fn new(container: bool) -> io::Result<Self> {
        let user = current_user()?;
        Self::for_user(user, container)
    }

    fn for_user(user: Sid, container: bool) -> io::Result<Self> {
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: OWNER_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if container {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                NO_INHERITANCE
            },
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: user.as_psid().cast(),
            },
        };
        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: one entry is described by `entry`, and no prior list is merged in.
        checked(unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut acl) })?;
        let acl = LocalAcl(acl);
        let mut descriptor = Box::new(unsafe { std::mem::zeroed::<SECURITY_DESCRIPTOR>() });
        // SAFETY: `descriptor` is a correctly sized, writable SECURITY_DESCRIPTOR.
        if unsafe {
            InitializeSecurityDescriptor(
                (&raw mut *descriptor).cast::<c_void>(),
                SECURITY_DESCRIPTOR_REVISION,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the owner SID and the list outlive `descriptor` inside `self`.
        if unsafe {
            SetSecurityDescriptorOwnerChecked(&mut descriptor, user.as_psid())
                && SetSecurityDescriptorDaclChecked(&mut descriptor, acl.0)
                && SetSecurityDescriptorProtected(&mut descriptor)
        } {
            Ok(Self {
                _owner: user,
                acl,
                descriptor,
            })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Attributes usable while `self` is alive. The descriptor keeps pointers into `self`, so
    /// the returned value must not outlive it.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&raw const *self.descriptor).cast::<c_void>().cast_mut(),
            bInheritHandle: 0,
        }
    }
}

#[allow(non_snake_case)]
unsafe fn SetSecurityDescriptorOwnerChecked(
    descriptor: &mut SECURITY_DESCRIPTOR,
    owner: PSID,
) -> bool {
    // SAFETY: the descriptor is initialized and `owner` stays alive in the caller.
    unsafe {
        windows_sys::Win32::Security::SetSecurityDescriptorOwner(
            (&raw mut *descriptor).cast::<c_void>(),
            owner,
            0,
        ) != 0
    }
}

#[allow(non_snake_case)]
unsafe fn SetSecurityDescriptorDaclChecked(
    descriptor: &mut SECURITY_DESCRIPTOR,
    acl: *mut ACL,
) -> bool {
    // SAFETY: the descriptor is initialized and `acl` stays alive in the caller.
    unsafe {
        windows_sys::Win32::Security::SetSecurityDescriptorDacl(
            (&raw mut *descriptor).cast::<c_void>(),
            1,
            acl,
            0,
        ) != 0
    }
}

#[allow(non_snake_case)]
unsafe fn SetSecurityDescriptorProtected(descriptor: &mut SECURITY_DESCRIPTOR) -> bool {
    // SAFETY: the descriptor is initialized; only the protected bit is touched.
    unsafe {
        windows_sys::Win32::Security::SetSecurityDescriptorControl(
            (&raw mut *descriptor).cast::<c_void>(),
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        ) != 0
    }
}

/// Replaces the access control list of an existing path with the owner-only list, dropping any
/// entry inherited from a permissive parent.
pub fn restrict_path(path: &Path, container: bool) -> io::Result<()> {
    let owner_only = OwnerOnly::new(container)?;
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and the list outlives the call.
    checked(unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            owner_only.acl.0,
            std::ptr::null(),
        )
    })
}

/// A security descriptor read back from an object, freed on drop.
struct ReadDescriptor {
    raw: PSECURITY_DESCRIPTOR,
    owner: PSID,
    dacl: *mut ACL,
}

impl Drop for ReadDescriptor {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: the descriptor came from Get*SecurityInfo and is freed exactly once.
            unsafe { LocalFree(self.raw as HLOCAL) };
        }
    }
}

impl ReadDescriptor {
    fn of_path(path: &Path) -> io::Result<Self> {
        let name = wide(path.as_os_str());
        let mut read = Self {
            raw: std::ptr::null_mut(),
            owner: std::ptr::null_mut(),
            dacl: std::ptr::null_mut(),
        };
        // SAFETY: `name` is NUL-terminated and every out parameter is a live local.
        checked(unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut read.owner,
                std::ptr::null_mut(),
                &mut read.dacl,
                std::ptr::null_mut(),
                &mut read.raw,
            )
        })?;
        Ok(read)
    }

    fn of_handle(handle: HANDLE) -> io::Result<Self> {
        let mut read = Self {
            raw: std::ptr::null_mut(),
            owner: std::ptr::null_mut(),
            dacl: std::ptr::null_mut(),
        };
        // SAFETY: `handle` is open and every out parameter is a live local.
        checked(unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut read.owner,
                std::ptr::null_mut(),
                &mut read.dacl,
                std::ptr::null_mut(),
                &mut read.raw,
            )
        })?;
        Ok(read)
    }

    /// Whether no account other than `user` is granted anything, and the owner is an identity
    /// this token holds.
    fn private_to(&self, user: &Sid) -> io::Result<bool> {
        if self.dacl.is_null() {
            // An absent list grants every account full access.
            return Ok(false);
        }
        let mut controls = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: the descriptor owns the backing memory and both outputs are writable locals.
        if unsafe {
            windows_sys::Win32::Security::GetSecurityDescriptorControl(
                self.raw,
                &mut controls,
                &mut revision,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if controls & SE_DACL_PROTECTED == 0 {
            return Ok(false);
        }

        // SAFETY: the owner pointer belongs to the descriptor this value still owns.
        let owner = unsafe { Sid::copy_from(self.owner) }?;
        if owner != *user && !token_holds(&owner)? {
            return Ok(false);
        }
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: the list is valid and the information buffer matches its class.
        if unsafe {
            GetAclInformation(
                self.dacl,
                (&raw mut size).cast::<c_void>(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        for index in 0..size.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: `index` is below the reported entry count.
            if unsafe { GetAce(self.dacl, index, &mut ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: every entry starts with an ACE_HEADER whose first byte is its type.
            let kind = u32::from(unsafe { *ace.cast::<u8>() });
            if is_deny_or_audit(kind) {
                // Denying and auditing an account never widens what it may reach.
                continue;
            }
            if kind != ACCESS_ALLOWED_ACE_TYPE && kind != ACCESS_ALLOWED_CALLBACK_ACE_TYPE {
                // An entry shape this build cannot read may grant another account access.
                return Ok(false);
            }
            // SAFETY: an allow entry stores its trustee at the `SidStart` offset.
            let sid = unsafe { ace.cast::<u8>().add(ALLOWED_ACE_SID_OFFSET) };
            // SAFETY: `sid` points inside the entry at a valid SID.
            if unsafe { EqualSid(sid.cast(), user.as_psid()) } == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Byte offset of `SidStart` inside `ACCESS_ALLOWED_ACE`: a four byte header plus the mask.
const ALLOWED_ACE_SID_OFFSET: usize = 8;

fn is_deny_or_audit(kind: u32) -> bool {
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE, SYSTEM_AUDIT_ACE_TYPE,
        SYSTEM_AUDIT_CALLBACK_ACE_TYPE, SYSTEM_MANDATORY_LABEL_ACE_TYPE,
    };
    [
        ACCESS_DENIED_ACE_TYPE,
        ACCESS_DENIED_CALLBACK_ACE_TYPE,
        SYSTEM_AUDIT_ACE_TYPE,
        SYSTEM_AUDIT_CALLBACK_ACE_TYPE,
        SYSTEM_MANDATORY_LABEL_ACE_TYPE,
    ]
    .contains(&kind)
}

/// Whether only this account can reach `path`.
pub fn path_is_private(path: &Path) -> io::Result<bool> {
    let user = current_user()?;
    ReadDescriptor::of_path(path)?.private_to(&user)
}

/// Whether only this account can reach the kernel object behind `handle`, used to prove the
/// process on the far end of a named pipe could not have been another account's squatter.
pub fn handle_is_private(handle: HANDLE) -> io::Result<bool> {
    let user = current_user()?;
    ReadDescriptor::of_handle(handle)?.private_to(&user)
}

/// Whether the other end of a connected pipe belongs to this account.
///
/// Pipe ACLs govern who may create connections. Each connected instance is a separate byte
/// stream, so the account holding the opposite endpoint is the identity that can read or write
/// this channel.
fn pipe_peer_is_current_user(handle: HANDLE) -> io::Result<bool> {
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeInfo, GetNamedPipeServerProcessId, PIPE_SERVER_END,
    };

    let mut flags = 0;
    // SAFETY: `handle` is open and `flags` points to a writable local.
    if unsafe {
        GetNamedPipeInfo(
            handle,
            &mut flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut peer = 0;
    // SAFETY: the pipe handle is open, the API identifies its other endpoint, and `peer` is a
    // writable local. A server handle has a client peer; a client handle has a server peer.
    let read = if flags & PIPE_SERVER_END != 0 {
        unsafe { GetNamedPipeClientProcessId(handle, &mut peer) }
    } else {
        unsafe { GetNamedPipeServerProcessId(handle, &mut peer) }
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    if peer == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "the pipe has no connected peer",
        ));
    }
    process_is_current_user(peer, &current_user()?)
}

/// Opens the channel a caller handed over as a handle value, refusing one another account
/// could read or write.
///
/// The handle is duplicated so it belongs to the returned `File` alone and is not inheritable,
/// which keeps a program the Node starts from being handed the secret's channel. A disk file
/// must carry an owner-only access control list. Each connected pipe instance is its own stream;
/// the process at the other end must run as this account. A pipe name's ACL only governs who may
/// create other connections to that name.
pub fn handed_over(reference: usize) -> io::Result<File> {
    use windows_sys::Win32::{
        Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
        Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType},
        System::Threading::GetCurrentProcess,
    };
    let handed: HANDLE = reference as HANDLE;
    let mut owned: HANDLE = std::ptr::null_mut();
    // SAFETY: both process handles are pseudo handles for this process and the out parameter
    // is a live local; failure is reported in the return value.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handed,
            GetCurrentProcess(),
            &mut owned,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the handed-over channel cannot be read",
        ));
    }
    let owned = OwnedHandle(owned);
    // SAFETY: the handle was just duplicated successfully.
    let kind = unsafe { GetFileType(owned.0) };
    match kind {
        FILE_TYPE_PIPE => {
            if !pipe_peer_is_current_user(owned.0)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the handed-over pipe must connect to this account",
                ));
            }
        }
        FILE_TYPE_DISK => {
            if !handle_is_private(owned.0)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the handed-over channel must be an owner-only file or pipe",
                ));
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the handed-over channel must be a file or a pipe",
            ));
        }
    }
    let handle = owned.0;
    std::mem::forget(owned);
    from_handle(handle)
}

/// Whether a named pipe already exists under `name`, whichever account created it.
pub fn pipe_name_exists(name: &str) -> bool {
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND},
        System::Pipes::WaitNamedPipeW,
    };
    let name = wide(OsStr::new(name));
    // SAFETY: `name` is NUL-terminated; a one millisecond timeout only polls the namespace.
    if unsafe { WaitNamedPipeW(name.as_ptr(), 1) } != 0 {
        return true;
    }
    // Every other failure, such as all instances being busy, still means the name is taken.
    !matches!(
        io::Error::last_os_error()
            .raw_os_error()
            .map(|code| code as u32),
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
    )
}

/// Whether the client connected to `server` runs as the account running this process.
///
/// Impersonating the client reads the account behind the connection itself, which no later
/// process lookup can be confused about. When the client refused impersonation, the identity of
/// its process is read instead and the pipe handle, which keeps the connection open, is what
/// ties that process id to this connection.
pub fn pipe_client_is_current_user(
    server: &tokio::net::windows::named_pipe::NamedPipeServer,
) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::{
        Foundation::ERROR_CANNOT_IMPERSONATE,
        Security::{RevertToSelf, TOKEN_QUERY},
        System::Pipes::{GetNamedPipeClientProcessId, ImpersonateNamedPipeClient},
        System::Threading::{GetCurrentThread, OpenThreadToken},
    };
    let expected = current_user()?;
    let handle: HANDLE = server.as_raw_handle().cast();
    // SAFETY: the pipe handle is open for the whole call.
    if unsafe { ImpersonateNamedPipeClient(handle) } != 0 {
        let read = (|| -> io::Result<Sid> {
            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: the thread is impersonating, so it has a token to open.
            if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            token_user(OwnedHandle(token).0)
        })();
        // SAFETY: impersonation succeeded, so reverting restores this thread's own token.
        if unsafe { RevertToSelf() } == 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(read? == expected);
    }
    let failure = io::Error::last_os_error();
    if failure.raw_os_error().map(|code| code as u32) != Some(ERROR_CANNOT_IMPERSONATE) {
        return Err(failure);
    }
    let mut client = 0u32;
    // SAFETY: the pipe handle is open and `client` is a live local.
    if unsafe { GetNamedPipeClientProcessId(handle, &mut client) } == 0 {
        return Err(io::Error::last_os_error());
    }
    process_is_current_user(client, &expected)
}

fn process_is_current_user(process_id: u32, expected: &Sid) -> io::Result<bool> {
    use windows_sys::Win32::{
        Security::TOKEN_QUERY,
        System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    // SAFETY: `process_id` came from the kernel for the connected pipe peer.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the process handle is open and `token` is a live local.
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    Ok(token_user(token.0)? == *expected)
}

fn token_user(token: HANDLE) -> io::Result<Sid> {
    let mut needed = 0u32;
    // SAFETY: a null buffer with length zero asks only for the required size.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: `buffer` holds the size the previous call asked for.
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the usize allocation aligns TOKEN_USER and covers its reported size; its SID stays inside the live buffer.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: the SID belongs to the buffer that is still alive here.
    unsafe { Sid::copy_from(user.User.Sid) }
}

#[cfg(test)]
mod handed_over_pipe_tests {
    use super::{OwnedHandle, OwnerOnly, handed_over, wide};
    use std::{
        ffi::OsStr,
        io::{self, Read as _},
        os::windows::io::AsRawHandle as _,
    };
    use windows_sys::Win32::{
        Foundation::{
            ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetHandleInformation, HANDLE,
            HANDLE_FLAG_INHERIT, LocalFree,
        },
        Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
        Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES},
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, ReadFile, WriteFile,
        },
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, CreatePipe, GetNamedPipeInfo, PIPE_ACCESS_DUPLEX,
            PIPE_ACCESS_INBOUND, PIPE_READMODE_BYTE, PIPE_SERVER_END, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
    };

    fn anonymous_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        // SAFETY: both output handles and the inheritable attributes are live locals.
        if unsafe { CreatePipe(&mut read, &mut write, &mut attributes, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((OwnedHandle(read), OwnedHandle(write)))
    }

    fn named_pipe(
        attributes: Option<&SECURITY_ATTRIBUTES>,
    ) -> io::Result<(OwnedHandle, OwnedHandle)> {
        let pipe_name = format!(r"\\.\pipe\vibeface-secret-{}", uuid::Uuid::new_v4());
        let pipe_name = wide(OsStr::new(&pipe_name));
        // SAFETY: the name is terminated and attributes live until CreateNamedPipeW copies them.
        let server = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                attributes.map_or(std::ptr::null(), std::ptr::from_ref),
            )
        };
        if server.is_null() || server == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let server = OwnedHandle(server);
        // SAFETY: the terminated name identifies the server created above. Connecting before
        // ConnectNamedPipe is supported and is reported below as ERROR_PIPE_CONNECTED.
        let client = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if client.is_null() || client == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let client = OwnedHandle(client);
        // SAFETY: `server` is an open server endpoint and no overlapped structure is used.
        if unsafe { ConnectNamedPipe(server.0, std::ptr::null_mut()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error().map(|code| code as u32) != Some(ERROR_PIPE_CONNECTED) {
                return Err(error);
            }
        }
        Ok((server, client))
    }

    struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: ConvertStringSecurityDescriptorToSecurityDescriptorW allocated this value.
            unsafe { LocalFree(self.0) };
        }
    }

    fn everyone_pipe_attributes() -> io::Result<(LocalSecurityDescriptor, SECURITY_ATTRIBUTES)> {
        let sddl = wide(OsStr::new("D:(A;;GA;;;WD)"));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the SDDL is terminated and the output is writable; the returned descriptor is
        // freed by `LocalSecurityDescriptor`.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor = LocalSecurityDescriptor(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        Ok((descriptor, attributes))
    }

    fn write_one(handle: HANDLE, byte: u8) -> io::Result<()> {
        let mut written = 0;
        // SAFETY: `byte` and `written` are live for the synchronous write.
        if unsafe { WriteFile(handle, &byte, 1, &mut written, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if written != 1 {
            return Err(io::Error::other("pipe wrote an incomplete byte"));
        }
        Ok(())
    }

    fn read_one(handle: HANDLE) -> io::Result<u8> {
        let mut byte = 0;
        let mut read = 0;
        // SAFETY: `byte` and `read` are live for the synchronous read.
        if unsafe { ReadFile(handle, &mut byte, 1, &mut read, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if read != 1 {
            return Err(io::Error::other("pipe returned an incomplete byte"));
        }
        Ok(byte)
    }

    fn handle_flags(handle: HANDLE) -> io::Result<u32> {
        let mut flags = 0;
        // SAFETY: `handle` is open and `flags` is a writable local.
        if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags)
    }

    #[test]
    fn inherited_default_anonymous_pipe_checks_the_peer_and_keeps_the_original() -> io::Result<()> {
        let (read, write) = anonymous_pipe()?;
        assert_ne!(handle_flags(read.0)? & HANDLE_FLAG_INHERIT, 0);

        let mut handed = handed_over(read.0 as usize)?;
        let handle = handed.as_raw_handle().cast();
        assert_eq!(handle_flags(handle)? & HANDLE_FLAG_INHERIT, 0);
        write_one(write.0, b'a')?;
        let mut byte = [0];
        handed.read_exact(&mut byte)?;
        assert_eq!(byte, [b'a']);

        drop(handed);
        write_one(write.0, b'b')?;
        assert_eq!(read_one(read.0)?, b'b');
        assert!(
            handle_flags(read.0).is_ok(),
            "handover keeps the caller's handle open"
        );
        Ok(())
    }

    #[test]
    fn a_private_named_pipe_with_a_same_account_peer_is_accepted() -> io::Result<()> {
        let security = OwnerOnly::new(false)?;
        let attributes = security.attributes();
        let (server, client) = named_pipe(Some(&attributes))?;
        let mut flags = 0;
        // SAFETY: `server` is connected and the output values are live locals.
        if unsafe {
            GetNamedPipeInfo(
                server.0,
                &mut flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        assert_ne!(flags & PIPE_SERVER_END, 0);

        let mut handed = handed_over(server.0 as usize)?;
        write_one(client.0, b'c')?;
        let mut byte = [0];
        handed.read_exact(&mut byte)?;
        assert_eq!(byte, [b'c']);

        let mut handed_client = handed_over(client.0 as usize)?;
        write_one(server.0, b'e')?;
        handed_client.read_exact(&mut byte)?;
        assert_eq!(byte, [b'e']);
        Ok(())
    }

    #[test]
    fn a_connected_same_account_peer_is_accepted_with_an_everyone_name_acl() -> io::Result<()>
    {
        let (_descriptor, attributes) = everyone_pipe_attributes()?;
        let (server, client) = named_pipe(Some(&attributes))?;
        assert!(handed_over(server.0 as usize).is_ok());
        write_one(client.0, b'd')?;
        assert_eq!(read_one(server.0)?, b'd');
        Ok(())
    }

    #[test]
    fn an_unconnected_named_pipe_is_refused_without_closing_the_original() -> io::Result<()> {
        let pipe_name = format!(r"\\.\pipe\vibeface-secret-{}", uuid::Uuid::new_v4());
        let pipe_name = wide(OsStr::new(&pipe_name));
        let security = OwnerOnly::new(false)?;
        let attributes = security.attributes();
        // SAFETY: the name is terminated and attributes live until CreateNamedPipeW copies them.
        let server = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                &attributes,
            )
        };
        if server.is_null() || server == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let server = OwnedHandle(server);

        assert!(handed_over(server.0 as usize).is_err());
        assert!(
            handle_flags(server.0).is_ok(),
            "a rejected handover keeps the caller's handle open"
        );
        Ok(())
    }

    const INVALID_HANDLE_VALUE: HANDLE = -1_isize as HANDLE;
}

/// The account that owns `path`.
pub fn owner_of_path(path: &Path) -> io::Result<Sid> {
    let read = ReadDescriptor::of_path(path)?;
    // SAFETY: the owner pointer belongs to the descriptor, which is alive until the copy ends.
    unsafe { Sid::copy_from(read.owner) }
}

/// What `create_private_file` does with a path that already holds a file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Refuse the call.
    New,
    /// Empty it.
    Truncate,
    /// Keep its content.
    Keep,
}

/// Creates or opens a file only this account can read or write. The access control list is
/// supplied at creation, so the file is never briefly reachable by another account.
pub fn create_private_file(path: &Path, disposition: Disposition) -> io::Result<File> {
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            CREATE_ALWAYS, CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
        },
    };
    let owner_only = OwnerOnly::new(false)?;
    let attributes = owner_only.attributes();
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and the attributes outlive the call.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            match disposition {
                Disposition::New => CREATE_NEW,
                Disposition::Truncate => CREATE_ALWAYS,
                Disposition::Keep => OPEN_ALWAYS,
            },
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let file = from_handle(handle)?;
    refuse_reparse_point(&file)?;
    if disposition != Disposition::New {
        // An existing file keeps the list it was created with, so it is reapplied here.
        restrict_path(path, false)?;
    }
    Ok(file)
}

/// Opens an existing file for reading, refusing a reparse point at the final component so a
/// link planted in place of a private file cannot redirect the read.
pub fn open_no_reparse(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::{
        Foundation::GENERIC_READ,
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            OPEN_EXISTING,
        },
    };
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and no attributes are supplied for an existing file.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let file = from_handle(handle)?;
    refuse_reparse_point(&file)?;
    Ok(file)
}

/// Creates a directory only this account can read, write, or list, whose files inherit the same
/// single entry.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let owner_only = OwnerOnly::new(true)?;
    let attributes = owner_only.attributes();
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and the attributes outlive the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Moves `from` onto `to` and does not return until the move is on the disk.
pub fn durable_move(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from = wide(from.as_os_str());
    let to = wide(to.as_os_str());
    // SAFETY: both names are NUL-terminated and alive for the call.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Flushes the metadata of `dir` so an entry created or removed in it is on the disk.
pub fn flush_directory(dir: &Path) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FlushFileBuffers, OPEN_EXISTING,
        },
    };
    let name = wide(dir.as_os_str());
    // SAFETY: `name` is NUL-terminated; backup semantics are what open a directory handle.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    let directory = from_handle(handle)?;
    // SAFETY: the handle is open for the whole call.
    if unsafe { FlushFileBuffers(as_handle(&directory)) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        // A file system that does not flush directory handles orders its metadata itself; the
        // content of the file was already flushed by its own handle.
        Some(ERROR_ACCESS_DENIED | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED) => Ok(()),
        _ => Err(error),
    }
}

fn as_handle(file: &File) -> HANDLE {
    use std::os::windows::io::AsRawHandle as _;
    file.as_raw_handle().cast()
}

fn from_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle came from a successful open and is owned only by the returned File.
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

/// The volume and file identifier Windows keeps for this file, which a copy or a restore from a
/// backup does not carry over.
pub fn file_identity(path: &Path) -> io::Result<String> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = File::open(path)?;
    // SAFETY: the handle is open and the information buffer is a live local.
    let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    // SAFETY: as above.
    if unsafe { GetFileInformationByHandle(as_handle(&file), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(format!(
        "{:x}:{:x}{:08x}",
        information.dwVolumeSerialNumber, information.nFileIndexHigh, information.nFileIndexLow
    ))
}

fn refuse_reparse_point(file: &File) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
    };
    // SAFETY: the handle is open and the information buffer is a live local.
    let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    // SAFETY: as above.
    if unsafe { GetFileInformationByHandle(as_handle(file), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a private file path must not be a link",
        ));
    }
    Ok(())
}

/// Points `alias` at the directory `target` with a junction, which needs no privilege a normal
/// account lacks, unlike a directory symbolic link.
pub fn create_junction(target: &Path, alias: &Path) -> io::Result<()> {
    std::fs::create_dir(alias)?;
    let result = point_junction(target, alias);
    if result.is_err() {
        let _ = std::fs::remove_dir(alias);
    }
    result
}

/// Points the existing junction `alias` at `target`.
///
/// One call replaces the whole reparse record, and a path is resolved when it is opened, so a
/// reader sees either the old target or the new one and never a junction with no target.
pub fn point_junction(target: &Path, alias: &Path) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::GENERIC_WRITE,
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
    };
    {
        let name = wide(alias.as_os_str());
        // SAFETY: `name` is NUL-terminated; backup semantics open the directory itself.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        let directory = from_handle(handle)?;
        let buffer = mount_point_buffer(target)?;
        let mut written = 0u32;
        // SAFETY: the buffer is a correctly shaped reparse record of the length passed in.
        if unsafe {
            DeviceIoControl(
                as_handle(&directory),
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut written,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// The directory a junction made by `create_junction` points at.
pub fn read_junction(alias: &Path) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::{
        Foundation::GENERIC_READ,
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl, Ioctl::FSCTL_GET_REPARSE_POINT,
            SystemServices::IO_REPARSE_TAG_MOUNT_POINT,
        },
    };
    let name = wide(alias.as_os_str());
    // SAFETY: `name` is NUL-terminated; the flags open the reparse point itself.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let directory = from_handle(handle)?;
    let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut returned = 0u32;
    // SAFETY: the output buffer has the maximum size a reparse record can use.
    if unsafe {
        DeviceIoControl(
            as_handle(&directory),
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let returned = returned as usize;
    if returned < REPARSE_HEADER_LEN + 8 {
        return Err(io::Error::other("reparse record is too short"));
    }
    let tag = u32::from_le_bytes(buffer[0..4].try_into().expect("four bytes"));
    if tag != IO_REPARSE_TAG_MOUNT_POINT {
        return Err(io::Error::other("directory link is not a junction"));
    }
    let offset = usize::from(u16::from_le_bytes(
        buffer[REPARSE_HEADER_LEN..REPARSE_HEADER_LEN + 2]
            .try_into()
            .expect("two bytes"),
    ));
    let length = usize::from(u16::from_le_bytes(
        buffer[REPARSE_HEADER_LEN + 2..REPARSE_HEADER_LEN + 4]
            .try_into()
            .expect("two bytes"),
    ));
    let start = REPARSE_HEADER_LEN + MOUNT_POINT_FIELDS_LEN + offset;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= returned)
        .ok_or_else(|| io::Error::other("reparse record names no target"))?;
    let units: Vec<u16> = buffer[start..end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let nt_prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    if !units.starts_with(&nt_prefix) {
        return Err(io::Error::other(
            "junction target is not an absolute NT path",
        ));
    }
    let mut windows_path: Vec<u16> = r"\\?\".encode_utf16().collect();
    windows_path.extend_from_slice(&units[nt_prefix.len()..]);
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(
        &windows_path,
    )))
}

/// `ReparseTag`, `ReparseDataLength`, and `Reserved`.
const REPARSE_HEADER_LEN: usize = 8;
/// The four offset and length fields a mount point record starts with.
const MOUNT_POINT_FIELDS_LEN: usize = 8;

fn mount_point_buffer(target: &Path) -> io::Result<Vec<u8>> {
    use std::{
        os::windows::ffi::OsStrExt as _,
        path::{Component, Prefix},
    };
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;
    let encoded: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                unit
            }
        })
        .collect();
    let mut substitute: Vec<u16> = r"\??\".encode_utf16().collect();
    match target.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _) => {
                substitute.extend_from_slice(&encoded[4..])
            }
            Prefix::Disk(_) => substitute.extend_from_slice(&encoded),
            Prefix::UNC(_, _) => {
                substitute.extend("UNC\\".encode_utf16());
                substitute.extend_from_slice(&encoded[2..]);
            }
            _ => {
                return Err(io::Error::other(
                    "a junction target must name an absolute disk or share directory",
                ));
            }
        },
        _ => return Err(io::Error::other("a junction target must be absolute")),
    }
    substitute.push(0);
    // The print name a person sees in Explorer is the plain path.
    let print: Vec<u16> = wide(target.as_os_str());
    let substitute_bytes = (substitute.len() - 1) * 2;
    let print_bytes = (print.len() - 1) * 2;
    let path_bytes = substitute_bytes + 2 + print_bytes + 2;
    let data_length = MOUNT_POINT_FIELDS_LEN + path_bytes;
    if data_length > u16::MAX as usize {
        return Err(io::Error::other(
            "managed directory target path is too long",
        ));
    }
    let mut buffer = Vec::with_capacity(REPARSE_HEADER_LEN + data_length);
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&(data_length as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&(substitute_bytes as u16).to_le_bytes());
    buffer.extend_from_slice(&((substitute_bytes + 2) as u16).to_le_bytes());
    buffer.extend_from_slice(&(print_bytes as u16).to_le_bytes());
    substitute.truncate(substitute.len() - 1);
    for unit in substitute.iter().chain(std::iter::once(&0u16)) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    for unit in &print {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(buffer)
}

/// Authenticate the process attached to an AF_UNIX socket before sending protocol data.
pub fn require_socket_peer(socket: std::os::windows::io::RawSocket) -> io::Result<()> {
    use windows_sys::Win32::{
        Networking::WinSock::{SIO_AF_UNIX_GETPEERPID, SOCKET_ERROR, WSAGetLastError, WSAIoctl},
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let mut pid = 0_u32;
    let mut returned = 0_u32;
    // SAFETY: socket remains open; the output points to one initialized PID-sized buffer.
    if unsafe {
        WSAIoctl(
            socket as _,
            SIO_AF_UNIX_GETPEERPID,
            std::ptr::null(),
            0,
            (&mut pid as *mut u32).cast(),
            std::mem::size_of_val(&pid) as u32,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    } == SOCKET_ERROR
    {
        // SAFETY: WSAGetLastError reads this thread's most recent socket error.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Codex socket has no verifiable peer",
        ));
    }
    // SAFETY: the kernel supplied the PID of this live socket's peer.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    let mut token = std::ptr::null_mut();
    // SAFETY: the process handle is owned above and the token output is writable.
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if token_user(OwnedHandle(token).0)? != current_user()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Codex socket belongs to another account",
        ));
    }
    Ok(())
}
