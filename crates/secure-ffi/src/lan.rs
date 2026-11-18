//! Mobile LAN adapter. The transport implementation lives in `vibeface-native-transport`; this
//! module keeps the existing UniFFI records, objects, functions, and error text stable.

use std::{
    future::Future,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{runtime::Runtime, sync::OnceCell, task::JoinHandle};
use vibeface_native_transport::{LanEndpoint, LanError};
use vibeface_secure as secure;

use crate::{Result, SecureError};

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("vibeface-lan")
        .enable_all()
        .build()
        .expect("LAN runtime")
});
static ENDPOINT: OnceCell<LanEndpoint> = OnceCell::const_new();

fn failed(reason: impl std::fmt::Display) -> SecureError {
    SecureError::Failed {
        reason: reason.to_string(),
    }
}
fn transport(error: LanError) -> SecureError {
    failed(error)
}

struct AbortOnDrop<T>(Option<JoinHandle<T>>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}
async fn on_runtime<T: Send + 'static>(
    work: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let mut task = AbortOnDrop(Some(RUNTIME.spawn(work)));
    task.0
        .as_mut()
        .expect("task handle")
        .await
        .map_err(failed)?
}

async fn endpoint() -> Result<&'static LanEndpoint> {
    ENDPOINT
        .get_or_try_init(|| async { LanEndpoint::bind().await.map_err(failed) })
        .await
}

#[derive(uniffi::Record)]
pub struct LanFrame {
    pub text: bool,
    pub data: Vec<u8>,
}

#[derive(uniffi::Object)]
pub struct LanStream {
    inner: Arc<vibeface_native_transport::LanStream>,
}

#[uniffi::export]
impl LanStream {
    /// Queues one binary frame. A failed write closes the stream, which `recv` then reports.
    pub fn send(&self, data: Vec<u8>) -> Result<()> {
        self.inner.send(data).map_err(transport)
    }

    /// The next binary or text frame, `None` once the Node ended the stream.
    pub async fn recv(self: Arc<Self>) -> Result<Option<LanFrame>> {
        let inner = self.inner.clone();
        on_runtime(async move {
            inner
                .recv()
                .await
                .map(|frame| {
                    frame.map(|frame| LanFrame {
                        text: frame.text,
                        data: frame.data,
                    })
                })
                .map_err(transport)
        })
        .await
    }

    pub fn close(&self) {
        self.inner.close();
    }
}

#[uniffi::export]
pub async fn lan_prepare() -> Result<()> {
    on_runtime(async { endpoint().await.map(|_| ()) }).await
}

#[uniffi::export]
pub async fn lan_connect(
    device_id: String,
    addrs: Vec<String>,
    timeout_ms: u32,
) -> Result<Arc<LanStream>> {
    on_runtime(async move {
        let stream = endpoint()
            .await?
            .connect(&device_id, &addrs, Duration::from_millis(timeout_ms.into()))
            .await
            .map_err(transport)?;
        Ok(Arc::new(LanStream { inner: stream }))
    })
    .await
}

/// Whether an advertised mDNS `tag` names `device_id` of `network_id`, by this device's clock.
#[uniffi::export]
pub fn lan_tag_matches(tag: String, network_id: String, device_id: String) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    secure::lan::lan_tag_matches(&tag, &network_id, &device_id, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_match_by_this_clock() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let tag = secure::lan::lan_tag("net", "dev", secure::lan::hour(now));
        assert!(lan_tag_matches(tag.clone(), "net".into(), "dev".into()));
        assert!(!lan_tag_matches(tag, "net".into(), "other".into()));
    }
}
