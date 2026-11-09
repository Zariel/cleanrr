# cleanrr

`cleanrr` is a small, long-running service that removes duplicate downloads and
stale, blocked imports from Radarr and Sonarr v3, v4, and v5 queues. It
clears lower-quality downloads of the same movie or episode. It removes them
while they are in the queue, and after Arr refuses to import them as
non-upgrades. It is designed for Kubernetes and exposes health probes and
Prometheus metrics from the same HTTP listener.

The default cleanup is deliberately conservative. It removes the item only
from the Arr queue; it does not delete the download from the download client,
change its category, or blocklist it. Your download client and existing
automation remain responsible for torrent retention and seeding rules.

## Cleanup policy

Cleanrr removes two kinds of queue items: duplicate downloads that a better
download in the queue supersedes, and blocked imports that are not upgrades.
Each item must also be in the queue for at least `minimum_age`, based on the
queue item's `added` timestamp. Set `minimum_age` to `0s` to remove duplicates
on the first poll. Each removal log line has a `reason` field with the value
`superseded` or `not_upgrade`. Superseded removals also log the `kept` title.

### Duplicate downloads

Cleanrr groups queue rows by movie, or by episode, and keeps the best download
in each group:

1. If both rows have a score, the higher Custom Format score wins.
2. Otherwise, a higher resolution wins.
3. Otherwise, the better source wins: Remux, then Bluray, then WEB, then HDTV.
4. Otherwise, the higher revision wins: REAL correction first, then the
   PROPER/REPACK version. An omitted revision defaults to an original release.
5. On a tie, an individual download wins over a pack, then the oldest row wins.

A pack is one download that covers more than one episode, for example a season
pack. Cleanrr keeps a pack while it is the best download for any of its
episodes. If every episode in a pack has a better download, cleanrr removes
the pack. Sonarr v4 (API v3) shows a pack as one row per episode with the same
download ID. Sonarr v5 shows a pack as one row with several `episodeIds`.

Cleanrr compares only rows with a download ID, `trackedDownloadStatus = ok`,
`status` of `queued`, `paused`, `downloading`, or `completed`, and
`trackedDownloadState` of `downloading`, `importPending`, or `importing`.
Pending releases, blocked imports, and failed downloads never replace a
download, and cleanrr never removes them as duplicates. A duplicate row
without an `added` timestamp is never removed.

### Blocked imports

Each blocked import that cleanrr removes meets both of these conditions:

1. Arr reports `trackedDownloadState = importBlocked`, or the item matches the
   narrow `importPending` compatibility signature described below.
2. Every status message is a non-upgrade rejection: it begins with
   `Not a Custom Format upgrade for existing episode file(s).` or
   `Not an upgrade for existing episode file(s).` (Sonarr), or with
   `Not a Custom Format upgrade for existing movie file(s).` or
   `Not an upgrade for existing movie file.` (Radarr).

Arr does not expose a structured rejection reason, so cleanrr matches these
message prefixes. Items blocked for any other reason, such as a series title
mismatch, missing files, or a sample, stay in the queue for manual action. An
item that is blocked for a non-upgrade and for another reason also stays.
Every status-message entry must contain rejection details. An entry with no
details, including a warning reported only in its title, prevents removal.

Sonarr and Radarr have a compatibility exception: a file rejected because it
is not a quality or Custom Format upgrade can remain in `importPending`.
Cleanrr recognizes only the complete response signature: `status = completed`,
`trackedDownloadStatus = warning`, `trackedDownloadState = importPending`, and
non-upgrade status messages as above. Other `importPending` items remain
untouched. Start in dry-run mode and confirm this policy fits your queues.

`minimum_age` is the item's total residence time in the Arr queue when Arr
provides an `added` timestamp. Sonarr can omit that field for tracked download
rows; in that case cleanrr conservatively measures completed, warning rows
from the first blocked poll observed during the current process. The fallback
uses monotonic time and its state is not persisted. A failed poll, restart, or
the item disappearing from a successful poll resets the timer. Items with a
future timestamp are left untouched. Queue removals are idempotent; an item
that disappears between polling and deletion is treated as already handled.

## Configuration

Copy [`config.example.toml`](config.example.toml) to `cleanrr.toml` and add at
least one server:

```toml
minimum_age = "30m"

[servers.movies]
url = "http://radarr:7878"
api_key = "replace-me"

[servers.tv]
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
Radarr or Sonarr instances are supported. No server type is configured:
cleanrr discovers the current API version from each server's `/api` endpoint.
It uses `/api/v3` for Radarr and Sonarr v3/v4, and `/api/v5` for Sonarr v5.
The API version is cached until cleanrr restarts so queue reads and removals
use the same queue IDs. Restart cleanrr after upgrading a server to a different
API version. Remove the former `kind` field or `__KIND` environment variable
from existing configurations.

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
  ghcr.io/zariel/cleanrr:v0.2.1
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
poll timestamp. The `server` label identifies the configured instance.

## Development

```console
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --locked --release
```

`Cargo.toml` defines the Rust version used by CI and container builds through
`package.rust-version`. To build the container locally, pass that version as a
build argument:

```console
docker build \
  --build-arg RUST_VERSION="$(sed -n 's/^rust-version = "\([^"]*\)".*/\1/p' Cargo.toml)" \
  -t cleanrr .
```

The project is licensed under the [MIT License](LICENSE).
