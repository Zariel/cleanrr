use std::{collections::HashSet, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::OnceCell;
use url::Url;

use crate::config::ServerConfig;

const PAGE_SIZE: usize = 100;

#[derive(Clone)]
pub struct ArrClient {
    client: Client,
    base_url: Url,
    api_key: String,
    api_base_url: Arc<OnceCell<Url>>,
}

#[derive(Debug, Deserialize)]
struct ApiInfo {
    current: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: i64,
    pub title: Option<String>,
    pub download_id: Option<String>,
    pub added: Option<DateTime<Utc>>,
    pub status: Option<String>,
    pub tracked_download_status: Option<String>,
    pub tracked_download_state: Option<String>,
    #[serde(default)]
    pub status_messages: Vec<QueueStatusMessage>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct QueueStatusMessage {
    #[serde(default)]
    pub messages: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueuePage {
    total_records: usize,
    #[serde(default)]
    records: Vec<QueueItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    Removed,
    AlreadyGone,
}

#[derive(Debug, Error)]
pub enum ArrError {
    #[error("invalid server URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("could not build HTTP client: {0}")]
    BuildClient(#[source] reqwest::Error),
    #[error("request to {operation} failed: {source}")]
    Request {
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{operation} returned HTTP {status}: {body}")]
    Http {
        operation: &'static str,
        status: StatusCode,
        body: String,
    },
    #[error("queue pagination made no progress after {records} records")]
    Pagination { records: usize },
    #[error("unsupported Arr API version: {0}")]
    UnsupportedApiVersion(String),
}

impl ArrClient {
    pub fn new(server: &ServerConfig, timeout: Duration) -> Result<Self, ArrError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(concat!("cleanrr/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ArrError::BuildClient)?;

        let mut base_url = server.url.clone();
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }

        Ok(Self {
            client,
            base_url,
            api_key: server.api_key.clone(),
            api_base_url: Arc::new(OnceCell::new()),
        })
    }

    async fn api_base_url(&self) -> Result<&Url, ArrError> {
        // Queue IDs differ between API versions. Keep reads and removals on
        // the same API for the lifetime of this client.
        self.api_base_url
            .get_or_try_init(|| async {
                let response = self
                    .client
                    .get(self.base_url.join("api")?)
                    .header("X-Api-Key", &self.api_key)
                    .send()
                    .await
                    .map_err(|source| ArrError::Request {
                        operation: "fetch API version",
                        source,
                    })?;

                if !response.status().is_success() {
                    return Err(http_error("fetch API version", response).await);
                }

                let info: ApiInfo = response.json().await.map_err(|source| ArrError::Request {
                    operation: "decode API version",
                    source,
                })?;
                if !matches!(info.current.as_str(), "v3" | "v5") {
                    return Err(ArrError::UnsupportedApiVersion(info.current));
                }

                Ok(self.base_url.join(&format!("api/{}/", info.current))?)
            })
            .await
    }

    pub async fn queue(&self) -> Result<Vec<QueueItem>, ArrError> {
        let url = self.api_base_url().await?.join("queue")?;
        let mut page_number = 1usize;
        let mut items = Vec::new();
        let mut seen_ids = HashSet::new();

        loop {
            let response = self
                .client
                .get(url.clone())
                .header("X-Api-Key", &self.api_key)
                .query(&[
                    ("page", page_number.to_string()),
                    ("pageSize", PAGE_SIZE.to_string()),
                    ("sortKey", "added".to_owned()),
                    ("sortDirection", "ascending".to_owned()),
                ])
                .send()
                .await
                .map_err(|source| ArrError::Request {
                    operation: "fetch queue",
                    source,
                })?;

            if !response.status().is_success() {
                return Err(http_error("fetch queue", response).await);
            }

            let page: QueuePage = response.json().await.map_err(|source| ArrError::Request {
                operation: "decode queue",
                source,
            })?;
            let page_len = page.records.len();
            for item in page.records {
                if seen_ids.insert(item.id) {
                    items.push(item);
                }
            }

            if items.len() >= page.total_records || page_len == 0 {
                return Ok(items);
            }
            if page_len < PAGE_SIZE {
                return Err(ArrError::Pagination {
                    records: items.len(),
                });
            }
            page_number += 1;
        }
    }

    pub async fn delete_queue_item(
        &self,
        id: i64,
        remove_from_client: bool,
    ) -> Result<DeleteOutcome, ArrError> {
        let url = self.api_base_url().await?.join(&format!("queue/{id}"))?;
        let removal_method = if remove_from_client {
            "removeFromClient"
        } else {
            "ignore"
        };
        // Newer Sonarr v5 builds require removalMethod and blocklistMethod.
        // Older builds ignore them and default removeFromClient to true, so
        // the legacy flags must still be sent.
        let response = self
            .client
            .delete(url)
            .header("X-Api-Key", &self.api_key)
            .query(&[
                ("removeFromClient", remove_from_client),
                ("blocklist", false),
                ("skipRedownload", false),
                ("changeCategory", false),
            ])
            .query(&[
                ("removalMethod", removal_method),
                ("blocklistMethod", "doNotBlocklist"),
            ])
            .send()
            .await
            .map_err(|source| ArrError::Request {
                operation: "delete queue item",
                source,
            })?;

        match response.status() {
            status if status.is_success() => Ok(DeleteOutcome::Removed),
            StatusCode::NOT_FOUND => Ok(DeleteOutcome::AlreadyGone),
            _ => Err(http_error("delete queue item", response).await),
        }
    }
}

async fn http_error(operation: &'static str, response: reqwest::Response) -> ArrError {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<response body unavailable>".to_owned());
    let mut body: String = body.chars().take(512).collect();
    if body.is_empty() {
        body = "<empty response>".to_owned();
    }
    ArrError::Http {
        operation,
        status,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{delete, get},
    };
    use serde_json::{Value, json};
    use std::{collections::HashMap, sync::Arc};
    use tokio::{net::TcpListener, sync::Mutex};

    type DeleteRequest = (i64, HashMap<String, String>, String);

    #[derive(Clone, Default)]
    struct TestState {
        deletes: Arc<Mutex<Vec<DeleteRequest>>>,
        api_info: Arc<Mutex<(StatusCode, Value)>>,
    }

    async fn api_info_handler(
        State(state): State<TestState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(headers["X-Api-Key"], "test-secret");
        let (status, info) = state.api_info.lock().await.clone();
        (status, Json(info))
    }

    async fn queue_handler(
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> Json<Value> {
        assert_eq!(headers["X-Api-Key"], "test-secret");
        assert_eq!(query["pageSize"], PAGE_SIZE.to_string());
        assert_eq!(query["sortKey"], "added");
        assert_eq!(query["sortDirection"], "ascending");
        let page: usize = query["page"].parse().unwrap();
        let records = if page == 1 {
            (0..100)
                .map(|id| json!({"id": id, "title": format!("item-{id}")}))
                .collect::<Vec<_>>()
        } else {
            vec![json!({
                "id": 100,
                "episodeIds": [10, 11],
                "seasonNumbers": [1],
                "title": "candidate",
                "downloadId": "download-100",
                "added": "2026-01-01T00:00:00Z",
                "status": "completed",
                "trackedDownloadStatus": "warning",
                "trackedDownloadState": "importBlocked",
                "statusMessages": [{
                    "title": "candidate.mkv",
                    "messages": ["Not a Custom Format upgrade"]
                }]
            })]
        };
        Json(json!({"totalRecords": 101, "records": records}))
    }

    async fn delete_handler(
        State(state): State<TestState>,
        Path(id): Path<i64>,
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> StatusCode {
        let key = headers
            .get("X-Api-Key")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        state.deletes.lock().await.push((id, query, key));
        if id == 404 {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::NO_CONTENT
        }
    }

    async fn test_client(version: &str) -> (ArrClient, TestState) {
        let state = TestState {
            api_info: Arc::new(Mutex::new((
                StatusCode::OK,
                json!({"current": version, "deprecated": []}),
            ))),
            ..TestState::default()
        };
        let app = Router::new()
            .route("/base/api", get(api_info_handler))
            .route(&format!("/base/api/{version}/queue"), get(queue_handler))
            .route(
                &format!("/base/api/{version}/queue/{{id}}"),
                delete(delete_handler),
            )
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let server = ServerConfig {
            url: Url::parse(&format!("http://{address}/base")).unwrap(),
            api_key: "test-secret".to_owned(),
        };
        (
            ArrClient::new(&server, Duration::from_secs(2)).unwrap(),
            state,
        )
    }

    #[tokio::test]
    async fn paginates_queue_and_preserves_base_path() {
        for version in ["v3", "v5"] {
            let (client, _) = test_client(version).await;
            let items = client.queue().await.unwrap();

            assert_eq!(items.len(), 101, "{version}");
            let candidate = items.last().unwrap();
            assert_eq!(candidate.id, 100);
            assert_eq!(candidate.download_id.as_deref(), Some("download-100"));
            assert_eq!(candidate.title.as_deref(), Some("candidate"));
            assert_eq!(candidate.status.as_deref(), Some("completed"));
            assert_eq!(
                candidate.tracked_download_status.as_deref(),
                Some("warning")
            );
            assert_eq!(
                candidate.tracked_download_state.as_deref(),
                Some("importBlocked")
            );
            assert_eq!(
                candidate.added.unwrap().to_rfc3339(),
                "2026-01-01T00:00:00+00:00"
            );
            assert_eq!(
                candidate.status_messages[0].messages,
                ["Not a Custom Format upgrade"]
            );
        }
    }

    #[tokio::test]
    async fn deletion_makes_all_safety_parameters_explicit() {
        for version in ["v3", "v5"] {
            for remove_from_client in [false, true] {
                let (client, state) = test_client(version).await;
                assert_eq!(
                    client
                        .delete_queue_item(42, remove_from_client)
                        .await
                        .unwrap(),
                    DeleteOutcome::Removed
                );

                let deletes = state.deletes.lock().await;
                let (id, query, key) = &deletes[0];
                assert_eq!(*id, 42);
                assert_eq!(key, "test-secret");
                assert_eq!(query["removeFromClient"], remove_from_client.to_string());
                assert_eq!(query["blocklist"], "false");
                assert_eq!(query["skipRedownload"], "false");
                assert_eq!(query["changeCategory"], "false");
                assert_eq!(
                    query["removalMethod"],
                    if remove_from_client {
                        "removeFromClient"
                    } else {
                        "ignore"
                    }
                );
                assert_eq!(query["blocklistMethod"], "doNotBlocklist");
            }
        }
    }

    #[tokio::test]
    async fn keeps_queue_ids_on_the_discovered_api() {
        let (client, state) = test_client("v5").await;
        let cloned = client.clone();
        let items = client.queue().await.unwrap();
        *state.api_info.lock().await = (StatusCode::OK, json!({"current": "v3"}));

        assert_eq!(
            cloned
                .delete_queue_item(items[100].id, false)
                .await
                .unwrap(),
            DeleteOutcome::Removed
        );
        assert_eq!(client.queue().await.unwrap().len(), 101);
    }

    #[tokio::test]
    async fn missing_queue_item_is_already_gone() {
        for version in ["v3", "v5"] {
            let (client, _) = test_client(version).await;
            assert_eq!(
                client.delete_queue_item(404, false).await.unwrap(),
                DeleteOutcome::AlreadyGone
            );
        }
    }

    #[tokio::test]
    async fn retries_failed_discovery() {
        let (client, state) = test_client("v5").await;
        *state.api_info.lock().await = (StatusCode::SERVICE_UNAVAILABLE, json!({}));

        assert!(matches!(
            client.queue().await,
            Err(ArrError::Http {
                operation: "fetch API version",
                status: StatusCode::SERVICE_UNAVAILABLE,
                ..
            })
        ));

        *state.api_info.lock().await = (StatusCode::OK, json!({"current": "v5"}));
        assert_eq!(client.queue().await.unwrap().len(), 101);
    }

    #[tokio::test]
    async fn rejects_invalid_discovery() {
        let (client, state) = test_client("v5").await;
        *state.api_info.lock().await = (StatusCode::OK, json!({}));
        assert!(matches!(
            client.queue().await,
            Err(ArrError::Request {
                operation: "decode API version",
                ..
            })
        ));

        *state.api_info.lock().await = (StatusCode::OK, json!({"current": "v99"}));
        assert!(matches!(
            client.delete_queue_item(42, false).await,
            Err(ArrError::UnsupportedApiVersion(version)) if version == "v99"
        ));
        assert!(state.deletes.lock().await.is_empty());
    }
}
