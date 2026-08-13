use std::{sync::Arc, time::Duration};

use prometheus_client::{
    encoding::{EncodeLabelSet, text::encode},
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
    },
    registry::Registry,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ServerLabels {
    server: String,
    kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabels {
    server: String,
    kind: String,
    result: String,
}

#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Registry>,
    polls: Family<ResultLabels, Counter>,
    queue_items: Family<ServerLabels, Counter>,
    matches: Family<ServerLabels, Counter>,
    removals: Family<ResultLabels, Counter>,
    poll_duration: Family<ServerLabels, Histogram>,
    last_success: Family<ServerLabels, Gauge<i64>>,
}

impl Metrics {
    pub fn new() -> Self {
        let polls = Family::<ResultLabels, Counter>::default();
        let queue_items = Family::<ServerLabels, Counter>::default();
        let matches = Family::<ServerLabels, Counter>::default();
        let removals = Family::<ResultLabels, Counter>::default();
        let poll_duration = Family::<ServerLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(exponential_buckets(0.05, 2.0, 12))
        });
        let last_success = Family::<ServerLabels, Gauge<i64>>::default();

        let mut registry = Registry::default();
        registry.register("cleanrr_polls", "Completed Arr queue polls.", polls.clone());
        registry.register(
            "cleanrr_queue_items",
            "Queue items inspected by cleanrr.",
            queue_items.clone(),
        );
        registry.register(
            "cleanrr_matches",
            "Queue items matching the cleanup policy.",
            matches.clone(),
        );
        registry.register(
            "cleanrr_removals",
            "Cleanup attempts by outcome.",
            removals.clone(),
        );
        registry.register(
            "cleanrr_poll_duration_seconds",
            "Time spent polling and processing an Arr queue.",
            poll_duration.clone(),
        );
        registry.register(
            "cleanrr_last_success_unixtime",
            "Unix timestamp of the last successful queue poll.",
            last_success.clone(),
        );

        Self {
            registry: Arc::new(registry),
            polls,
            queue_items,
            matches,
            removals,
            poll_duration,
            last_success,
        }
    }

    pub fn record_poll(&self, server: &str, kind: &str, result: &str, duration: Duration) {
        self.polls
            .get_or_create(&result_labels(server, kind, result))
            .inc();
        self.poll_duration
            .get_or_create(&server_labels(server, kind))
            .observe(duration.as_secs_f64());
    }

    pub fn record_poll_success(&self, server: &str, kind: &str, timestamp: i64) {
        self.last_success
            .get_or_create(&server_labels(server, kind))
            .set(timestamp);
    }

    pub fn add_queue_items(&self, server: &str, kind: &str, count: usize) {
        self.queue_items
            .get_or_create(&server_labels(server, kind))
            .inc_by(count as u64);
    }

    pub fn add_matches(&self, server: &str, kind: &str, count: usize) {
        self.matches
            .get_or_create(&server_labels(server, kind))
            .inc_by(count as u64);
    }

    pub fn record_removal(&self, server: &str, kind: &str, result: &str) {
        self.removals
            .get_or_create(&result_labels(server, kind, result))
            .inc();
    }

    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut body = String::new();
        encode(&mut body, &self.registry)?;
        Ok(body)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

fn server_labels(server: &str, kind: &str) -> ServerLabels {
    ServerLabels {
        server: server.to_owned(),
        kind: kind.to_owned(),
    }
}

fn result_labels(server: &str, kind: &str, result: &str) -> ResultLabels {
    ResultLabels {
        server: server.to_owned(),
        kind: kind.to_owned(),
        result: result.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_prometheus_metrics() {
        let metrics = Metrics::new();
        metrics.record_poll("movies", "radarr", "success", Duration::from_millis(10));
        metrics.add_queue_items("movies", "radarr", 3);
        metrics.add_matches("movies", "radarr", 1);
        metrics.record_removal("movies", "radarr", "removed");
        metrics.record_poll_success("movies", "radarr", 123);

        let output = metrics.encode().unwrap();
        assert!(output.contains("cleanrr_polls_total"));
        assert!(output.contains("server=\"movies\""));
        assert!(output.contains("kind=\"radarr\""));
        assert!(output.contains("cleanrr_last_success_unixtime"));
    }
}
