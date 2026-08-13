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
    match_patterns: Vec<String>,
    dry_run: bool,
    remove_from_client: bool,
    blocklist: bool,
}

impl From<&Config> for CleanupPolicy {
    fn from(config: &Config) -> Self {
        Self {
            minimum_age: config.minimum_age,
            match_patterns: config
                .match_patterns
                .iter()
                .map(|pattern| pattern.to_lowercase())
                .collect(),
            dry_run: config.dry_run,
            remove_from_client: config.remove_from_client,
            blocklist: config.blocklist,
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
    let client = match ArrClient::new(&server, config.request_timeout) {
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
                match poll_once(&client, &name, kind, &policy, &metrics).await {
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
    let mut candidate_ids = HashSet::new();
    let candidates: Vec<_> = items
        .iter()
        .filter(|item| is_candidate(item, now, policy))
        .filter(|item| candidate_ids.insert(item.id))
        .collect();
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
            .delete_queue_item(item.id, policy.remove_from_client, policy.blocklist)
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

    item.status_messages
        .iter()
        .flat_map(|status| &status.messages)
        .map(|message| message.to_lowercase())
        .any(|message| {
            policy
                .match_patterns
                .iter()
                .any(|pattern| message.contains(pattern))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arr::StatusMessage;
    use chrono::TimeDelta;

    fn policy() -> CleanupPolicy {
        CleanupPolicy {
            minimum_age: Duration::from_secs(1800),
            match_patterns: vec!["not an upgrade for existing".to_owned()],
            dry_run: false,
            remove_from_client: false,
            blocklist: false,
        }
    }

    fn item(now: DateTime<Utc>) -> QueueItem {
        QueueItem {
            id: 1,
            title: Some("release".to_owned()),
            added: Some(now - TimeDelta::minutes(31)),
            tracked_download_state: Some("importBlocked".to_owned()),
            status_messages: vec![StatusMessage {
                messages: vec!["Not an upgrade for existing episode file(s)".to_owned()],
            }],
        }
    }

    #[test]
    fn matches_old_blocked_non_upgrade() {
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
    fn does_not_match_other_import_failure() {
        let now = Utc::now();
        let mut item = item(now);
        item.status_messages[0].messages =
            vec!["No files found are eligible for import".to_owned()];
        assert!(!is_candidate(&item, now, &policy()));
    }

    #[test]
    fn does_not_match_non_blocked_download() {
        let now = Utc::now();
        let mut item = item(now);
        item.tracked_download_state = Some("downloading".to_owned());
        assert!(!is_candidate(&item, now, &policy()));
    }
}
