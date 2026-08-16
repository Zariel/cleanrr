use std::{
    collections::{HashMap, HashSet},
    error::Error,
    time::Duration,
};

use chrono::{DateTime, Utc};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    arr::{ArrClient, ArrError, DeleteOutcome, QueueItem},
    config::{Config, ServerConfig},
    metrics::Metrics,
};

const SONARR_V4_NOT_CUSTOM_FORMAT_UPGRADE: &str =
    "Not a Custom Format upgrade for existing episode file(s).";

#[derive(Clone)]
struct CleanupPolicy {
    minimum_age: Duration,
    dry_run: bool,
    remove_from_client: bool,
}

#[derive(Default)]
struct CandidateTracker {
    first_seen: HashMap<String, DateTime<Utc>>,
}

impl CandidateTracker {
    fn candidates<'a>(
        &mut self,
        items: &'a [QueueItem],
        now: DateTime<Utc>,
        policy: &CleanupPolicy,
    ) -> Vec<&'a QueueItem> {
        let active_without_timestamp = items
            .iter()
            .filter(|item| has_cleanup_state(item) && item.added.is_none())
            .map(candidate_key)
            .collect::<HashSet<_>>();
        self.first_seen
            .retain(|key, _| active_without_timestamp.contains(key));

        let mut downloads = HashSet::new();
        let mut candidates = Vec::new();
        for item in items {
            if self.is_candidate(item, now, policy) {
                let key = candidate_key(item);
                if downloads.insert(key) {
                    candidates.push(item);
                }
            }
        }
        candidates
    }

    fn is_candidate(
        &mut self,
        item: &QueueItem,
        now: DateTime<Utc>,
        policy: &CleanupPolicy,
    ) -> bool {
        if !has_cleanup_state(item) {
            return false;
        }

        let eligible_since = item
            .added
            .unwrap_or_else(|| *self.first_seen.entry(candidate_key(item)).or_insert(now));
        let Ok(age) = now.signed_duration_since(eligible_since).to_std() else {
            return false;
        };

        age >= policy.minimum_age
    }
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
    let client = match ArrClient::new(&server, Duration::from_secs(15)) {
        Ok(client) => client,
        Err(error) => {
            error!(
                server = %name,
                error = %format_error_chain(&error),
                "failed to create Arr client"
            );
            return;
        }
    };
    let policy = CleanupPolicy::from(&config);
    let mut candidate_tracker = CandidateTracker::default();
    let mut interval = tokio::time::interval(config.poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    info!(
        server = %name,
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
                    result = poll_once(
                        &client,
                        &name,
                        &policy,
                        &metrics,
                        &mut candidate_tracker,
                    ) => result,
                };
                match result {
                    Ok(()) => {
                        metrics.record_poll(&name, "success", started.elapsed());
                        metrics.record_poll_success(&name, Utc::now().timestamp());
                    }
                    Err(error) => {
                        metrics.record_poll(&name, "error", started.elapsed());
                        error!(
                            server = %name,
                            error = %format_error_chain(&error),
                            "queue poll failed"
                        );
                    }
                }
            }
        }
    }

    info!(server = %name, "cleaner stopped");
}

async fn poll_once(
    client: &ArrClient,
    server: &str,
    policy: &CleanupPolicy,
    metrics: &Metrics,
    candidate_tracker: &mut CandidateTracker,
) -> Result<(), ArrError> {
    let items = client.queue().await?;
    metrics.add_queue_items(server, items.len());
    debug!(server, queue_items = items.len(), "queue poll completed");

    let now = Utc::now();
    let candidates = candidate_tracker.candidates(&items, now, policy);
    metrics.add_matches(server, candidates.len());

    for item in candidates {
        let title = item.title.as_deref().unwrap_or("<unknown>");
        if policy.dry_run {
            info!(server, queue_id = item.id, title, "would remove queue item");
            metrics.record_removal(server, "dry_run");
            continue;
        }

        match client
            .delete_queue_item(item.id, policy.remove_from_client)
            .await
        {
            Ok(DeleteOutcome::Removed) => {
                info!(server, queue_id = item.id, title, "removed queue item");
                metrics.record_removal(server, "removed");
            }
            Ok(DeleteOutcome::AlreadyGone) => {
                debug!(server, queue_id = item.id, title, "queue item already gone");
                metrics.record_removal(server, "already_gone");
            }
            Err(error) => {
                warn!(
                    server,
                    queue_id = item.id,
                    title,
                    error = %format_error_chain(&error),
                    "failed to remove queue item"
                );
                metrics.record_removal(server, "error");
            }
        }
    }

    Ok(())
}

fn format_error_chain(error: &(dyn Error + 'static)) -> String {
    let mut formatted = error.to_string();
    let mut source = error.source();

    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !formatted.ends_with(&cause_text) {
            formatted.push_str(": ");
            formatted.push_str(&cause_text);
        }
        source = cause.source();
    }

    formatted
}

fn has_cleanup_state(item: &QueueItem) -> bool {
    if item.tracked_download_state.as_deref() == Some("importBlocked") {
        return true;
    }

    // Sonarr v4 leaves this single rejected-import case in importPending. Keep
    // the compatibility match narrow because importPending is otherwise a
    // normal transient state and must not be treated as blocked.
    item.status.as_deref() == Some("completed")
        && item.tracked_download_status.as_deref() == Some("warning")
        && item.tracked_download_state.as_deref() == Some("importPending")
        && item.status_messages.iter().any(|status| {
            status
                .messages
                .iter()
                .any(|message| message.starts_with(SONARR_V4_NOT_CUSTOM_FORMAT_UPGRADE))
        })
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
    use crate::config::ServerConfig;
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
            status: Some("completed".to_owned()),
            tracked_download_status: Some("warning".to_owned()),
            tracked_download_state: Some("importBlocked".to_owned()),
            status_messages: Vec::new(),
        }
    }

    #[test]
    fn matches_old_blocked_import() {
        let now = Utc::now();
        let items = [item(now)];
        let candidates = CandidateTracker::default().candidates(&items, now, &policy());
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn does_not_match_young_item() {
        let now = Utc::now();
        let mut item = item(now);
        item.added = Some(now - TimeDelta::minutes(29));
        let items = [item];
        let candidates = CandidateTracker::default().candidates(&items, now, &policy());
        assert!(candidates.is_empty());
    }

    #[test]
    fn does_not_match_non_blocked_download() {
        let now = Utc::now();
        let mut item = item(now);
        item.tracked_download_state = Some("downloading".to_owned());
        let items = [item];
        let candidates = CandidateTracker::default().candidates(&items, now, &policy());
        assert!(candidates.is_empty());
    }

    #[test]
    fn matches_sonarr_v4_custom_format_rejection() {
        let now = Utc::now();
        let mut item = item(now);
        item.tracked_download_state = Some("importPending".to_owned());
        item.status_messages = vec![crate::arr::QueueStatusMessage {
            messages: vec![
                "Not a Custom Format upgrade for existing episode file(s). New: [HDTV] (10) do not improve on Existing: [WEB] (20)"
                    .to_owned(),
            ],
        }];

        let items = [item];
        let candidates = CandidateTracker::default().candidates(&items, now, &policy());
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn sonarr_v4_match_requires_every_typed_field_and_reason() {
        let now = Utc::now();
        let mut item = item(now);
        item.tracked_download_state = Some("importPending".to_owned());
        item.status_messages = vec![crate::arr::QueueStatusMessage {
            messages: vec![SONARR_V4_NOT_CUSTOM_FORMAT_UPGRADE.to_owned()],
        }];

        item.tracked_download_state = Some("downloading".to_owned());
        let mut tracker = CandidateTracker::default();
        assert!(
            tracker
                .candidates(&[item.clone()], now, &policy())
                .is_empty()
        );
        item.tracked_download_state = Some("importPending".to_owned());

        item.status = Some("downloading".to_owned());
        assert!(
            tracker
                .candidates(&[item.clone()], now, &policy())
                .is_empty()
        );
        item.status = Some("completed".to_owned());

        item.tracked_download_status = Some("ok".to_owned());
        assert!(
            tracker
                .candidates(&[item.clone()], now, &policy())
                .is_empty()
        );
        item.tracked_download_status = Some("warning".to_owned());

        item.status_messages[0].messages =
            vec!["No files found are eligible for import".to_owned()];
        assert!(tracker.candidates(&[item], now, &policy()).is_empty());
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
        let candidates = CandidateTracker::default().candidates(&items, now, &policy());

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, 1);
    }

    #[test]
    fn ages_missing_timestamps_from_first_observation() {
        let now = Utc::now();
        let mut item = item(now);
        item.added = None;
        let items = [item];
        let mut tracker = CandidateTracker::default();

        assert!(tracker.candidates(&items, now, &policy()).is_empty());
        assert!(
            tracker
                .candidates(&items, now + TimeDelta::minutes(29), &policy())
                .is_empty()
        );
        assert_eq!(
            tracker
                .candidates(&items, now + TimeDelta::minutes(30), &policy())
                .len(),
            1
        );
    }

    #[test]
    fn missing_timestamp_age_resets_after_disappearance() {
        let now = Utc::now();
        let mut item = item(now);
        item.added = None;
        let items = [item];
        let mut tracker = CandidateTracker::default();

        assert!(tracker.candidates(&items, now, &policy()).is_empty());
        assert!(
            tracker
                .candidates(&[], now + TimeDelta::minutes(30), &policy())
                .is_empty()
        );
        assert!(
            tracker
                .candidates(&items, now + TimeDelta::minutes(31), &policy())
                .is_empty()
        );
        assert_eq!(
            tracker
                .candidates(&items, now + TimeDelta::minutes(61), &policy())
                .len(),
            1
        );
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

    #[test]
    fn formats_the_complete_error_chain_without_duplicate_wrappers() {
        let error = anyhow::Error::msg("DNS server timed out")
            .context("could not resolve service")
            .context("request failed");

        assert_eq!(
            format_error_chain(error.as_ref()),
            "request failed: could not resolve service: DNS server timed out"
        );
    }
}
