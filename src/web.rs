use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};

use crate::metrics::Metrics;

#[derive(Clone)]
pub struct HealthState {
    ready: Arc<AtomicBool>,
}

impl HealthState {
    pub fn new() -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct AppState {
    health: HealthState,
    metrics: Metrics,
}

pub fn router(health: HealthState, metrics: Metrics) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/health/startup", get(ready))
        .route("/livez", get(live))
        .route("/readyz", get(ready))
        .route("/metrics", get(prometheus))
        .with_state(AppState { health, metrics })
}

async fn live() -> &'static str {
    "ok\n"
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    if state.health.is_ready() {
        (StatusCode::OK, "ready\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
    }
}

async fn prometheus(State(state): State<AppState>) -> Response<Body> {
    match state.metrics.encode() {
        Ok(body) => Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )
            .body(Body::from(body))
            .expect("valid metrics response"),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("could not encode metrics\n"))
            .expect("valid error response"),
    }
}
