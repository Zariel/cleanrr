use std::{collections::HashSet, time::Duration};

use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::config::ServerConfig;

const PAGE_SIZE: usize = 100;

#[derive(Clone)]
pub struct ArrClient {
    client: Client,
    base_url: Url,
    api_key: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: i64,
    pub title: Option<String>,
    pub added: Option<DateTime<Utc>>,
    pub tracked_download_state: Option<String>,
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
        })
    }

    pub async fn queue(&self) -> Result<Vec<QueueItem>, ArrError> {
        let url = self.base_url.join("api/v3/queue")?;
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
        let url = self.base_url.join(&format!("api/v3/queue/{id}"))?;
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
    }

    async fn queue_handler(Query(query): Query<HashMap<String, String>>) -> Json<Value> {
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
                "title": "candidate",
                "added": "2026-01-01T00:00:00Z",
                "trackedDownloadState": "importBlocked"
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
        StatusCode::OK
    }

    async fn test_client() -> (ArrClient, TestState) {
        let state = TestState::default();
        let app = Router::new()
            .route("/base/api/v3/queue", get(queue_handler))
            .route("/base/api/v3/queue/{id}", delete(delete_handler))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let server = ServerConfig {
            kind: crate::config::ServerKind::Radarr,
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
        let (client, _) = test_client().await;
        let items = client.queue().await.unwrap();
        assert_eq!(items.len(), 101);
        assert_eq!(items.last().unwrap().title.as_deref(), Some("candidate"));
    }

    #[tokio::test]
    async fn deletion_makes_all_safety_parameters_explicit() {
        let (client, state) = test_client().await;
        assert_eq!(
            client.delete_queue_item(42, false).await.unwrap(),
            DeleteOutcome::Removed
        );
        let deletes = state.deletes.lock().await;
        let (id, query, key) = &deletes[0];
        assert_eq!(*id, 42);
        assert_eq!(key, "test-secret");
        assert_eq!(query["removeFromClient"], "false");
        assert_eq!(query["blocklist"], "false");
        assert_eq!(query["skipRedownload"], "false");
        assert_eq!(query["changeCategory"], "false");
    }
}
