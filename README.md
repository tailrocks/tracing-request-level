# tracing-request-level

`tracing-request-level` is a small utility crate for **runtime + per-request log-level control** in services built with `tracing`.

It is designed for setups where:
- you keep a normal base log level (for example `info`),
- you occasionally need extra detail (`debug` / `trace`) for a single request,
- and you want that override to propagate across service boundaries (HTTP -> gRPC).

Typical deployment roles for this crate:
- an edge HTTP service (Axum) that receives external requests
- one or more downstream gRPC services (tonic) that should inherit request-level overrides

## What Problem It Solves

Without this crate, increasing log verbosity often means changing global process log level, which can create noise and overhead for all traffic.

With this crate, you can:
1. Keep global logging conservative.
2. Override to a higher level for one request via `x-log-level`.
3. Propagate that override from upstream HTTP handlers to downstream gRPC calls.
4. Still support normal `RUST_LOG` per-target directives.

## How It Works

`PerRequestTraceFilter` evaluates events with three layers of allow rules:
1. `Targets` directives parsed from `RUST_LOG` (for example `info,tokio_postgres=debug`).
2. Runtime-adjustable base level from `LogLevelHandle`.
3. Per-request override stored in the current span (`log_level_override`) and task-local request context.

The request override value is transported through the `x-log-level` header.

## Public API

- `LOG_LEVEL_HEADER`: header/metadata key (`x-log-level`).
- `LogLevelHandle`: shared runtime handle to get/set base level.
- `PerRequestTraceFilter`: tracing filter combining `Targets` + runtime base + per-request override.
- `log_level_middleware` (feature `axum`): reads `x-log-level` from HTTP request headers.
- `propagate_log_level` (feature `tonic-client`): tonic interceptor that forwards request-level override as gRPC metadata.
- `GrpcLogLevelLayer` (feature `tonic-server`): Tower layer for tonic servers; reads gRPC metadata and instruments request handling with override span.

## Feature Flags

All optional functionality is feature-gated:

- `axum`: enables Axum middleware support and request task-local context.
- `tonic-client`: enables tonic interceptor support and request task-local context.
- `tonic-server`: enables gRPC server layer support and request task-local context.
- `request-context`: internal/shared task-local context feature (`tokio::task_local!`).

`axum`, `tonic-client`, and `tonic-server` all include `request-context` automatically.

## Usage

### 1) Initialize tracing with `PerRequestTraceFilter`

```rust
use tracing::Level;
use tracing_request_level::{LogLevelHandle, PerRequestTraceFilter};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

let targets: Targets = rust_log
    .parse()
    .unwrap_or_else(|_| Targets::new().with_default(Level::INFO));

let initial_level = rust_log
    .split(',')
    .next()
    .and_then(|s| s.trim().parse::<Level>().ok())
    .unwrap_or(Level::INFO);

let log_level_handle = LogLevelHandle::new(initial_level);
let filter = PerRequestTraceFilter::new(log_level_handle.clone(), targets);

tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer().with_filter(filter))
    .init();
```

This initialization pattern is typically shared by both edge and downstream services.

### 2) Axum ingress (`axum` feature)

Enable middleware on router:

```rust
use axum::{Router, middleware};
use tracing_request_level::log_level_middleware;

let app = Router::new()
    .layer(middleware::from_fn(log_level_middleware));
```

Now an incoming HTTP request can override level for its request scope:

```bash
curl -H 'x-log-level: trace' http://localhost:8080/graphql
```

Example usage: attach this middleware near the edge of your HTTP request pipeline.

### 3) tonic client propagation (`tonic-client` feature)

Wrap each generated client with interceptor:

```rust
let mut client = SomeServiceClient::with_interceptor(
    channel.clone(),
    tracing_request_level::propagate_log_level,
);
```

If current request context has an override level, interceptor inserts:
- metadata key: `x-log-level`
- metadata value: `trace | debug | info | warn | error`

Example usage: apply this interceptor to every generated tonic client used inside request handlers.

### 4) tonic server handling (`tonic-server` feature)

Add gRPC layer on server builder:

```rust
use tonic::transport::Server;
use tracing_request_level::GrpcLogLevelLayer;

Server::builder()
    .layer(GrpcLogLevelLayer)
    // .add_service(...)
    ;
```

The layer reads incoming `x-log-level` metadata, creates an instrumented span with `log_level_override`, and scopes downstream work in task-local request context.

Example usage: add this layer once on your tonic `Server::builder()` so all services inherit it.

## End-to-End Flow (Typical Setup)

1. External caller hits an edge HTTP service and sends `x-log-level: trace` header.
2. `log_level_middleware` stores this request override in span/task-local context.
3. The edge service makes gRPC calls to downstream services using tonic clients configured with `propagate_log_level`.
4. Interceptor forwards the same level in gRPC metadata (`x-log-level`).
5. The downstream gRPC service has `GrpcLogLevelLayer`, which applies that request-level override while handling the request.

Result: both services produce verbose logs only for that request path.

## Runtime Global Level Changes

`LogLevelHandle` lets you change process base level at runtime without rebuilding tracing subscriber.

Common patterns for runtime global changes:
- edge service: HTTP endpoint such as `POST /log-level`
- gRPC service: admin RPC methods such as `GetLogLevel` and `SetLogLevel`

Per-request override still takes precedence for the request where it is present.

## Best-Practices Audit

### What This Crate Already Does Well

1. Clear separation of concerns: filter logic, Axum ingress, tonic propagation, and tonic server handling are independent but composable.
2. Feature-gated optional dependencies: no forced Axum/tonic/tower deps unless feature is enabled.
3. Runtime mutability with lock-free primitive (`AtomicU8`) for lightweight global level updates.
4. Preserves normal `RUST_LOG` target directives via `Targets` instead of replacing them.
5. Uses request-scoped context (`tokio::task_local!`) to avoid process-wide side effects.
6. Uses lowercase canonical level values for header/metadata transport, reducing ambiguity.

### Gaps Against Common Library Best Practices

1. No automated tests in this crate yet (unit + feature-gated integration tests are recommended).
2. No doctests/examples in crate docs (`examples/` folder would help consumers).
3. Public API docs exist for key types/functions, but some behavior details (precedence edge cases) could be documented more explicitly in rustdoc.
4. No benchmark coverage for the filter/layer overhead under high throughput.

### Suggested Next Improvements

1. Add unit tests for level conversion and precedence ordering (`Targets` vs base level vs per-request override).
2. Add integration tests per feature (`axum`, `tonic-client`, `tonic-server`) validating header/metadata propagation.
3. Add one minimal runnable example for each feature combination.
4. Consider exposing a small helper to parse/derive base level from `RUST_LOG` to reduce duplicated setup code in consuming crates.

## Cargo Examples

Use crates.io dependency declarations based on your runtime role.

Axum edge service (HTTP ingress only):

```toml
tracing-request-level = { version = "0.1", features = ["axum"] }
```

tonic client service (gRPC propagation only):

```toml
tracing-request-level = { version = "0.1", features = ["tonic-client"] }
```

Axum edge service that also makes tonic client calls:

```toml
tracing-request-level = { version = "0.1", features = ["axum", "tonic-client"] }
```

tonic gRPC server:

```toml
tracing-request-level = { version = "0.1", features = ["tonic-server"] }
```

## Notes

- Header names are case-insensitive in HTTP/gRPC metadata transport; crate constant uses lowercase `x-log-level`.
- Valid values are tracing levels: `trace`, `debug`, `info`, `warn`, `error`.
