use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use chrono::{TimeDelta, Utc};
use cleanrr::{
    config::{Config, ServerConfig},
    metrics::Metrics,
    service::run_cleaner,
};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify},
};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
struct QueueState {
    records: Arc<Mutex<BTreeMap<i64, Value>>>,
    polls: Arc<Mutex<usize>>,
    polled: Arc<Notify>,
}

async fn queue(State(state): State<QueueState>, headers: HeaderMap) -> Json<Value> {
    assert_eq!(headers["X-Api-Key"], "test-secret");
    let records = state.records.lock().await;
    let mut polls = state.polls.lock().await;
    *polls += 1;
    if *polls == 3 {
        state.polled.notify_one();
    }

    Json(json!({
        "totalRecords": records.len(),
        "records": records.values().collect::<Vec<_>>()
    }))
}

async fn remove(
    State(state): State<QueueState>,
    Path(id): Path<i64>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> StatusCode {
    assert_eq!(headers["X-Api-Key"], "test-secret");
    assert_eq!(query["removeFromClient"], "false");
    assert_eq!(query["blocklist"], "false");
    assert_eq!(query["skipRedownload"], "false");
    assert_eq!(query["changeCategory"], "false");

    let mut records = state.records.lock().await;
    if records.remove(&id).is_none() {
        return StatusCode::NOT_FOUND;
    }
    StatusCode::NO_CONTENT
}

#[tokio::test]
async fn cleans_v5_queue() {
    let old = (Utc::now() - TimeDelta::hours(1)).to_rfc3339();
    let records = BTreeMap::from([
        (
            1,
            json!({
                "id": 1,
                "episodeIds": [10, 11],
                "seasonNumbers": [1],
                "downloadId": "season-pack",
                "title": "Season pack",
                "added": old,
                "status": "completed",
                "trackedDownloadStatus": "warning",
                "trackedDownloadState": "importBlocked"
            }),
        ),
        (
            2,
            json!({
                "id": 2,
                "episodeIds": [12],
                "downloadId": "rejected-upgrade",
                "added": old,
                "status": "completed",
                "trackedDownloadStatus": "warning",
                "trackedDownloadState": "importPending",
                "statusMessages": [{"messages": [
                    "Not a Custom Format upgrade for existing episode file(s). New: [WEB] (10) do not improve on Existing: [WEB, HDR] (20)"
                ]}]
            }),
        ),
        (
            3,
            json!({
                "id": 3,
                "added": old,
                "status": "completed",
                "trackedDownloadStatus": "warning",
                "trackedDownloadState": "importPending",
                "statusMessages": [{"messages": ["No files found are eligible for import"]}]
            }),
        ),
        (
            4,
            json!({
                "id": 4,
                "added": Utc::now().to_rfc3339(),
                "status": "completed",
                "trackedDownloadStatus": "warning",
                "trackedDownloadState": "importBlocked"
            }),
        ),
        (
            5,
            json!({
                "id": 5,
                "status": "completed",
                "trackedDownloadStatus": "warning",
                "trackedDownloadState": "importBlocked"
            }),
        ),
    ]);
    let state = QueueState {
        records: Arc::new(Mutex::new(records)),
        polls: Arc::new(Mutex::new(0)),
        polled: Arc::new(Notify::new()),
    };
    let app = Router::new()
        .route(
            "/sonarr/api",
            get(|| async { Json(json!({"current": "v5", "deprecated": ["v3"]})) }),
        )
        .route("/sonarr/api/v5/queue", get(queue))
        .route("/sonarr/api/v5/queue/{id}", axum::routing::delete(remove))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mock = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let server = ServerConfig {
        url: Url::parse(&format!("http://{address}/sonarr")).unwrap(),
        api_key: "test-secret".to_owned(),
    };
    let config = Config {
        poll_interval: Duration::from_millis(10),
        servers: BTreeMap::from([("tv".to_owned(), server.clone())]),
        ..Config::default()
    };
    let cancellation = CancellationToken::new();
    let cleaner = tokio::spawn(run_cleaner(
        "tv".to_owned(),
        server,
        config,
        Metrics::new(),
        cancellation.clone(),
    ));

    // A third fetch means two cleanup polls finished, including their removals.
    let result = tokio::time::timeout(Duration::from_secs(2), state.polled.notified()).await;
    cancellation.cancel();
    cleaner.await.unwrap();
    mock.abort();
    result.expect("cleaner did not poll the v5 queue");

    let records = state.records.lock().await;
    assert_eq!(records.keys().copied().collect::<Vec<_>>(), [3, 4, 5]);
}
