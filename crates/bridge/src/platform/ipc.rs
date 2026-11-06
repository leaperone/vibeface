//! A local endpoint only the account running this process can connect to.
//!
//! Unix uses a `0o600` socket inside a `0o700` directory and reads the credentials of every
//! peer. Windows uses a named pipe whose protected access control list names the token user,
//! claims the first instance of the name so no other process can squat it, and reads the
//! account behind every client by impersonating it. Neither family listens on a network
//! socket, so nothing here is reachable from another machine or another account.

use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Where a local endpoint lives. The displayed path is what `vibeface status` reports and what
/// the CLI passes back to the Node, so it round-trips through [`Endpoint::from_path`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    display: PathBuf,
    #[cfg(windows)]
    name: String,
}

impl Endpoint {
    /// An endpoint named `name` belonging to the private directory `dir`.
    ///
    /// Unix places the socket inside `dir`. Windows has one machine-wide pipe namespace, so the
    /// name of `dir` and a fingerprint of this account are folded into the pipe name; two
    /// accounts on one machine therefore never collide and never squat each other.
    pub fn in_private_dir(dir: &Path, name: &str) -> Result<Self> {
        ensure!(
            !name.is_empty() && !name.contains(['\\', '/', ':']),
            "a local endpoint name must be a plain file name"
        );
        #[cfg(unix)]
        {
            Ok(Self {
                display: dir.join(name),
            })
        }
        #[cfg(windows)]
        {
            let scope = dir
                .file_name()
                .context("a local endpoint needs a named private directory")?
                .to_string_lossy()
                .into_owned();
            let account = super::winsec::current_user()?.fingerprint();
            let name = format!(r"\\.\pipe\{scope}-{name}-{account}");
            Ok(Self {
                display: PathBuf::from(&name),
                name,
            })
        }
    }

    /// Reads back an endpoint the Node reported earlier.
    pub fn from_path(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            ensure!(
                path.is_absolute(),
                "a local endpoint path must be absolute: {}",
                path.display()
            );
            Ok(Self {
                display: path.to_owned(),
            })
        }
        #[cfg(windows)]
        {
            let name = path.to_string_lossy().into_owned();
            ensure!(
                name.starts_with(r"\\.\pipe\") && !name[r"\\.\pipe\".len()..].contains('\\'),
                "a local endpoint must be a named pipe on this machine: {name}"
            );
            Ok(Self {
                display: path.to_owned(),
                name,
            })
        }
    }

    /// The path a person sees and the CLI carries.
    pub fn path(&self) -> &Path {
        &self.display
    }

    /// How a program the Node starts is told to listen here. Unix spells a socket path as a
    /// `unix://` URL; Windows names the pipe, which is the only form a pipe has.
    pub fn listen_argument(&self) -> String {
        #[cfg(unix)]
        {
            format!("unix://{}", self.display.display())
        }
        #[cfg(windows)]
        {
            self.name.clone()
        }
    }

    /// Refuses an endpoint another account could read, replace, or have put there.
    ///
    /// Unix checks the socket and the private directory holding it, because the socket is a
    /// directory entry another account could swap. Windows has no such entry: the name belongs
    /// to this account's part of the machine-wide pipe namespace, and the access control list on
    /// the pipe object itself is checked when [`Stream::connect`] opens it, which is the moment
    /// a request would otherwise reach the wrong process.
    pub fn ensure_private(&self, label: &str) -> Result<()> {
        #[cfg(unix)]
        {
            ensure!(self.display.is_absolute(), "{label} path must be absolute");
            let parent = self
                .display
                .parent()
                .context("a local endpoint has no parent directory")?;
            let Ok(parent_metadata) = std::fs::symlink_metadata(parent) else {
                // Nothing is listening yet; the caller reports that as a stopped Node.
                return Ok(());
            };
            ensure!(
                parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
                "{label} is not inside a directory"
            );
            super::privatefs::ensure_private(parent, &format!("the directory holding {label}"))?;
            let Ok(metadata) = std::fs::symlink_metadata(&self.display) else {
                return Ok(());
            };
            use std::os::unix::fs::FileTypeExt as _;
            ensure!(
                !metadata.file_type().is_symlink(),
                "{label} must not be a symbolic link"
            );
            ensure!(
                metadata.file_type().is_socket(),
                "{label} is occupied by another file"
            );
            ensure!(
                super::privatefs::same_owner(&self.display, parent)?,
                "{label} has unexpected ownership"
            );
            super::privatefs::ensure_private(&self.display, label)
        }
        #[cfg(windows)]
        {
            let account = super::winsec::current_user()?.fingerprint();
            ensure!(
                self.name.ends_with(&format!("-{account}")),
                "{label} is not in this account's part of the local pipe namespace"
            );
            Ok(())
        }
    }

    /// Whether something already holds this endpoint. A `true` answer does not prove the holder
    /// is this Node, which is why [`Stream::connect`] still checks the owner.
    pub fn is_held(&self) -> bool {
        #[cfg(unix)]
        {
            std::fs::symlink_metadata(&self.display).is_ok()
        }
        #[cfg(windows)]
        {
            super::winsec::pipe_name_exists(&self.name)
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.display.to_string_lossy())
    }
}

/// Accepts current-user-only local connections.
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    pending: tokio::net::windows::named_pipe::NamedPipeServer,
    endpoint: Endpoint,
}

impl Drop for Listener {
    fn drop(&mut self) {
        // A Unix socket leaves its directory entry behind when the listener closes, so the next
        // run would find a dead file in its place. A Windows pipe name is freed with its last
        // handle, which the field above just dropped.
        #[cfg(unix)]
        let _ = std::fs::remove_file(self.endpoint.path());
        #[cfg(windows)]
        let _ = &self.endpoint;
    }
}

impl Listener {
    /// Claims `endpoint`. Fails when another process already holds it, so a second Node never
    /// quietly takes over the first one's control surface.
    pub async fn bind(endpoint: &Endpoint) -> Result<Self> {
        #[cfg(unix)]
        {
            let inner = tokio::net::UnixListener::bind(endpoint.path())
                .with_context(|| format!("claim the local endpoint {endpoint}"))?;
            super::privatefs::restrict_file(endpoint.path())?;
            Ok(Self {
                inner,
                endpoint: endpoint.clone(),
            })
        }
        #[cfg(windows)]
        {
            let pending = create_pipe_instance(endpoint, true)?;
            Ok(Self {
                pending,
                endpoint: endpoint.clone(),
            })
        }
    }

    /// Claims `endpoint` for a Node that is starting, clearing only what an earlier run left
    /// behind. A holder that still answers is refused rather than displaced.
    pub async fn claim(endpoint: &Endpoint) -> Result<Self> {
        #[cfg(unix)]
        match std::fs::symlink_metadata(endpoint.path()) {
            Ok(metadata) => {
                use std::os::unix::fs::FileTypeExt as _;
                ensure!(
                    metadata.file_type().is_socket(),
                    "the local endpoint {endpoint} is occupied by another file"
                );
                ensure!(
                    tokio::net::UnixStream::connect(endpoint.path())
                        .await
                        .is_err(),
                    "the local endpoint {endpoint} is already active"
                );
                // A socket entry outlives the process that made it, so an earlier run's is
                // removed now that nothing answers on it.
                std::fs::remove_file(endpoint.path())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(windows)]
        ensure!(
            !endpoint.is_held(),
            "the local endpoint {endpoint} is already active"
        );
        Self::bind(endpoint).await
    }

    /// Waits for the next connection from this account.
    pub async fn accept(&mut self) -> Result<Stream> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            Ok(Stream { inner: stream })
        }
        #[cfg(windows)]
        {
            self.pending.connect().await?;
            // The next instance is created before this one is handed out so the endpoint is
            // never momentarily absent from the namespace.
            let next = create_pipe_instance(&self.endpoint, false)?;
            Ok(Stream {
                inner: PipeSide::Server(std::mem::replace(&mut self.pending, next)),
            })
        }
    }
}

#[cfg(windows)]
fn create_pipe_instance(
    endpoint: &Endpoint,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let owner_only = super::winsec::OwnerOnly::new(false)?;
    let mut attributes = owner_only.attributes();
    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: the attributes and the descriptor they point at outlive this call.
    let server = unsafe {
        options.create_with_security_attributes_raw(
            &endpoint.name,
            (&raw mut attributes).cast::<std::ffi::c_void>(),
        )
    }
    .with_context(|| format!("claim the local endpoint {endpoint}"))?;
    Ok(server)
}

#[cfg(windows)]
enum PipeSide {
    Server(tokio::net::windows::named_pipe::NamedPipeServer),
    Client(tokio::net::windows::named_pipe::NamedPipeClient),
}

/// One local connection. Reads and writes like any byte stream, and can prove which account is
/// on the other end.
pub struct Stream {
    #[cfg(unix)]
    inner: tokio::net::UnixStream,
    #[cfg(windows)]
    inner: PipeSide,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a local connection of this account")
    }
}

impl Stream {
    /// Connects to `endpoint` and refuses an endpoint this account does not own, so a squatter
    /// cannot collect a request meant for the Node.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self> {
        #[cfg(unix)]
        {
            // The directory and socket checks happen in the caller, which knows what the
            // endpoint is for and what to say when it is wrong.
            let inner = tokio::net::UnixStream::connect(endpoint.path()).await?;
            let stream = Self { inner };
            ensure!(
                stream.peer_is_current_account()?,
                "the local endpoint {endpoint} belongs to another account"
            );
            Ok(stream)
        }
        #[cfg(windows)]
        {
            let client = tokio::net::windows::named_pipe::ClientOptions::new()
                .read(true)
                .write(true)
                .open(&endpoint.name)?;
            let stream = Self {
                inner: PipeSide::Client(client),
            };
            ensure!(
                stream.endpoint_is_private()?,
                "the local endpoint {endpoint} belongs to another account"
            );
            Ok(stream)
        }
    }

    /// Whether the process on the other end runs as this account.
    pub fn peer_is_current_account(&self) -> Result<bool> {
        #[cfg(unix)]
        {
            // SAFETY: geteuid reads the calling process and touches no memory.
            Ok(self.inner.peer_cred()?.uid() == unsafe { libc::geteuid() })
        }
        #[cfg(windows)]
        {
            match &self.inner {
                PipeSide::Server(server) => {
                    super::winsec::pipe_client_is_current_user(server).map_err(Into::into)
                }
                PipeSide::Client(_) => self.endpoint_is_private(),
            }
        }
    }

    /// Whether the endpoint itself is reachable only by this account.
    #[cfg(windows)]
    fn endpoint_is_private(&self) -> Result<bool> {
        use std::os::windows::io::AsRawHandle as _;
        let handle = match &self.inner {
            PipeSide::Server(server) => server.as_raw_handle(),
            PipeSide::Client(client) => client.as_raw_handle(),
        };
        super::winsec::handle_is_private(handle.cast()).map_err(Into::into)
    }

    /// Refuses the connection unless the peer runs as this account.
    pub fn require_current_account(&self, label: &str) -> Result<()> {
        ensure!(
            self.peer_is_current_account()
                .with_context(|| format!("read the account behind {label}"))?,
            "{label} requires its local owner"
        );
        Ok(())
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        #[cfg(unix)]
        {
            Pin::new(&mut self.get_mut().inner).poll_read(context, buffer)
        }
        #[cfg(windows)]
        {
            match &mut self.get_mut().inner {
                PipeSide::Server(server) => Pin::new(server).poll_read(context, buffer),
                PipeSide::Client(client) => Pin::new(client).poll_read(context, buffer),
            }
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        #[cfg(unix)]
        {
            Pin::new(&mut self.get_mut().inner).poll_write(context, bytes)
        }
        #[cfg(windows)]
        {
            match &mut self.get_mut().inner {
                PipeSide::Server(server) => Pin::new(server).poll_write(context, bytes),
                PipeSide::Client(client) => Pin::new(client).poll_write(context, bytes),
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        #[cfg(unix)]
        {
            Pin::new(&mut self.get_mut().inner).poll_flush(context)
        }
        #[cfg(windows)]
        {
            match &mut self.get_mut().inner {
                PipeSide::Server(server) => Pin::new(server).poll_flush(context),
                PipeSide::Client(client) => Pin::new(client).poll_flush(context),
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        #[cfg(unix)]
        {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
        }
        #[cfg(windows)]
        {
            match &mut self.get_mut().inner {
                PipeSide::Server(server) => Pin::new(server).poll_shutdown(context),
                PipeSide::Client(client) => Pin::new(client).poll_shutdown(context),
            }
        }
    }
}

/// Two connected ends of one local connection, used where a test or a probe needs a stream
/// without a program on the other side.
pub async fn pair() -> Result<(Stream, Stream)> {
    #[cfg(unix)]
    {
        let (left, right) = tokio::net::UnixStream::pair()?;
        Ok((Stream { inner: left }, Stream { inner: right }))
    }
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let endpoint = Endpoint::from_path(Path::new(&format!(
            r"\\.\pipe\vibeface-pair-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )))?;
        let mut listener = Listener::bind(&endpoint).await?;
        let accepted = tokio::spawn(async move { listener.accept().await });
        let client = Stream::connect(&endpoint).await?;
        Ok((accepted.await??, client))
    }
}

/// A stable name a long-lived program keeps in its environment while the Node moves the
/// endpoint behind it, used for the coordination Agent's App Server link.
pub struct Alias {
    path: PathBuf,
}

impl Alias {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Points the alias at `endpoint` without a moment where it points nowhere.
    ///
    /// Unix replaces a symbolic link. Windows writes the endpoint name into a private pointer
    /// file, because a named pipe has no directory entry to link to.
    pub fn point_at(&self, endpoint: &Endpoint) -> Result<()> {
        #[cfg(unix)]
        {
            let next = self.path.with_extension("next");
            let _ = std::fs::remove_file(&next);
            std::os::unix::fs::symlink(endpoint.path(), &next)?;
            std::fs::rename(&next, &self.path)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            super::privatefs::write_atomic(&self.path, endpoint.to_string().as_bytes())
        }
    }

    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// The endpoint a program reached through `path`, which may be an alias or an endpoint.
    pub fn resolve(path: &Path) -> Result<Endpoint> {
        #[cfg(unix)]
        {
            // The kernel follows the link, so the alias is usable as the endpoint itself.
            Endpoint::from_path(path)
        }
        #[cfg(windows)]
        {
            if let Ok(name) = path.to_string_lossy().parse::<AliasTarget>() {
                return Ok(name.0);
            }
            let bytes = super::privatefs::read_private_bounded(path, 4096, "App Server link")
                .context("read the App Server link this Node published")?;
            let text = String::from_utf8(bytes).context("App Server link must be UTF-8")?;
            Endpoint::from_path(Path::new(text.trim()))
        }
    }
}

/// A path that already names a pipe needs no pointer file behind it.
#[cfg(windows)]
struct AliasTarget(Endpoint);

#[cfg(windows)]
impl std::str::FromStr for AliasTarget {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Endpoint::from_path(Path::new(value)).map(Self)
    }
}

/// The private per-device directory the local endpoints and their aliases live in.
pub fn runtime_dir(key: &str) -> Result<PathBuf> {
    ensure!(
        !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric()),
        "a runtime directory key must be alphanumeric"
    );
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let directory = base.join(format!("vibeface-{key}"));
    if directory.as_os_str().is_empty() {
        bail!("this account has no usable temporary directory");
    }
    Ok(directory)
}
