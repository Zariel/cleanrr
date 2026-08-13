use std::{collections::HashSet, time::Duration};

use chrono::{DateTime, Utc};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    arr::{ArrClient, ArrError, DeleteOutcome, QueueItem},
    config::{Config, ServerConfig},
    metrics::Metrics,
};

#[derive(Clone)]
struct CleanupPolicy {
    minimum_age: Duration,
    dry_run: bool,
    remove_from_client: bool,
}

impl From<&Config> for CleanupPolicy {
    fn from(config: &Config) -> Self {
        Self {
            minimum_age: config.minimum_age,
            dry_run: config.dry_run,
            remove_from_client: config.remove_from_client,
        }
    }
}

pub async fn run_cleaner(
    name: String,
    server: ServerConfig,
    config: Config,
    metrics: Metrics,
    cancellation: CancellationToken,
) {
    let kind = server.kind.as_str();
    let client = match ArrClient::new(&server, Duration::from_secs(15)) {
        Ok(client) => client,
        Err(error) => {
            error!(server = %name, kind, %error, "failed to create Arr client");
            return;
        }
    };
    let policy = CleanupPolicy::from(&config);
    let mut interval = tokio::time::interval(config.poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    info!(
        server = %name,
        kind,
        url = %server.url,
        dry_run = policy.dry_run,
        remove_from_client = policy.remove_from_client,
        "cleaner started"
    );

    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            _ = interval.tick() => {
                let started = Instant::now();
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    result = poll_once(&client, &name, kind, &policy, &metrics) => result,
                };
                match result {
                    Ok(()) => {
                        metrics.record_poll(&name, kind, "success", started.elapsed());
                        metrics.record_poll_success(&name, kind, Utc::now().timestamp());
                    }
                    Err(error) => {
                        metrics.record_poll(&name, kind, "error", started.elapsed());
                        error!(server = %name, kind, %error, "queue poll failed");
                    }
                }
            }
        }
    }

    info!(server = %name, kind, "cleaner stopped");
}

async fn poll_once(
    client: &ArrClient,
    server: &str,
    kind: &str,
    policy: &CleanupPolicy,
    metrics: &Metrics,
) -> Result<(), ArrError> {
    let items = client.queue().await?;
    metrics.add_queue_items(server, kind, items.len());
    debug!(
        server,
        kind,
        queue_items = items.len(),
        "queue poll completed"
    );

    let now = Utc::now();
    let candidates = cleanup_candidates(&items, now, policy);
    metrics.add_matches(server, kind, candidates.len());

    for item in candidates {
        let title = item.title.as_deref().unwrap_or("<unknown>");
        if policy.dry_run {
            info!(
                server,
                kind,
                queue_id = item.id,
                title,
                "would remove queue item"
            );
            metrics.record_removal(server, kind, "dry_run");
            continue;
        }

        match client
            .delete_queue_item(item.id, policy.remove_from_client)
            .await
        {
            Ok(DeleteOutcome::Removed) => {
                info!(
                    server,
                    kind,
                    queue_id = item.id,
                    title,
                    "removed queue item"
                );
                metrics.record_removal(server, kind, "removed");
            }
            Ok(DeleteOutcome::AlreadyGone) => {
                debug!(
                    server,
                    kind,
                    queue_id = item.id,
                    title,
                    "queue item already gone"
                );
                metrics.record_removal(server, kind, "already_gone");
            }
            Err(error) => {
                warn!(server, kind, queue_id = item.id, title, %error, "failed to remove queue item");
                metrics.record_removal(server, kind, "error");
            }
        }
    }

    Ok(())
}

fn is_candidate(item: &QueueItem, now: DateTime<Utc>, policy: &CleanupPolicy) -> bool {
    if item.tracked_download_state.as_deref() != Some("importBlocked") {
        return false;
    }

    let Some(added) = item.added else {
        return false;
    };
    let Ok(age) = now.signed_duration_since(added).to_std() else {
        return false;
    };
    if age < policy.minimum_age {
        return false;
    }

    true
}

fn cleanup_candidates<'a>(
    items: &'a [QueueItem],
    now: DateTime<Utc>,
    policy: &CleanupPolicy,
) -> Vec<&'a QueueItem> {
    let mut downloads = HashSet::new();
    items
        .iter()
        .filter(|item| is_candidate(item, now, policy))
        .filter(|item| downloads.insert(candidate_key(item)))
        .collect()
}

fn candidate_key(item: &QueueItem) -> String {
    item.download_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| format!("download:{}", id.to_ascii_lowercase()))
        .unwrap_or_else(|| format!("queue:{}", item.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServerConfig, ServerKind};
    use axum::{Json, Router, extract::State, routing::get};
    use chrono::TimeDelta;
    use serde_json::Value;
    use std::{collections::BTreeMap, future::pending, sync::Arc};
    use tokio::{net::TcpListener, sync::Notify};
    use url::Url;

    fn policy() -> CleanupPolicy {
        CleanupPolicy {
            minimum_age: Duration::from_secs(1800),
            dry_run: false,
            remove_from_client: false,
        }
    }

    fn item(now: DateTime<Utc>) -> QueueItem {
        QueueItem {
            id: 1,
            title: Some("release".to_owned()),
            download_id: Some("download-1".to_owned()),
            added: Some(now - TimeDelta::minutes(31)),
            tracked_download_state: Some("importBlocked".to_owned()),
        }
    }

    #[test]
    fn matches_old_blocked_import() {
        let now = Utc::now();
        assert!(is_candidate(&item(now), now, &policy()));
    }

    #[test]
    fn does_not_match_young_item() {
        let now = Utc::now();
        let mut item = item(now);
        item.added = Some(now - TimeDelta::minutes(29));
        assert!(!is_candidate(&item, now, &policy()));
    }

    #[test]
    fn does_not_match_non_blocked_download() {
        let now = Utc::now();
        let mut item = item(now);
        item.tracked_download_state = Some("downloading".to_owned());
        assert!(!is_candidate(&item, now, &policy()));
    }

    #[test]
    fn deduplicates_sonarr_episode_rows_by_download() {
        let now = Utc::now();
        let first = item(now);
        let mut second = item(now);
        second.id = 2;
        second.title = Some("another episode".to_owned());
        second.download_id = Some("DOWNLOAD-1".to_owned());

        let items = [first, second];
        let candidates = cleanup_candidates(&items, now, &policy());

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, 1);
    }

    async fn hanging_queue(State(started): State<Arc<Notify>>) -> Json<Value> {
        started.notify_one();
        pending().await
    }

    #[tokio::test]
    async fn cancellation_stops_an_active_poll() {
        let started = Arc::new(Notify::new());
        let app = Router::new()
            .route("/api/v3/queue", get(hanging_queue))
            .with_state(started.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let server = ServerConfig {
            kind: ServerKind::Sonarr,
            url: Url::parse(&format!("http://{address}")).unwrap(),
            api_key: "secret".to_owned(),
        };
        let config = Config {
            poll_interval: Duration::from_secs(3600),
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

        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("cleaner did not start polling");
        cancellation.cancel();
        tokio::time::timeout(Duration::from_millis(250), cleaner)
            .await
            .expect("cleaner did not stop promptly")
            .expect("cleaner task failed");
    }
}
