# cleanrr

`cleanrr` is a small, long-running service that removes stale, blocked imports
from Radarr and Sonarr queues. Its primary use is clearing completed downloads
that Arr refuses to import because they are not upgrades. It is designed for
Kubernetes and exposes health probes and Prometheus metrics from the same HTTP
listener.

The default cleanup is deliberately conservative. It removes the item only
from the Arr queue; it does not delete the download from the download client,
change its category, or blocklist it. Your download client and existing
automation remain responsible for torrent retention and seeding rules.

## Cleanup policy

An item is eligible only when all of these are true:

1. Arr reports `trackedDownloadState = importBlocked`.
2. The item has been in the queue for at least `minimum_age`, based on the
   queue item's `added` timestamp.

Cleanrr intentionally treats Arr's typed `importBlocked` state as the source
of truth instead of matching human-readable, localized error messages. Arr
does not expose a structured rejection reason, so this convention includes
all `importBlocked` items, not only non-upgrades. Start in dry-run mode and
confirm this policy fits your queues.

`minimum_age` is the item's total residence time in the Arr queue, derived
from Arr's `added` timestamp. It is not time measured from the first blocked
poll, and cleanrr does not persist or reconcile local queue state. Items with
a missing or future timestamp are left untouched. Queue removals are
idempotent; an item that disappears between polling and deletion is treated
as already handled.

## Configuration

Copy [`config.example.toml`](config.example.toml) to `cleanrr.toml` and add at
least one server:

```toml
minimum_age = "30m"

[servers.movies]
kind = "radarr"
url = "http://radarr:7878"
api_key = "replace-me"

[servers.tv]
kind = "sonarr"
url = "http://sonarr:8989"
api_key = "replace-me"
```

By default cleanrr reads `./cleanrr.toml`. Set `CLEANRR_CONFIG` to use another
path. A missing default file is allowed when the complete configuration comes
from environment variables; a missing explicitly selected file is an error.

Environment variables override TOML. They use the `CLEANRR_` prefix and `__`
between nested keys:

```text
CLEANRR_MINIMUM_AGE=2h
CLEANRR_DRY_RUN=true
CLEANRR_SERVERS__MOVIES__URL=http://radarr.media.svc:7878
CLEANRR_SERVERS__MOVIES__API_KEY=secret
```

This makes it possible to mount ordinary configuration in a ConfigMap and
override API keys from Secrets. Server names are arbitrary, so multiple
Radarr or Sonarr instances are supported.

The main settings are:

| Setting | Default | Purpose |
| --- | --- | --- |
| `listen_addr` | `0.0.0.0:8080` | Probe and metrics listener |
| `poll_interval` | `1m` | Time between queue polls |
| `minimum_age` | `30m` | Minimum queue residence time |
| `dry_run` | `false` | Log candidates without removing them |
| `remove_from_client` | `false` | Ask Arr to delete from the download client |

Start with `dry_run = true` when validating the cleanup convention against
your queues. Enabling `remove_from_client` can delete data or disrupt seeding
depending on the Arr download-client configuration. API requests use a
15-second timeout, graceful shutdown uses a 10-second deadline, blocklisting
is always disabled, and logs are JSON. Set `RUST_LOG` to override the default
`cleanrr=info` log filter. Request failures include the complete error chain,
including underlying DNS, connection, and timeout causes where reqwest
provides them.

## Running

```console
cargo run --release
```

Published images use `ghcr.io/zariel/cleanrr`. Run one with a read-only config
mount:

```console
docker run --rm -p 8080:8080 \
  -v "$PWD/cleanrr.toml:/config/cleanrr.toml:ro" \
  -e CLEANRR_CONFIG=/config/cleanrr.toml \
  ghcr.io/zariel/cleanrr:v0.1.1
```

The image runs as an unprivileged user (UID/GID `65532`) and handles SIGTERM
as a graceful shutdown request. On shutdown, readiness fails immediately,
in-flight HTTP requests drain, and pollers stop within 10 seconds.

## Kubernetes probes and metrics

Use these HTTP endpoints on the configured listener:

| Endpoint | Use |
| --- | --- |
| `/health/startup` | Startup probe |
| `/health/live` or `/livez` | Liveness probe |
| `/health/ready` or `/readyz` | Readiness probe |
| `/metrics` | Prometheus/OpenMetrics scrape |

Readiness means cleanrr has loaded and validated configuration, bound its HTTP
listener, and started all pollers. An individual Arr outage is reported in
logs and metrics but does not make cleanrr unready or cause a restart loop.

Metrics include poll outcomes and duration, inspected queue-row counts,
matched underlying-download counts, removal outcomes, and the last successful
poll timestamp. The `server` and `kind` labels identify the configured
instance.

## Development

```console
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --locked --release
```

The project is licensed under the [MIT License](LICENSE).
