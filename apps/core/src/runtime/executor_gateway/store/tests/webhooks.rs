use super::*;
use crate::triage;
use std::sync::Arc;

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn signed_caller_destination_dedup_retry_then_4xx_dead_letter() -> Result<()> {
    let received = Arc::new(Mutex::new(
        Vec::<(String, axum::http::HeaderMap, String)>::new(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let sink = received.clone();
    let _server = Server(tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/{destination}",
            axum::routing::post(
                move |axum::extract::Path(destination): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap,
                      body: String| {
                    let sink = sink.clone();
                    async move {
                        let n = {
                            let mut received = sink.lock().unwrap();
                            received.push((destination, headers, body));
                            received.len()
                        };
                        if n == 1 {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            axum::http::StatusCode::BAD_REQUEST
                        }
                    }
                },
            ),
        );
        axum::serve(listener, app).await.unwrap();
    }));
    let f = Fixture::new()?;
    let secret = "fixture-webhook-secret-0123456789";
    let minted = f
        .store
        .mint(false, Some(&format!("{base}/caller")), Some(secret))?;
    let caller = f.store.authenticate(minted["key"].as_str().unwrap())?;
    let _decoy = f
        .store
        .mint(false, Some(&format!("{base}/other")), Some("other-secret"))?;
    let work = gateway_spec(&caller, "webhook");
    f.store.accept(&caller, &work, false, None)?;
    start(&f.store, &work.work_id, "webhook-thread")?;
    for _ in 0..2 {
        f.store.signal(
            &work.work_id,
            Signal::Progress {
                summary: "needs_input".into(),
                at_ms: now_ms(),
            },
        )?;
    }
    let event_id = format!("needs_input-{}", work.work_id);
    let queued: i64 =
        f.store
            .connection()?
            .query_row("SELECT count(*) FROM gateway_outbox", [], |row| row.get(0))?;
    assert_eq!(queued, 1);
    // Even a future destination change must not redirect an already queued event.
    f.store.connection()?.execute(
        "UPDATE gateway_callers SET webhook_url=?,webhook_secret=? WHERE caller_id=?",
        params![
            format!("{base}/other"),
            b"changed-secret".as_slice(),
            caller.id
        ],
    )?;
    f.store.deliver().await?;
    let (state, attempts, next): (String, u32, i64) = f.store.connection()?.query_row(
        "SELECT state,attempts,next_ms FROM gateway_outbox WHERE event_id=?",
        [&event_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!((state.as_str(), attempts), ("pending", 1));
    assert!(next > now_ms());
    f.store.deliver().await?;
    assert_eq!(
        received.lock().unwrap().len(),
        1,
        "backoff must delay retry"
    );
    f.store
        .connection()?
        .execute("UPDATE gateway_outbox SET next_ms=0", [])?;
    f.store.deliver().await?;
    f.store.deliver().await?;
    let (state, attempts): (String, u32) = f.store.connection()?.query_row(
        "SELECT state,attempts FROM gateway_outbox WHERE event_id=?",
        [&event_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!((state.as_str(), attempts), ("dead", 2));
    assert!(
        attempts < triage::MAX_ATTEMPTS,
        "400 ends delivery before exhausting retries"
    );
    let received = received.lock().unwrap();
    assert_eq!(received.len(), 2);
    for (destination, headers, body) in received.iter() {
        assert_eq!(destination, "caller");
        let timestamp: i64 = headers["x-vibeface-timestamp"].to_str()?.parse()?;
        assert_eq!(
            headers["x-vibeface-signature"].to_str()?,
            triage::signature(secret.as_bytes(), timestamp, body)
        );
        assert_eq!(headers["x-vibeface-event-id"].to_str()?, event_id);
        let callback: Value = serde_json::from_str(body)?;
        assert_eq!(callback["type"], "needs_input");
        assert_eq!(callback["work"]["state"], "needs_input");
        assert!(!body.contains(secret));
        assert!(!body.contains(minted["key"].as_str().unwrap()));
        assert!(!body.contains("other-secret"));
    }
    drop(received);
    finish(&f.store, &work.work_id)?;
    let terminal: String = f.store.connection()?.query_row(
        "SELECT body FROM gateway_outbox WHERE event_id LIKE 'terminal-%'",
        [],
        |row| row.get(0),
    )?;
    let terminal: Value = serde_json::from_str(&terminal)?;
    assert_eq!(terminal["work"]["final_reply"], "done");
    assert_eq!(terminal["work"]["sandbox"], "read-only");
    assert_eq!(terminal["work"]["state"], "finished");
    Ok(())
}
