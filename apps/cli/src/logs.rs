use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{LocalNode, data_dir, default_data_dir, now_ms, post_local};

const LOG_FILE: &str = "node.log";
const MAX_LOG_RECORD: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum LogSource {
    Node,
    Web,
    Agent,
    Cli,
    Codex,
    Cursor,
}

#[derive(Args)]
pub(super) struct LogRecord {
    /// Log severity.
    level: LogLevel,
    /// Log message.
    message: String,
    /// Conversation ID.
    #[arg(long, env = "VIBEFACE_NODE_CONVERSATION")]
    conversation: Option<String>,
    /// Work ID.
    #[arg(long)]
    work: Option<String>,
    /// Provider thread ID.
    #[arg(long)]
    thread: Option<String>,
    /// Structured event name.
    #[arg(long)]
    event: Option<String>,
    /// Log source.
    #[arg(long)]
    source: Option<LogSource>,
    #[command(flatten)]
    local: LocalNode,
}

#[derive(Args)]
pub(super) struct Logs {
    /// Node data directory.
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    /// Earliest timestamp or relative age, such as 10m.
    #[arg(long)]
    since: Option<String>,
    /// Log severity.
    #[arg(long)]
    level: Option<LogLevel>,
    /// Log source. Repeat to include multiple sources.
    #[arg(long)]
    source: Vec<LogSource>,
    /// Conversation ID.
    #[arg(long)]
    conversation: Option<String>,
    /// Work ID.
    #[arg(long)]
    work: Option<String>,
    /// Keep records containing this text.
    #[arg(long)]
    grep: Option<String>,
    /// Maximum number of records to show.
    #[arg(long, default_value_t = 200)]
    limit: usize,
    /// Follow new complete records.
    #[arg(short, long)]
    follow: bool,
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

pub(super) async fn log_record(options: LogRecord) -> Result<()> {
    let source = options
        .source
        .unwrap_or(if std::env::var_os("VIBEFACE_TRIAGE_URL").is_some() {
            LogSource::Agent
        } else {
            LogSource::Cli
        });
    ensure!(
        matches!(source, LogSource::Agent | LogSource::Cli),
        "--source is agent or cli"
    );
    let body = json!({"source":source,"records":[{"level":options.level,"message":options.message,"event":options.event,"conversation_id":options.conversation,"work_id":options.work,"thread_id":options.thread}]});
    post_local(&options.local, "/api/log", &body, Duration::from_secs(10)).await?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LogValue {
    ts_ms: i64,
    level: LogLevel,
    source: LogSource,
    event: String,
    message: String,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    work_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    fields: serde_json::Map<String, Value>,
}

fn log_matches(record: &LogValue, options: &Logs, since: Option<i64>) -> bool {
    let same = |want: &Option<String>, have: &Option<String>| {
        want.as_ref().is_none_or(|want| have.as_ref() == Some(want))
    };
    since.is_none_or(|since| record.ts_ms >= since)
        && options.level.is_none_or(|level| record.level >= level)
        && (options.source.is_empty() || options.source.contains(&record.source))
        && same(&options.conversation, &record.conversation_id)
        && same(&options.work, &record.work_id)
        && options.grep.as_deref().is_none_or(|needle| {
            let needle = needle.to_lowercase();
            [
                Some(&record.message),
                Some(&record.event),
                record.conversation_id.as_ref(),
                record.work_id.as_ref(),
                record.thread_id.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|part| part.to_lowercase().contains(&needle))
                || Value::Object(record.fields.clone())
                    .to_string()
                    .to_lowercase()
                    .contains(&needle)
        })
}

pub(super) async fn logs(options: Logs) -> Result<()> {
    let current = options.data_dir.join(LOG_FILE);
    ensure!(
        current.exists(),
        "no Node log in {}; start the Node with this --data-dir first",
        options.data_dir.display()
    );
    let since = options.since.as_deref().map(since_ms).transpose()?;
    let print = |record: &LogValue| -> Result<()> {
        let line = if options.json {
            serde_json::to_string(record)?
        } else {
            log_line(record)
        };
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{line}").is_err() {
            std::process::exit(0);
        }
        Ok(())
    };
    let mut records = std::collections::VecDeque::new();
    for path in log_files(&options.data_dir) {
        let file = fs::File::open(path)?;
        for line in std::io::BufReader::new(file).split(b'\n') {
            let line = line?;
            if line.len() > MAX_LOG_RECORD {
                continue;
            }
            if let Ok(record) = serde_json::from_slice::<LogValue>(&line)
                && log_matches(&record, &options, since)
            {
                records.push_back(record);
                if records.len() > options.limit.max(1) {
                    records.pop_front();
                }
            }
        }
    }
    for record in &records {
        print(record)?;
    }
    if !options.follow {
        return Ok(());
    }
    let mut offset = fs::metadata(&current).map(|m| m.len()).unwrap_or(0);
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some(len) = fs::metadata(&current).ok().map(|m| m.len()) else {
            continue;
        };
        if len < offset {
            offset = 0;
        }
        if len == offset {
            continue;
        }
        let mut file = fs::File::open(&current)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        offset += complete as u64;
        for line in bytes[..complete].split(|byte| *byte == b'\n') {
            if line.len() <= MAX_LOG_RECORD
                && let Ok(record) = serde_json::from_slice::<LogValue>(line)
                && log_matches(&record, &options, since)
            {
                print(&record)?;
            }
        }
    }
}

fn log_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for index in (1..=4).rev() {
        let path = dir.join(format!("{LOG_FILE}.{index}"));
        if path.exists() {
            files.push(path);
        }
    }
    files.push(dir.join(LOG_FILE));
    files
}

fn log_line(record: &LogValue) -> String {
    let mut line = format!(
        "{} {:<5} {:<6} {} {}",
        utc_time(record.ts_ms),
        format!("{:?}", record.level).to_uppercase(),
        format!("{:?}", record.source).to_lowercase(),
        record.event,
        record.message
    );
    for (name, id) in [
        ("conversation", &record.conversation_id),
        ("work", &record.work_id),
        ("thread", &record.thread_id),
    ] {
        if let Some(id) = id {
            line.push_str(&format!(" {name}={id}"));
        }
    }
    if !record.fields.is_empty() {
        line.push(' ');
        line.push_str(&Value::Object(record.fields.clone()).to_string());
    }
    line
}

fn utc_time(ms: i64) -> String {
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

fn since_ms(text: &str) -> Result<i64> {
    let trimmed = text.trim();
    let number_end = trimmed.trim_end_matches(char::is_alphabetic).len();
    let (number, unit) = trimmed.split_at(number_end);
    let number: i64 = number
        .parse()
        .context("--since is like 30m, 2h, 1d or a time in ms")?;
    let unit_ms = match unit {
        "" => return Ok(number),
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => bail!("--since unit is s, m, h or d"),
    };
    Ok(now_ms() - number * unit_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Logs {
        Logs {
            data_dir: PathBuf::from("/tmp/vibeface-logs-test"),
            since: None,
            level: None,
            source: Vec::new(),
            conversation: None,
            work: None,
            grep: None,
            limit: 200,
            follow: false,
            json: false,
        }
    }

    #[test]
    fn since_ms_accepts_absolute_and_relative_values() {
        assert_eq!(since_ms(" 123 ").unwrap(), 123);
        let before = now_ms();
        let value = since_ms("2s").unwrap();
        let after = now_ms();
        assert!((before - 2_000..=after - 1_900).contains(&value));
        assert!(since_ms("1x").is_err());
    }

    #[test]
    fn filtering_and_human_formatting_keep_structured_fields() {
        let record = LogValue {
            ts_ms: 0,
            level: LogLevel::Error,
            source: LogSource::Cli,
            event: "request".into(),
            message: "Hello world".into(),
            conversation_id: Some("conversation-1".into()),
            work_id: Some("work-1".into()),
            thread_id: None,
            fields: serde_json::Map::from_iter([(String::from("attempt"), Value::from(2))]),
        };
        let mut filters = options();
        filters.level = Some(LogLevel::Warn);
        filters.source = vec![LogSource::Cli];
        filters.grep = Some("HELLO".into());
        filters.conversation = Some("conversation-1".into());
        assert!(log_matches(&record, &filters, Some(0)));
        assert_eq!(
            log_line(&record),
            "1970-01-01T00:00:00.000Z ERROR cli    request Hello world conversation=conversation-1 work=work-1 {\"attempt\":2}"
        );
    }
}
