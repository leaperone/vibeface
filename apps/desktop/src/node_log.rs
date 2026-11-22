//! The desktop app's own errors and panics, written to the local Node's `node.log` through
//! `/api/log` as source `desktop`, like `vibeface log`. Without a connected local Node they only
//! reach stderr.
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Duration,
};

static NODE: OnceLock<Mutex<Option<(PathBuf, tokio::runtime::Handle)>>> = OnceLock::new();

fn node() -> std::sync::MutexGuard<'static, Option<(PathBuf, tokio::runtime::Handle)>> {
    NODE.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The local Node this app reads, by its data directory; later records go to it.
pub fn attach(data_dir: PathBuf) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        *node() = Some((data_dir, handle));
    }
}

fn body(level: &str, event: &str, message: &str) -> Value {
    let message: String = message.chars().take(1500).collect();
    json!({"source":"desktop","records":[{"level":level,"event":event,"message":message}]})
}

async fn post(data_dir: PathBuf, body: Value) -> anyhow::Result<()> {
    let credential = vibeface_control::LocalHttpCredential::read(&data_dir)?;
    let url = reqwest::Url::parse(&credential.origin)?;
    credential.require_origin(&url)?;
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(url.join("api/log")?)
        .bearer_auth(&credential.capability)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Sends one error to the attached Node without waiting for it.
pub fn error(event: &str, message: &str) {
    let Some((data_dir, handle)) = node().clone() else {
        return;
    };
    let body = body("error", event, message);
    handle.spawn(async move {
        if let Err(error) = post(data_dir, body).await {
            eprintln!("Vibeface desktop log: {error:#}");
        }
    });
}

/// Records every panic as `desktop.panic` before the default report, waiting up to 2 s.
pub fn record_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic without a message".into());
        let location = info
            .location()
            .map(|at| format!(" at {}:{}", at.file(), at.line()))
            .unwrap_or_default();
        if let Some((data_dir, _)) = node().clone() {
            let body = body("error", "desktop.panic", &format!("{message}{location}"));
            let (done, wait) = std::sync::mpsc::channel();
            let _ = std::thread::Builder::new().spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    let _ = runtime.block_on(post(data_dir, body));
                }
                let _ = done.send(());
            });
            let _ = wait.recv_timeout(Duration::from_secs(2));
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn records_name_the_desktop_and_stay_bounded() {
        let body = super::body("error", "desktop.error", &"x".repeat(9000));
        assert_eq!(body["source"], "desktop");
        assert_eq!(body["records"][0]["event"], "desktop.error");
        assert_eq!(body["records"][0]["message"].as_str().unwrap().len(), 1500);
    }
}
