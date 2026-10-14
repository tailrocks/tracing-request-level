use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::Filter;
use tracing_subscriber::registry::LookupSpan;

#[cfg(any(feature = "tonic-server", feature = "tonic-client-logging"))]
use std::future::Future;
#[cfg(any(feature = "tonic-server", feature = "tonic-client-logging"))]
use std::pin::Pin;
#[cfg(any(feature = "tonic-server", feature = "tonic-client-logging"))]
use std::task::{Context as TaskContext, Poll};

#[cfg(any(feature = "tonic-server", feature = "tonic-client-logging"))]
use http::Request;
#[cfg(any(feature = "tonic-server", feature = "tonic-client-logging"))]
use tower::{Layer, Service};
#[cfg(any(feature = "axum", feature = "tonic-server"))]
use tracing::Instrument;

pub const LOG_LEVEL_HEADER: &str = "x-log-level";

/// Per-request log level override stored in span extensions.
#[derive(Debug, Clone)]
struct LogLevelOverride(Level);

/// Visits span fields looking for a `log_level_override` string value.
struct LogLevelOverrideVisitor {
    level: Option<Level>,
}

impl tracing::field::Visit for LogLevelOverrideVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "log_level_override" {
            self.level = value.parse().ok();
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

/// Shared handle for the base log level, allowing runtime changes.
#[derive(Clone)]
pub struct LogLevelHandle {
    level: Arc<AtomicU8>,
}

impl LogLevelHandle {
    pub fn new(level: Level) -> Self {
        Self {
            level: Arc::new(AtomicU8::new(level_to_u8(level))),
        }
    }

    pub fn set(&self, level: Level) {
        self.level.store(level_to_u8(level), Ordering::Relaxed);
    }

    pub fn get(&self) -> Level {
        u8_to_level(self.level.load(Ordering::Relaxed))
    }
}

fn level_to_u8(level: Level) -> u8 {
    match level {
        Level::ERROR => 0,
        Level::WARN => 1,
        Level::INFO => 2,
        Level::DEBUG => 3,
        Level::TRACE => 4,
    }
}

fn u8_to_level(v: u8) -> Level {
    match v {
        0 => Level::ERROR,
        1 => Level::WARN,
        2 => Level::INFO,
        3 => Level::DEBUG,
        _ => Level::TRACE,
    }
}

#[cfg(feature = "request-context")]
fn level_to_str(level: Level) -> &'static str {
    match level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

#[cfg(feature = "request-context")]
tokio::task_local! {
    static REQUEST_LOG_LEVEL: Level;
}

/// A tracing [`Filter`] that combines a base log level with per-request overrides.
///
/// Supports full `RUST_LOG` directive syntax (e.g. `info,tokio_postgres=debug`) via
/// [`Targets`], plus a runtime-adjustable base level via [`LogLevelHandle`],
/// and per-request overrides via span extensions.
pub struct PerRequestTraceFilter {
    handle: LogLevelHandle,
    targets: Targets,
}

impl PerRequestTraceFilter {
    pub fn new(handle: LogLevelHandle, targets: Targets) -> Self {
        Self { handle, targets }
    }
}

impl<S> Filter<S> for PerRequestTraceFilter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        // Always allow middleware spans carrying the override field to be created.
        if meta.is_span() && meta.fields().field("log_level_override").is_some() {
            return true;
        }

        // Check RUST_LOG per-target directives (e.g. info,tokio_postgres=debug).
        if self.targets.would_enable(meta.target(), meta.level()) {
            return true;
        }

        // Check runtime-adjustable base level.
        if *meta.level() <= self.handle.get() {
            return true;
        }

        // Slow path: check if the current span carries a per-request override.
        cx.lookup_current()
            .and_then(|span| {
                span.extensions()
                    .get::<LogLevelOverride>()
                    .map(|o| *meta.level() <= o.0)
            })
            .unwrap_or(false)
    }

    fn callsite_enabled(&self, _meta: &'static Metadata<'static>) -> Interest {
        // Callsite enablement must be request-sensitive.
        Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = LogLevelOverrideVisitor { level: None };
        attrs.record(&mut visitor);

        if let Some(span) = ctx.span(id) {
            if let Some(level) = visitor.level {
                span.extensions_mut().insert(LogLevelOverride(level));
            } else {
                // Propagate from parent so child spans inherit the override.
                let parent_override = span
                    .parent()
                    .and_then(|p| p.extensions().get::<LogLevelOverride>().cloned());
                if let Some(o) = parent_override {
                    span.extensions_mut().insert(o);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Axum middleware
// ---------------------------------------------------------------------------

#[cfg(feature = "axum")]
/// Axum middleware that reads the `x-log-level` header and, when present,
/// wraps the request in a tracing span carrying a `log_level_override` field.
///
/// Usage: `router.layer(axum::middleware::from_fn(log_level_middleware))`
pub async fn log_level_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let override_level = request
        .headers()
        .get(LOG_LEVEL_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<Level>().ok());

    if let Some(level) = override_level {
        let span = tracing::info_span!("request", log_level_override = level_to_str(level));
        REQUEST_LOG_LEVEL
            .scope(level, next.run(request).instrument(span))
            .await
    } else {
        next.run(request).await
    }
}

/// Maximum request body size (in bytes) that will be buffered for logging.
#[cfg(feature = "axum")]
const MAX_LOG_BODY_SIZE: usize = 1024 * 1024;

#[cfg(feature = "axum")]
/// Axum middleware that logs incoming HTTP requests at `DEBUG` level.
///
/// Logs the HTTP method, URI path, and request body (UTF-8 text up to 1 MiB).
/// Non-UTF-8 bodies are logged with their byte size instead.
///
/// Usage: `router.layer(axum::middleware::from_fn(request_logging_middleware))`
pub async fn request_logging_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use http_body_util::BodyExt;

    let method = request.method().clone();
    let uri = request.uri().path().to_string();

    let (parts, body) = request.into_parts();
    let collected = body.collect().await;

    match collected {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            if bytes.len() <= MAX_LOG_BODY_SIZE {
                if let Ok(body_str) = std::str::from_utf8(&bytes) {
                    tracing::debug!(%method, uri = %uri, body = %body_str, "incoming request");
                } else {
                    tracing::debug!(%method, uri = %uri, body_size = bytes.len(), "incoming request");
                }
            } else {
                tracing::debug!(%method, uri = %uri, body_size = bytes.len(), "incoming request");
            }
            let request = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
            next.run(request).await
        }
        Err(err) => {
            tracing::debug!(%method, uri = %uri, error = %err, "incoming request (failed to read body)");
            let request = axum::extract::Request::from_parts(parts, axum::body::Body::empty());
            next.run(request).await
        }
    }
}

// ---------------------------------------------------------------------------
// Tonic client: log-level propagation interceptor
// ---------------------------------------------------------------------------

#[cfg(feature = "tonic-client")]
/// Tonic interceptor that propagates the per-request log level override
/// as `x-log-level` gRPC metadata to downstream services.
pub fn propagate_log_level(
    mut req: tonic::Request<()>,
) -> Result<tonic::Request<()>, tonic::Status> {
    let _ = REQUEST_LOG_LEVEL.try_with(|level| {
        req.metadata_mut().insert(
            LOG_LEVEL_HEADER,
            tonic::metadata::MetadataValue::from_static(level_to_str(*level)),
        );
    });
    Ok(req)
}

// ---------------------------------------------------------------------------
// Tonic server: per-request log-level layer
// ---------------------------------------------------------------------------

#[cfg(feature = "tonic-server")]
/// Tower [`Layer`] for gRPC servers that reads `x-log-level` from incoming
/// requests and instruments request handling with a per-request trace span.
#[derive(Clone)]
pub struct GrpcLogLevelLayer;

#[cfg(feature = "tonic-server")]
impl<S> Layer<S> for GrpcLogLevelLayer {
    type Service = GrpcLogLevelService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcLogLevelService { inner }
    }
}

#[cfg(feature = "tonic-server")]
#[derive(Clone)]
pub struct GrpcLogLevelService<S> {
    inner: S,
}

#[cfg(feature = "tonic-server")]
impl<S, B> Service<Request<B>> for GrpcLogLevelService<S>
where
    S: Service<Request<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let level = req
            .headers()
            .get(LOG_LEVEL_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<Level>().ok());

        let future = self.inner.call(req);

        if let Some(level) = level {
            let span =
                tracing::info_span!("grpc_request", log_level_override = level_to_str(level));
            Box::pin(REQUEST_LOG_LEVEL.scope(level, future.instrument(span)))
        } else {
            Box::pin(future)
        }
    }
}

// ---------------------------------------------------------------------------
// Tonic client: logging layer
// ---------------------------------------------------------------------------

#[cfg(feature = "tonic-client-logging")]
/// Tower [`Layer`] for gRPC clients that logs outgoing requests and responses.
///
/// - `DEBUG`: logs the gRPC method path before each call.
/// - `TRACE`: logs the gRPC method path and whether the call succeeded after each call.
///
/// Usage with tonic channel:
/// ```ignore
/// use tower::ServiceBuilder;
/// use tracing_request_level::GrpcClientLoggingLayer;
///
/// let channel = Endpoint::from_shared(endpoint)?.connect().await?;
/// let channel = ServiceBuilder::new()
///     .layer(GrpcClientLoggingLayer)
///     .service(channel);
/// ```
#[derive(Clone, Copy)]
pub struct GrpcClientLoggingLayer;

#[cfg(feature = "tonic-client-logging")]
impl<S> Layer<S> for GrpcClientLoggingLayer {
    type Service = GrpcClientLoggingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcClientLoggingService { inner }
    }
}

#[cfg(feature = "tonic-client-logging")]
#[derive(Clone)]
pub struct GrpcClientLoggingService<S> {
    inner: S,
}

#[cfg(feature = "tonic-client-logging")]
impl<S, B> Service<Request<B>> for GrpcClientLoggingService<S>
where
    S: Service<Request<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Response: std::fmt::Debug + Send + 'static,
    S::Error: std::fmt::Debug + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let method = req.uri().path().to_string();
        tracing::debug!(method = %method, "gRPC client request");

        let future = self.inner.call(req);

        Box::pin(async move {
            let result = future.await;
            match &result {
                Ok(response) => {
                    tracing::trace!(method = %method, response = ?response, "gRPC client response");
                }
                Err(err) => {
                    tracing::trace!(method = %method, error = ?err, "gRPC client response");
                }
            }
            result
        })
    }
}

// ---------------------------------------------------------------------------
// gRPC server: validation + logging interceptor macro
// ---------------------------------------------------------------------------

#[cfg(feature = "grpc-server-interceptor")]
/// Validates the incoming gRPC request message using protify.
///
/// Returns `tonic::Status::invalid_argument` if validation fails.
pub fn validate_message<M: protify::ValidatedMessage>(
    request: tonic::Request<M>,
) -> Result<tonic::Request<M>, tonic::Status> {
    request.get_ref().validate()?;
    Ok(request)
}

/// Generates a validated + logged wrapper for a gRPC service trait.
///
/// Wraps each method to:
/// 1. Log the incoming request at `DEBUG` level with method name and body.
/// 2. Validate the request message using protify.
/// 3. Delegate to the inner service implementation.
///
/// # Usage
///
/// ```ignore
/// use tracing_request_level::impl_validated_service;
///
/// impl_validated_service!(
///     ValidatedMyService,
///     my_proto::my_service_server::MyService,
///     get_item, my_proto::GetItemRequest, my_proto::GetItemResponse;
///     list_items, my_proto::ListItemsRequest, my_proto::ListItemsResponse
/// );
/// ```
#[cfg(feature = "grpc-server-interceptor")]
#[macro_export]
macro_rules! impl_validated_service {
    ($wrapper:ident, $trait:path, $( $method:ident, $request:ty, $response:ty );+ $(;)?) => {
        pub struct $wrapper<S> {
            inner: S,
        }

        impl<S> $wrapper<S> {
            pub fn new(inner: S) -> Self {
                Self { inner }
            }
        }

        #[tonic::async_trait]
        impl<S> $trait for $wrapper<S>
        where
            S: $trait + Send + Sync,
        {
            $(
                async fn $method(
                    &self,
                    request: tonic::Request<$request>,
                ) -> Result<tonic::Response<$response>, tonic::Status> {
                    tracing::debug!(
                        method = stringify!($method),
                        body = ?request.get_ref(),
                        "gRPC request"
                    );
                    self.inner
                        .$method($crate::validate_message(request)?)
                        .await
                }
            )+
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_handle_get_set_is_shared_across_clones() {
        let handle = LogLevelHandle::new(Level::INFO);
        let clone = handle.clone();

        assert_eq!(handle.get(), Level::INFO);
        clone.set(Level::DEBUG);

        assert_eq!(handle.get(), Level::DEBUG);
        assert_eq!(clone.get(), Level::DEBUG);
    }

    #[test]
    fn level_values_round_trip() {
        for level in [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ] {
            assert_eq!(u8_to_level(level_to_u8(level)), level);
        }
    }

    #[test]
    fn unknown_level_value_maps_to_trace() {
        assert_eq!(u8_to_level(99), Level::TRACE);
    }
}
