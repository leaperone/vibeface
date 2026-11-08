//! Windows AF_UNIX connections for the documented Codex `unix://PATH` transport.
//! The Node's maintenance channel continues to use owner-only named pipes.

use async_io::Async;
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket},
    path::Path,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt};

pub struct Stream(Compat<Async<Socket>>);

impl Stream {
    pub async fn connect(path: &Path) -> anyhow::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Codex socket has no parent"))?;
        super::privatefs::ensure_private(parent, "the Codex socket directory")?;
        let owned = path.to_owned();
        let socket =
            tokio::task::spawn_blocking(move || uds_windows::UnixStream::connect(owned)).await??;
        super::winsec::require_socket_peer(socket.as_raw_socket())?;
        Ok(Self(Async::new(Socket(socket))?.compat()))
    }

    pub fn require_current_account(&self) -> io::Result<()> {
        super::winsec::require_socket_peer(self.0.get_ref().get_ref().0.as_raw_socket())
    }
}

struct Socket(uds_windows::UnixStream);
impl AsSocket for Socket {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        // SAFETY: this wrapper owns the socket for the full lifetime of the borrow.
        unsafe { BorrowedSocket::borrow_raw(self.0.as_raw_socket()) }
    }
}
impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}
impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
// SAFETY: reads and writes on a nonblocking AF_UNIX socket cannot invalidate initialized buffers.
unsafe impl async_io::IoSafe for Socket {}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = &mut self.get_mut().0;
        ready!(Pin::new(&mut *stream).poll_flush(cx))?;
        stream.get_ref().get_ref().0.shutdown(Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}
