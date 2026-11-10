//! Memory budgets at provider transport boundaries. Limits reject whole frames; they never
//! truncate JSON or turn a partial provider reply into a successful terminal event.
use serde_json::Value;
use std::io;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

// Includes pi's existing 10 MiB images after base64 encoding.
pub const FRAME_BYTES: usize = 16 << 20;
pub const BODY_BYTES: usize = 16 << 20;
pub const QUEUE_BYTES: usize = 32 << 20;
pub const QUEUE_ITEMS: usize = 128;

fn extend(bytes: &mut Vec<u8>, input: &[u8], limit: usize) {
    let needed = bytes.len() + input.len();
    if needed > bytes.capacity() {
        let capacity = needed.max(bytes.capacity().saturating_mul(2)).min(limit);
        bytes.reserve_exact(capacity - bytes.len());
    }
    bytes.extend_from_slice(input);
}

fn limit_error(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("provider {what} exceeds its memory budget"),
    )
}

/// Unlike `lines()`, this retains at most `limit` bytes even without a newline. The partial
/// line belongs to the reader, so cancelling `next_line` in a select does not lose bytes.
pub struct Lines<R> {
    reader: R,
    partial: Vec<u8>,
    limit: usize,
    failed: bool,
}

impl<R: AsyncBufRead + Unpin> Lines<R> {
    pub fn new(reader: R, limit: usize) -> Self {
        Self {
            reader,
            partial: Vec::new(),
            limit,
            failed: false,
        }
    }

    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        if self.failed {
            return Err(limit_error("line"));
        }
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return if self.partial.is_empty() {
                    Ok(None)
                } else {
                    self.finish().map(Some)
                };
            }
            let end = available.iter().position(|byte| *byte == b'\n');
            let count = end.unwrap_or(available.len());
            if count > self.limit.saturating_sub(self.partial.len()) {
                self.failed = true;
                return Err(limit_error("line"));
            }
            extend(&mut self.partial, &available[..count], self.limit);
            self.reader.consume(count + usize::from(end.is_some()));
            if end.is_some() {
                return self.finish().map(Some);
            }
        }
    }

    fn finish(&mut self) -> io::Result<String> {
        let mut line = std::mem::take(&mut self.partial);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

/// Reads HTTP bodies incrementally, including chunked bodies with no Content-Length.
pub async fn body(mut response: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= limit as u64),
        "provider HTTP body exceeds its memory budget"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(limit_error("HTTP body").into());
        }
        extend(&mut bytes, &chunk, limit);
    }
    Ok(bytes)
}

/// Parses one SSE event at a time from borrowed chunks. Both the current line and all data
/// fields in an event are bounded, and multiline data uses the SSE-mandated newline separator.
pub struct Sse {
    line: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    limit: usize,
}

impl Sse {
    pub fn new(limit: usize) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            has_data: false,
            limit,
        }
    }

    pub fn next(&mut self, input: &mut &[u8]) -> io::Result<Option<Vec<u8>>> {
        while !input.is_empty() {
            let end = input.iter().position(|byte| *byte == b'\n');
            let count = end.unwrap_or(input.len());
            if count > self.limit.saturating_sub(self.line.len()) {
                return Err(limit_error("SSE line"));
            }
            extend(&mut self.line, &input[..count], self.limit);
            *input = &input[count + usize::from(end.is_some())..];
            if end.is_none() {
                break;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            if self.line.is_empty() {
                if self.has_data {
                    self.has_data = false;
                    return Ok(Some(std::mem::take(&mut self.data)));
                }
            } else if let Some(value) = self.line.strip_prefix(b"data:") {
                let value = value.strip_prefix(b" ").unwrap_or(value);
                let added = value.len().saturating_add(usize::from(self.has_data));
                if added > self.limit.saturating_sub(self.data.len()) {
                    return Err(limit_error("assembled SSE event"));
                }
                if self.has_data {
                    extend(&mut self.data, b"\n", self.limit);
                }
                extend(&mut self.data, value, self.limit);
                self.has_data = true;
            }
            self.line.clear();
        }
        Ok(None)
    }

    /// An event without its terminating blank line is incomplete and must be reconciled.
    pub fn finish(&self) -> io::Result<()> {
        if self.has_data || !self.line.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "provider SSE ended during an event",
            ));
        }
        Ok(())
    }
}

// Charge retained tree capacity, not just serialized text. Use saturating arithmetic so
// pathological nesting/counts cannot wrap into a small reservation. Map entries include a
// conservative allowance for BTree node slots and allocator overhead.
pub fn value_bytes(value: &Value) -> usize {
    let extra = match value {
        Value::String(value) => value.capacity(),
        Value::Array(values) => values
            .capacity()
            .saturating_mul(std::mem::size_of::<Value>())
            .saturating_add(
                values
                    .iter()
                    .map(value_bytes)
                    .fold(0usize, usize::saturating_add),
            ),
        Value::Object(values) => values.iter().fold(0usize, |size, (key, value)| {
            size.saturating_add(128)
                .saturating_add(key.capacity())
                .saturating_add(value_bytes(value))
        }),
        _ => 0,
    };
    std::mem::size_of::<Value>().saturating_add(extra)
}

struct Queued<T> {
    value: T,
    _bytes: OwnedSemaphorePermit,
}
pub struct Sender<T> {
    sender: mpsc::Sender<Queued<T>>,
    measure: fn(&T) -> usize,
    bytes: Arc<Semaphore>,
    budget: usize,
    failure: Arc<Mutex<Option<String>>>,
}
pub struct Receiver<T> {
    receiver: mpsc::Receiver<Queued<T>>,
    failure: Arc<Mutex<Option<String>>>,
}

pub type JsonSender = Sender<Value>;
pub type JsonReceiver = Receiver<Value>;

pub fn json_channel(items: usize, bytes: usize) -> (JsonSender, JsonReceiver) {
    channel(items, bytes, value_bytes)
}

pub fn channel<T>(
    items: usize,
    bytes: usize,
    measure: fn(&T) -> usize,
) -> (Sender<T>, Receiver<T>) {
    assert!(bytes <= u32::MAX as usize);
    let (sender, receiver) = mpsc::channel(items);
    let failure = Arc::new(Mutex::new(None));
    (
        Sender {
            sender,
            measure,
            bytes: Arc::new(Semaphore::new(bytes)),
            budget: bytes,
            failure: failure.clone(),
        },
        Receiver { receiver, failure },
    )
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self { sender: self.sender.clone(), measure: self.measure, bytes: self.bytes.clone(),
            budget: self.budget, failure: self.failure.clone() }
    }
}

impl<T> Sender<T> {
    pub fn fail(&self, error: impl ToString) {
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_string());
    }

    /// For multiplexed stdout: never wait for progress while an RPC response may be behind it.
    /// The caller must fail transport intake on pressure, release RPC waiters and reconcile.
    pub fn try_send(&self, value: T) -> io::Result<()> {
        let size = (self.measure)(&value);
        if size > self.budget {
            return Err(limit_error("queued event"));
        }
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(size as u32)
            .map_err(|_| limit_error("event queue bytes"))?;
        self.sender
            .try_send(Queued {
                value,
                _bytes: bytes,
            })
            .map_err(|error| io::Error::other(format!("provider event queue unavailable: {error}")))
    }

    /// For independent event streams: backpressure without blocking provider HTTP RPC.
    pub async fn send(&self, value: T) -> io::Result<()> {
        let size = (self.measure)(&value);
        if size > self.budget {
            return Err(limit_error("queued event"));
        }
        let bytes = self
            .bytes
            .clone()
            .acquire_many_owned(size as u32)
            .await
            .map_err(io::Error::other)?;
        self.sender
            .send(Queued {
                value,
                _bytes: bytes,
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "provider event consumer closed")
            })
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        self.receiver.try_recv().map(|queued| queued.value)
    }

    pub async fn recv(&mut self) -> Option<T> {
        self.receiver.recv().await.map(|queued| queued.value)
    }

    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A bounded response mailbox reservation, independent of the progress queue. RPC readers
/// use try_hold so progress and response consumers can never block each other's pipe.
pub struct JsonBudget {
    bytes: Arc<Semaphore>,
    limit: usize,
}
#[derive(Debug)]
pub struct HeldJson {
    pub value: Value,
    _bytes: OwnedSemaphorePermit,
}
impl JsonBudget {
    pub fn new(limit: usize) -> Self {
        assert!(limit <= u32::MAX as usize);
        Self {
            bytes: Arc::new(Semaphore::new(limit)),
            limit,
        }
    }
    pub fn try_hold(&self, value: Value) -> io::Result<HeldJson> {
        let size = value_bytes(&value);
        if size > self.limit {
            return Err(limit_error("response mailbox"));
        }
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(size as u32)
            .map_err(|_| limit_error("response mailbox bytes"))?;
        Ok(HeldJson {
            value,
            _bytes: bytes,
        })
    }
}

/// Bounds serialization as it happens, before the HTTP client or stdin allocates a full body.
pub fn json(value: &Value, limit: usize) -> io::Result<Vec<u8>> {
    struct Writer {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl io::Write for Writer {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            if input.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(limit_error("outbound JSON"));
            }
            extend(&mut self.bytes, input, self.limit);
            Ok(input.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(io::Error::other)?;
    Ok(writer.bytes)
}

/// A slow progress callback must not prevent cancellation or outlive the turn's deadline.
/// The caller retains its existing unknown/readback handling when delivery is interrupted.
pub async fn deliver(
    delivery: impl std::future::Future<Output = anyhow::Result<()>>,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    tokio::select! {
        biased;
        _ = crate::agent::wait_for_cancel(cancel) => anyhow::bail!("provider progress delivery cancelled"),
        _ = tokio::time::sleep_until(deadline) => anyhow::bail!("provider progress delivery timed out"),
        result = delivery => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn oversized_line_is_rejected_before_full_allocation() {
        let input = vec![b'x'; 64 * 1024];
        let mut lines = Lines::new(BufReader::with_capacity(32, input.as_slice()), 127);
        assert_eq!(
            lines.next_line().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(lines.partial.len() <= 127);
        assert!(lines.partial.capacity() <= 127);
        assert!(lines.next_line().await.is_err());
    }

    #[tokio::test]
    async fn cancellation_keeps_fragmented_line_and_normal_terminal_frame() {
        let (mut writer, reader) = tokio::io::duplex(256);
        let mut lines = Lines::new(BufReader::new(reader), 128);
        writer.write_all(b"{\"type\":").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), lines.next_line())
                .await
                .is_err()
        );
        writer.write_all(b"\"done\"}\r\nlast").await.unwrap();
        drop(writer);
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("{\"type\":\"done\"}")
        );
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("last"));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[test]
    fn sse_fragments_and_multiline_data_preserve_terminal_json() {
        let input = b": ping\r\ndata: {\"type\":\r\ndata: \"session.idle\"}\r\n\r\n";
        let mut sse = Sse::new(128);
        let mut events = Vec::new();
        for byte in input {
            let bytes = [*byte];
            let mut fragment = bytes.as_slice();
            while let Some(event) = sse.next(&mut fragment).unwrap() {
                events.push(event);
            }
        }
        assert_eq!(events, vec![b"{\"type\":\n\"session.idle\"}".to_vec()]);
        assert_eq!(
            serde_json::from_slice::<Value>(&events[0]).unwrap(),
            json!({"type":"session.idle"})
        );
        sse.finish().unwrap();
    }

    #[test]
    fn sse_budget_includes_all_data_lines_before_the_blank_line() {
        let mut sse = Sse::new(32);
        for _ in 0..4 {
            assert!(
                sse.next(&mut b"data: 1234567\n".as_slice())
                    .unwrap()
                    .is_none()
            );
        }
        assert!(sse.next(&mut b"data: 1\n".as_slice()).is_err());
        assert!(sse.data.len() <= 32 && sse.data.capacity() <= 32);
        let mut sse = Sse::new(8);
        assert!(sse.next(&mut b"data: 123".as_slice()).is_err());
        assert!(sse.line.capacity() <= 8);
    }

    #[test]
    fn truncated_sse_never_dispatches_a_terminal_event() {
        let mut sse = Sse::new(128);
        assert!(
            sse.next(&mut b"data: {\"type\":\"session.idle\"}\n".as_slice())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            sse.finish().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn event_queue_bounds_items_and_bytes_and_preserves_failure() {
        let event = json!({"type":"progress", "text":"hello"});
        let bytes = value_bytes(&event);
        let (sender, mut receiver) = json_channel(2, bytes);
        sender.try_send(event.clone()).unwrap();
        assert!(sender.try_send(event.clone()).is_err());
        assert_eq!(receiver.recv().await.unwrap(), event);
        sender.try_send(event.clone()).unwrap();
        sender.fail("queue pressure requires readback");
        drop(sender);
        assert_eq!(receiver.recv().await.unwrap(), event);
        assert!(receiver.recv().await.is_none());
        assert_eq!(
            receiver.failure().as_deref(),
            Some("queue pressure requires readback")
        );
        let (sender, mut receiver) = json_channel(1, bytes * 4);
        sender.try_send(event.clone()).unwrap();
        assert!(sender.try_send(event.clone()).is_err());
        assert_eq!(receiver.recv().await.unwrap(), event);
    }

    #[tokio::test]
    async fn backpressure_keeps_terminal_and_releases_cancelled_reservations() {
        let progress = json!({"type":"progress"});
        let terminal = json!({"type":"done"});
        let (sender, mut receiver) = json_channel(1, 8192);
        sender.try_send(progress.clone()).unwrap();
        let sending = sender.send(terminal.clone());
        tokio::pin!(sending);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut sending)
                .await
                .is_err()
        );
        assert_eq!(receiver.recv().await, Some(progress.clone()));
        sending.await.unwrap();
        assert_eq!(receiver.recv().await, Some(terminal.clone()));
        // Cancelling while waiting for an item slot must return the byte reservation.
        sender.try_send(progress).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), sender.send(terminal))
                .await
                .is_err()
        );
        receiver.recv().await.unwrap();
        assert_eq!(sender.bytes.available_permits(), 8192);
    }

    #[tokio::test]
    async fn closing_consumer_unblocks_backpressure() {
        let event = json!({"type":"done"});
        let size = value_bytes(&event);
        let (sender, receiver) = json_channel(1, size);
        sender.try_send(event.clone()).unwrap();
        let sending = sender.send(event);
        tokio::pin!(sending);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut sending)
                .await
                .is_err()
        );
        drop(receiver);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), sending)
                .await
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn json_and_response_mailbox_are_bounded_independently_of_progress() {
        assert_eq!(
            json(&json!({"done":true}), 13).unwrap(),
            br#"{"done":true}"#
        );
        assert!(json(&json!("x".repeat(128)), 32).is_err());
        let value = json!({"type":"response", "data":[1,2,3]});
        let size = value_bytes(&value);
        let budget = JsonBudget::new(size);
        let held = budget.try_hold(value.clone()).unwrap();
        assert!(budget.try_hold(value.clone()).is_err());
        assert_eq!(held.value["data"], json!([1, 2, 3]));
        drop(held);
        assert!(budget.try_hold(value).is_ok());
        assert!(value_bytes(&json!({"data": ["hello"]})) > "hello".len());
    }

    #[tokio::test]
    async fn stalled_progress_delivery_is_cancellable_and_deadline_bounded() {
        let (stop, mut cancel) = tokio::sync::watch::channel(false);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        stop.send(true).unwrap();
        let error = deliver(std::future::pending(), &mut cancel, deadline)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        let (_stop, mut cancel) = tokio::sync::watch::channel(false);
        let error = deliver(
            std::future::pending(),
            &mut cancel,
            tokio::time::Instant::now(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    // Controlled loopback HTTP only. No provider, user state or credentials are involved.
    async fn http_response(wire: &'static [u8]) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            use tokio::io::AsyncReadExt;
            let _ = stream.read(&mut request).await;
            let _ = stream.write_all(wire).await;
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn http_budget_checks_known_and_chunked_lengths_and_small_json() {
        let response = http_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n123456789",
        )
        .await;
        assert!(body(response, 8).await.is_err());
        let response = http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\n12345\r\n5\r\n67890\r\n0\r\n\r\n").await;
        assert!(body(response, 8).await.is_err());
        let response =
            http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await;
        assert_eq!(body(response, 8).await.unwrap(), b"{}");
    }
}
