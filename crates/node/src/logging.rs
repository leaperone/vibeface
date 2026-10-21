//! One structured log per Node: JSON lines in `node.log`, rotated by size while the Node runs.
//! Every record, whether from `tracing`, a browser, the CLI or a subprocess's stderr, is written
//! by [`Sink::write`], the one place that redacts.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{BufRead, Write},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
use tracing::field::{Field, Visit};
use tracing_subscriber::{layer::Context, registry::LookupSpan};

pub const FILE: &str = "node.log";
pub const MAX_FILE_BYTES: u64 = 8 << 20;
/// Files kept, the current one included.
pub const KEEP_FILES: usize = 5;
/// One submitted record as JSON, all fields included.
pub const MAX_RECORD_BYTES: usize = 8 * 1024;
pub const MAX_BATCH: usize = 20;
/// Submitted records per sender per minute.
pub const RATE_PER_MINUTE: u32 = 120;
pub const MAX_READ: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Node,
    Web,
    Agent,
    Cli,
    Codex,
    Cursor,
}

impl std::str::FromStr for Level {
    type Err = anyhow::Error;
    fn from_str(text: &str) -> Result<Self> {
        serde_json::from_value(Value::String(text.to_ascii_lowercase()))
            .context("level is one of trace, debug, info, warn, error")
    }
}

impl std::str::FromStr for Source {
    type Err = anyhow::Error;
    fn from_str(text: &str) -> Result<Self> {
        serde_json::from_value(Value::String(text.to_ascii_lowercase()))
            .context("source is one of node, web, agent, cli, codex, cursor")
    }
}

/// One line of `node.log`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub ts_ms: i64,
    pub level: Level,
    pub source: Source,
    /// Stable dotted name such as `web.uncaught` or `work.state`; a plain `tracing` call without
    /// one gets its module path.
    pub event: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub fields: Map<String, Value>,
}

/// Blanks known secrets, invite tickets and bearer credentials. Every log line passes here.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|secret| secret.len() >= 8) {
        text = text.replace(secret.as_str(), "[redacted]");
    }
    blank_after(&mut text, "vibeface1", false, "[invite]", |c| {
        c.is_ascii_alphanumeric()
    });
    blank_after(&mut text, "Bearer ", true, "[redacted]", |c| {
        c.is_ascii_alphanumeric() || "-._~+/=".contains(c)
    });
    text
}

fn blank_after(
    text: &mut String,
    marker: &str,
    keep_marker: bool,
    with: &str,
    part: impl Fn(char) -> bool,
) {
    let mut from = 0;
    while let Some(found) = text[from..].find(marker) {
        let start = from + found;
        let body = start + marker.len();
        let end = text[body..]
            .find(|c| !part(c))
            .map_or(text.len(), |n| body + n);
        if end == body {
            from = body;
            continue;
        }
        let blank = if keep_marker { body } else { start };
        text.replace_range(blank..end, with);
        from = blank + with.len();
    }
}

fn path(dir: &Path, index: usize) -> PathBuf {
    match index {
        0 => dir.join(FILE),
        n => dir.join(format!("{FILE}.{n}")),
    }
}

fn open(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

struct Rotating {
    dir: PathBuf,
    file: fs::File,
    len: u64,
    max_bytes: u64,
    keep: usize,
}

impl Rotating {
    fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.len > 0 && self.len + line.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(line)?;
        self.len += line.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        for index in (0..self.keep - 1).rev() {
            let _ = fs::rename(path(&self.dir, index), path(&self.dir, index + 1));
        }
        self.file = open(&path(&self.dir, 0))?;
        self.len = 0;
        Ok(())
    }
}

/// The Node's log file and the secrets it must never contain.
pub struct Sink {
    file: Mutex<Rotating>,
    secrets: Mutex<Vec<String>>,
}

impl Sink {
    pub fn open(dir: &Path, max_bytes: u64, keep: usize) -> Result<Self> {
        ensure!(keep >= 1, "keep at least the current log file");
        let current = path(dir, 0);
        let file = open(&current)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(Rotating {
                dir: dir.to_owned(),
                file,
                len,
                max_bytes,
                keep,
            }),
            secrets: Mutex::new(environment_secrets()),
        })
    }

    pub fn add_secret(&self, secret: &str) {
        let mut secrets = self.secrets.lock().expect("log secrets");
        if secret.len() >= 8 && !secrets.iter().any(|known| known == secret) {
            secrets.push(secret.to_owned());
        }
    }

    pub fn write(&self, record: &Record) {
        let Ok(line) = serde_json::to_string(record) else {
            return;
        };
        let mut line = redact(&line, &self.secrets.lock().expect("log secrets"));
        line.push('\n');
        let _ = self.file.lock().expect("log file").write(line.as_bytes());
    }
}

/// Model keys and other credentials the Node process was started with.
fn environment_secrets() -> Vec<String> {
    std::env::vars()
        .filter(|(key, value)| {
            value.len() >= 8
                && ["_KEY", "_TOKEN", "_SECRET", "_PASSWORD"]
                    .iter()
                    .any(|suffix| key.ends_with(suffix))
        })
        .map(|(_, value)| value)
        .collect()
}

static SINK: OnceLock<Arc<Sink>> = OnceLock::new();

/// Keeps `secret` out of every later log line of this process.
pub fn add_secret(secret: &str) {
    if let Some(sink) = SINK.get() {
        sink.add_secret(secret);
    }
}

pub fn write(record: &Record) {
    if let Some(sink) = SINK.get() {
        sink.write(record);
    }
}

/// Info for vibeface and warn for libraries unless `RUST_LOG` says otherwise. A serving Node
/// writes structured records to `node.log` and copies text to stderr only on a terminal, so a
/// service's own stderr file keeps just crashes.
pub fn init(data_dir: Option<&Path>) {
    use std::io::IsTerminal as _;
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let sink = data_dir.and_then(|dir| {
        let sink = Arc::new(Sink::open(dir, MAX_FILE_BYTES, KEEP_FILES).ok()?);
        SINK.set(sink.clone()).ok()?;
        Some(sink)
    });
    let terminal = sink.is_none() || std::io::stderr().is_terminal();
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,vibeface=info,vibeface_bridge=info")),
        )
        .with(sink.map(|sink| Layer { sink }))
        .with(terminal.then(|| fmt::layer().with_writer(std::io::stderr)))
        .init();
}

/// Turns `tracing` events into records. The ids and `source` come from the event's fields or
/// any enclosing span's, so a span around a turn tags every line inside it.
pub struct Layer {
    sink: Arc<Sink>,
}

#[derive(Clone, Default)]
struct Tags {
    source: Option<Source>,
    event: Option<String>,
    conversation_id: Option<String>,
    work_id: Option<String>,
    thread_id: Option<String>,
}

impl Tags {
    fn inherit(&mut self, outer: &Tags) {
        self.source = self.source.or(outer.source);
        for (mine, theirs) in [
            (&mut self.event, &outer.event),
            (&mut self.conversation_id, &outer.conversation_id),
            (&mut self.work_id, &outer.work_id),
            (&mut self.thread_id, &outer.thread_id),
        ] {
            if mine.is_none() {
                mine.clone_from(theirs);
            }
        }
    }
}

struct Collect<'a> {
    tags: &'a mut Tags,
    message: Option<String>,
    fields: Option<&'a mut Map<String, Value>>,
}

impl Collect<'_> {
    fn put(&mut self, name: &str, value: Value) {
        let text = || match &value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        match name {
            "message" => self.message = Some(text()),
            "source" => self.tags.source = text().parse().ok(),
            "event" => self.tags.event = Some(text()),
            "conversation_id" => self.tags.conversation_id = Some(text()),
            "work_id" => self.tags.work_id = Some(text()),
            "thread_id" => self.tags.thread_id = Some(text()),
            _ => {
                if let Some(fields) = self.fields.as_deref_mut() {
                    fields.insert(name.to_owned(), value);
                }
            }
        }
    }
}

impl Visit for Collect<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field.name(), Value::String(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field.name(), Value::String(value.to_owned()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field.name(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field.name(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field.name(), value.into());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field.name(), value.into());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field.name(), Value::String(value.to_string()));
    }
}

impl<S> tracing_subscriber::Layer<S> for Layer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut tags = Tags::default();
        attrs.record(&mut Collect {
            tags: &mut tags,
            message: None,
            fields: None,
        });
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(tags);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id)
            && let Some(tags) = span.extensions_mut().get_mut::<Tags>()
        {
            values.record(&mut Collect {
                tags,
                message: None,
                fields: None,
            });
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut tags = Tags::default();
        let mut fields = Map::new();
        let mut collect = Collect {
            tags: &mut tags,
            message: None,
            fields: Some(&mut fields),
        };
        event.record(&mut collect);
        let message = collect.message.take().unwrap_or_default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope {
                if let Some(outer) = span.extensions().get::<Tags>() {
                    tags.inherit(outer);
                }
            }
        }
        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing::Level::ERROR => Level::Error,
            tracing::Level::WARN => Level::Warn,
            tracing::Level::INFO => Level::Info,
            tracing::Level::DEBUG => Level::Debug,
            tracing::Level::TRACE => Level::Trace,
        };
        self.sink.write(&Record {
            ts_ms: crate::runtime::now_ms(),
            level,
            source: tags.source.unwrap_or(Source::Node),
            event: tags.event.unwrap_or_else(|| metadata.target().to_owned()),
            message,
            conversation_id: tags.conversation_id,
            work_id: tags.work_id,
            thread_id: tags.thread_id,
            fields,
        });
    }
}

/// Which records to read; every condition must hold.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// The least severe level shown.
    pub level: Option<Level>,
    /// Empty shows every source.
    pub sources: Vec<Source>,
    pub conversation_id: Option<String>,
    pub work_id: Option<String>,
    /// Case-insensitive, in the message, event, ids or fields.
    pub text: Option<String>,
    /// The newest this many; defaults to 1000, at most [`MAX_READ`].
    pub limit: Option<usize>,
}

impl Filter {
    pub fn matches(&self, record: &Record) -> bool {
        let same = |wanted: &Option<String>, have: &Option<String>| {
            wanted
                .as_ref()
                .is_none_or(|wanted| have.as_ref() == Some(wanted))
        };
        self.since_ms.is_none_or(|since| record.ts_ms >= since)
            && self.until_ms.is_none_or(|until| record.ts_ms <= until)
            && self.level.is_none_or(|level| record.level >= level)
            && (self.sources.is_empty() || self.sources.contains(&record.source))
            && same(&self.conversation_id, &record.conversation_id)
            && same(&self.work_id, &record.work_id)
            && self.text.as_deref().is_none_or(|text| {
                let text = text.to_lowercase();
                [
                    Some(&record.message),
                    Some(&record.event),
                    record.conversation_id.as_ref(),
                    record.work_id.as_ref(),
                    record.thread_id.as_ref(),
                ]
                .into_iter()
                .flatten()
                .any(|part| part.to_lowercase().contains(&text))
                    || (!record.fields.is_empty()
                        && Value::Object(record.fields.clone())
                            .to_string()
                            .to_lowercase()
                            .contains(&text))
            })
    }

    fn limit(&self) -> usize {
        self.limit.unwrap_or(1000).clamp(1, MAX_READ)
    }
}

/// `node.log` and its rotations in `dir`, oldest first.
pub fn files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = (1..)
        .map(|index| path(dir, index))
        .take_while(|path| path.exists())
        .collect();
    files.reverse();
    files.push(path(dir, 0));
    files
}

/// The newest records in `dir` that match, oldest first. Lines that are not records, such as
/// a text log from before this format, are skipped.
pub fn read(dir: &Path, filter: &Filter) -> Result<Vec<Record>> {
    let limit = filter.limit();
    let mut kept = VecDeque::new();
    for path in files(dir) {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        for line in std::io::BufReader::new(file).split(b'\n') {
            let Ok(record) = serde_json::from_slice::<Record>(&line?) else {
                continue;
            };
            if filter.matches(&record) {
                if kept.len() == limit {
                    kept.pop_front();
                }
                kept.push_back(record);
            }
        }
    }
    Ok(kept.into())
}

/// A record from a browser, the CLI or the coordination Agent, before the Node accepts it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submitted {
    level: Level,
    #[serde(default)]
    event: Option<String>,
    message: String,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    work_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    fields: Map<String, Value>,
}

fn submitted(source: Source, value: &Value, now_ms: i64) -> Result<Record> {
    ensure!(
        value.to_string().len() <= MAX_RECORD_BYTES,
        "log record is larger than {MAX_RECORD_BYTES} bytes"
    );
    let input: Submitted = serde_json::from_value(value.clone()).context("invalid log record")?;
    ensure!(!input.message.trim().is_empty(), "log message is empty");
    let name = |text: &str, extra: &str| {
        !text.is_empty()
            && text.len() <= 128
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    };
    if let Some(event) = &input.event {
        ensure!(name(event, "._-"), "invalid log event name");
    }
    for id in [&input.conversation_id, &input.work_id, &input.thread_id]
        .into_iter()
        .flatten()
    {
        ensure!(name(id, "-_.:"), "invalid id in log record");
    }
    let source_name = serde_json::to_value(source)?;
    Ok(Record {
        ts_ms: now_ms,
        level: input.level,
        source,
        event: input
            .event
            .unwrap_or_else(|| format!("{}.log", source_name.as_str().unwrap_or("node"))),
        message: input.message,
        conversation_id: input.conversation_id,
        work_id: input.work_id,
        thread_id: input.thread_id,
        fields: input.fields,
    })
}

/// Per-sender budget of submitted records in one-minute windows.
#[derive(Default)]
pub struct RateLimit {
    windows: Mutex<HashMap<String, (i64, u32)>>,
}

impl RateLimit {
    fn take(&self, sender: &str, count: u32, now_ms: i64) -> bool {
        let mut windows = self.windows.lock().expect("log rate limit");
        windows.retain(|_, (start, _)| now_ms - *start < 60_000);
        let (_, used) = windows.entry(sender.to_owned()).or_insert((now_ms, 0));
        if *used + count > RATE_PER_MINUTE {
            return false;
        }
        *used += count;
        true
    }
}

/// Accepts a batch from `sender` whole or not at all, and returns how many records it wrote.
pub fn submit(
    limit: &RateLimit,
    sender: &str,
    source: Source,
    records: &Value,
    now_ms: i64,
) -> Result<usize> {
    let records = records.as_array().context("records must be a list")?;
    ensure!(
        !records.is_empty() && records.len() <= MAX_BATCH,
        "send between 1 and {MAX_BATCH} log records at a time"
    );
    let records = records
        .iter()
        .map(|record| submitted(source, record, now_ms))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        limit.take(sender, records.len() as u32, now_ms),
        "too many log records; try again in a minute"
    );
    for record in &records {
        write(record);
    }
    Ok(records.len())
}

/// Copies a child's stderr into the log line by line, through `emit`, until it closes. More than
/// `max` bytes in all is an error; a longer line is split every 4 KiB.
pub async fn forward_lines(
    stderr: impl tokio::io::AsyncRead + Unpin,
    max: usize,
    mut emit: impl FnMut(&str),
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};
    let mut reader = tokio::io::BufReader::new(stderr);
    let (mut total, mut line) = (0, Vec::new());
    loop {
        line.clear();
        let read = (&mut reader)
            .take(4096)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return Ok(());
        }
        total += read;
        ensure!(total <= max, "diagnostic stream exceeded the size limit");
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if !text.is_empty() {
            emit(text);
        }
    }
}

/// Records one line of a child's stderr at the level the line names, info otherwise.
pub fn child_line(source: Source, event: &str, line: &str, thread_id: Option<&str>) {
    let source = match source {
        Source::Codex => "codex",
        Source::Cursor => "cursor",
        _ => "node",
    };
    let words = || line.split(|c: char| !c.is_ascii_alphabetic());
    if words().any(|word| word == "ERROR") {
        tracing::error!(source, event, thread_id, "{line}");
    } else if words().any(|word| word == "WARN") {
        tracing::warn!(source, event, thread_id, "{line}");
    } else {
        tracing::info!(source, event, thread_id, "{line}");
    }
}

/// From a separate process such as `vibeface codex send`: adds one record to this device's Node
/// log through `/api/log` when the Node's local URL is in the environment, as it is in a
/// coordination Agent's shell. Best effort; the command's own result never depends on it.
pub async fn send_to_node(mut record: Value) {
    if let Some(message) = record["message"].as_str() {
        record["message"] = Value::String(message.chars().take(2_000).collect());
    }
    if let (true, Ok(conversation)) = (
        record["conversation_id"].is_null(),
        std::env::var("VIBEFACE_NODE_CONVERSATION"),
    ) {
        record["conversation_id"] = Value::String(conversation);
    }
    let Ok(url) = std::env::var(crate::runtime::TRIAGE_URL_ENV) else {
        return;
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return;
    };
    let body = serde_json::json!({"source":"agent","records":[record]});
    let _ = client
        .post(format!("{}/api/log", url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await;
}

/// A Codex thread id mentioned in a line of App Server output, such as `thread_id=…`.
pub fn thread_hint(line: &str) -> Option<String> {
    let after = &line[line.find("thread_id")? + "thread_id".len()..];
    let id: String = after
        .trim_start_matches(|c: char| "=\":{ ".contains(c))
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    (id.len() >= 8).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(ts_ms: i64, level: Level, source: Source, message: &str) -> Record {
        Record {
            ts_ms,
            level,
            source,
            event: "test".into(),
            message: message.into(),
            conversation_id: None,
            work_id: None,
            thread_id: None,
            fields: Map::new(),
        }
    }

    #[test]
    fn redaction_blanks_secrets_invites_and_bearers_in_written_lines() {
        let token = "ab".repeat(32);
        assert_eq!(
            redact(
                &format!("join https://e.example/join#vibeface1qq2ww3 failed; token {token}."),
                std::slice::from_ref(&token)
            ),
            "join https://e.example/join#[invite] failed; token [redacted]."
        );
        assert_eq!(
            redact("Authorization: Bearer sk.abc-123/x= ok", &[]),
            "Authorization: Bearer [redacted] ok"
        );
        let dir = scratch("log-redact");
        let sink = Sink::open(&dir, MAX_FILE_BYTES, 2).unwrap();
        sink.add_secret("model-key-1234567890");
        let mut leaked = record(1, Level::Info, Source::Web, "key model-key-1234567890");
        leaked.fields.insert(
            "link".into(),
            json!("https://e.example/join#vibeface1secretticket"),
        );
        sink.write(&leaked);
        let text = fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(!text.contains("model-key-1234567890") && !text.contains("vibeface1"));
        let back = read(&dir, &Filter::default()).unwrap();
        assert_eq!(back[0].message, "key [redacted]");
        assert_eq!(back[0].fields["link"], "https://e.example/join#[invite]");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotation_keeps_the_newest_n_files_under_the_size_limit() {
        let dir = scratch("log-rotate");
        let sink = Sink::open(&dir, 400, 3).unwrap();
        for index in 0..60 {
            sink.write(&record(
                index,
                Level::Info,
                Source::Node,
                &format!("line {index:02}"),
            ));
        }
        let names: Vec<_> = files(&dir);
        assert_eq!(names.len(), 3, "the current file and two rotations");
        assert!(!path(&dir, 3).exists());
        for file in &names {
            assert!(fs::metadata(file).unwrap().len() <= 400);
        }
        let kept = read(&dir, &Filter::default()).unwrap();
        assert_eq!(kept.last().unwrap().message, "line 59");
        assert!(kept.first().unwrap().ts_ms > 0, "the oldest lines are gone");
        let stamps: Vec<_> = kept.iter().map(|record| record.ts_ms).collect();
        assert!(
            stamps.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "read oldest first, without gaps"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn filters_combine_time_level_source_ids_and_text() {
        let dir = scratch("log-filter");
        let sink = Sink::open(&dir, MAX_FILE_BYTES, 2).unwrap();
        let mut tagged = record(20, Level::Warn, Source::Codex, "turn stalled");
        tagged.conversation_id = Some("c1".into());
        tagged.work_id = Some("w1".into());
        tagged
            .fields
            .insert("detail".into(), json!("Socket Closed"));
        for written in [
            record(10, Level::Error, Source::Web, "uncaught TypeError"),
            tagged,
            record(30, Level::Info, Source::Codex, "turn started"),
            record(40, Level::Debug, Source::Node, "noise"),
        ] {
            sink.write(&written);
        }
        fs::write(path(&dir, 1), "plain text from an old version\n").unwrap();
        let messages = |filter: Filter| -> Vec<String> {
            read(&dir, &filter)
                .unwrap()
                .into_iter()
                .map(|record| record.message)
                .collect()
        };
        assert_eq!(messages(Filter::default()).len(), 4);
        assert_eq!(
            messages(Filter {
                level: Some(Level::Warn),
                ..Default::default()
            }),
            ["uncaught TypeError", "turn stalled"]
        );
        assert_eq!(
            messages(Filter {
                sources: vec![Source::Codex],
                ..Default::default()
            }),
            ["turn stalled", "turn started"]
        );
        assert_eq!(
            messages(Filter {
                since_ms: Some(20),
                until_ms: Some(30),
                ..Default::default()
            }),
            ["turn stalled", "turn started"]
        );
        assert_eq!(
            messages(Filter {
                conversation_id: Some("c1".into()),
                ..Default::default()
            }),
            ["turn stalled"]
        );
        assert_eq!(
            messages(Filter {
                work_id: Some("w2".into()),
                ..Default::default()
            }),
            Vec::<String>::new()
        );
        assert_eq!(
            messages(Filter {
                text: Some("socket closed".into()),
                ..Default::default()
            }),
            ["turn stalled"]
        );
        assert_eq!(
            messages(Filter {
                limit: Some(2),
                ..Default::default()
            }),
            ["turn started", "noise"]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn submissions_are_bounded_validated_and_rate_limited() {
        let limit = RateLimit::default();
        let ok = json!({"level":"error","event":"web.uncaught","message":"boom","conversation_id":"c-1"});
        assert_eq!(
            submit(&limit, "a", Source::Web, &json!([ok]), 0).unwrap(),
            1
        );
        let big = json!({"level":"info","message":"x".repeat(MAX_RECORD_BYTES)});
        for rejected in [
            json!([big]),
            json!([]),
            json!(vec![ok.clone(); MAX_BATCH + 1]),
            json!([{"level":"loud","message":"x"}]),
            json!([{"level":"info","message":"x","event":"has space"}]),
            json!([{"level":"info","message":"x","conversation_id":"../etc"}]),
            json!([{"level":"info","message":"x","source":"node"}]),
        ] {
            assert!(
                submit(&limit, "a", Source::Web, &rejected, 0).is_err(),
                "{rejected}"
            );
        }
        let batch = json!(vec![ok.clone(); MAX_BATCH]);
        let accepted = (0..RATE_PER_MINUTE as usize / MAX_BATCH + 1)
            .filter(|_| submit(&limit, "a", Source::Web, &batch, 1).is_ok())
            .count();
        assert_eq!(
            1 + accepted * MAX_BATCH,
            1 + (RATE_PER_MINUTE as usize - 1) / MAX_BATCH * MAX_BATCH
        );
        assert!(
            submit(&limit, "b", Source::Web, &json!([ok]), 1).is_ok(),
            "per sender"
        );
        assert!(
            submit(&limit, "a", Source::Web, &json!([ok]), 60_001).is_ok(),
            "a new minute"
        );
    }

    #[test]
    fn thread_hints_read_app_server_spans() {
        assert_eq!(
            thread_hint("WARN session{thread_id=0199a2b3-c4d5-7e6f} stream error").as_deref(),
            Some("0199a2b3-c4d5-7e6f")
        );
        assert_eq!(thread_hint("no ids here"), None);
    }
}
