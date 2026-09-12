//! Provider lifecycle behaviour against a local OTLP/HTTP stub: nothing is
//! contacted when telemetry is disabled, exported data carries the configured
//! service identity, and shutdown stays inside its deadline no matter how the
//! collector behaves.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use opentelemetry::trace::{Span as _, Tracer as _};
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValue;
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message as _;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use agentic_server::telemetry::{TelemetryConfig, TelemetryError, init_providers};

/// Collector behaviour for one stub instance.
#[derive(Clone, Copy)]
enum StubMode {
    /// Accept and acknowledge every export.
    Accept,
    /// Hold every request open for far longer than any test deadline.
    Hang,
}

#[derive(Clone)]
struct Stub {
    mode: StubMode,
    connections: Arc<AtomicUsize>,
    traces: Arc<Mutex<Vec<Bytes>>>,
    metrics: Arc<Mutex<Vec<Bytes>>>,
}

async fn receive(State(stub): State<Stub>, headers: HeaderMap, path: &'static str, body: Bytes) -> StatusCode {
    stub.connections.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
        Some("application/x-protobuf"),
        "OTLP/HTTP protobuf is the only wire format"
    );
    match stub.mode {
        StubMode::Hang => {
            tokio::time::sleep(Duration::from_secs(120)).await;
            StatusCode::OK
        }
        StubMode::Accept => {
            let store = if path == "traces" { &stub.traces } else { &stub.metrics };
            store.lock().await.push(body);
            StatusCode::OK
        }
    }
}

async fn spawn_stub(mode: StubMode) -> (String, Stub) {
    let stub = Stub {
        mode,
        connections: Arc::new(AtomicUsize::new(0)),
        traces: Arc::new(Mutex::new(Vec::new())),
        metrics: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route(
            "/v1/traces",
            post(|state, headers, body| receive(state, headers, "traces", body)),
        )
        .route(
            "/v1/metrics",
            post(|state, headers, body| receive(state, headers, "metrics", body)),
        )
        .with_state(stub.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), stub)
}

fn enabled_config(endpoint: &str) -> TelemetryConfig {
    TelemetryConfig::from_lookup(|name| match name {
        "OTEL_TRACES_EXPORTER" | "OTEL_METRICS_EXPORTER" => Some("otlp".to_owned()),
        "OTEL_SERVICE_NAME" => Some("agentic-api-test".to_owned()),
        _ => None,
    })
    .unwrap()
    .with_otlp_endpoint(endpoint)
}

fn service_name(resource: Option<&Resource>) -> Option<&str> {
    resource?
        .attributes
        .iter()
        .find(|attribute| attribute.key == "service.name")
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| match &value.value {
            Some(AnyValue::StringValue(name)) => Some(name.as_str()),
            _ => None,
        })
}

#[tokio::test]
async fn disabled_configuration_builds_nothing_and_contacts_nothing() {
    let (endpoint, stub) = spawn_stub(StubMode::Accept).await;
    let config = TelemetryConfig::disabled().with_otlp_endpoint(&endpoint);

    let (guard, handles) = init_providers(&config).unwrap();
    assert!(!guard.is_enabled());
    assert!(handles.tracer.is_none());
    assert!(handles.meter.is_none());

    guard.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(stub.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_carry_the_configured_service_identity() {
    let (endpoint, stub) = spawn_stub(StubMode::Accept).await;
    let config = enabled_config(&endpoint).with_otlp_timeout(Duration::from_secs(2));

    let (guard, handles) = init_providers(&config).unwrap();
    assert!(guard.is_enabled());

    let tracer = handles.tracer.expect("traces enabled");
    let mut span = tracer.start("spike.span");
    span.end();
    let meter = handles.meter.expect("metrics enabled");
    meter.u64_counter("spike.counter").build().add(1, &[]);

    guard.shutdown(Duration::from_secs(5)).await.unwrap();

    let trace_exports = stub.traces.lock().await;
    let export = ExportTraceServiceRequest::decode(trace_exports[0].clone()).unwrap();
    assert_eq!(
        service_name(export.resource_spans[0].resource.as_ref()),
        Some("agentic-api-test")
    );
    let span_names: Vec<&str> = export.resource_spans[0]
        .scope_spans
        .iter()
        .flat_map(|scope| scope.spans.iter().map(|span| span.name.as_str()))
        .collect();
    assert_eq!(span_names, ["spike.span"]);

    let metric_exports = stub.metrics.lock().await;
    let export = ExportMetricsServiceRequest::decode(metric_exports[0].clone()).unwrap();
    assert_eq!(
        service_name(export.resource_metrics[0].resource.as_ref()),
        Some("agentic-api-test")
    );
    let metric_names: Vec<&str> = export.resource_metrics[0]
        .scope_metrics
        .iter()
        .flat_map(|scope| scope.metrics.iter().map(|metric| metric.name.as_str()))
        .collect();
    assert_eq!(metric_names, ["spike.counter"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_completes_when_the_collector_times_out() {
    let (endpoint, _stub) = spawn_stub(StubMode::Hang).await;
    let config = enabled_config(&endpoint).with_otlp_timeout(Duration::from_millis(200));
    let (guard, handles) = init_providers(&config).unwrap();
    let mut span = handles.tracer.unwrap().start("spike.span");
    span.end();
    handles.meter.unwrap().u64_counter("spike.counter").build().add(1, &[]);

    let started = Instant::now();
    let result = guard.shutdown(Duration::from_secs(3)).await;
    let elapsed = started.elapsed();

    assert!(elapsed < Duration::from_secs(3), "shutdown took {elapsed:?}");
    match result {
        Ok(()) | Err(TelemetryError::ProviderShutdown { .. }) => {}
        Err(other) => panic!("unexpected shutdown outcome: {other}"),
    }
}

/// An unresponsive collector combined with a long exporter timeout: the
/// guard must give up at its own deadline rather than wait for the exporter.
///
/// Uses a dedicated runtime so the still-running blocking shutdown does not
/// hold the test open; `main` stops its runtime the same way.
#[test]
fn shutdown_deadline_is_honoured_with_an_unresponsive_collector() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let deadline = Duration::from_millis(500);

    let (result, elapsed) = runtime.block_on(async {
        let (endpoint, _stub) = spawn_stub(StubMode::Hang).await;
        let config = enabled_config(&endpoint).with_otlp_timeout(Duration::from_secs(60));
        let (guard, handles) = init_providers(&config).unwrap();
        let mut span = handles.tracer.unwrap().start("spike.span");
        span.end();
        // A pending metric makes the final export block on the collector, so
        // the blocking shutdown provably outlives the guard's deadline.
        handles.meter.unwrap().u64_counter("spike.counter").build().add(1, &[]);

        let started = Instant::now();
        let result = guard.shutdown(deadline).await;
        (result, started.elapsed())
    });

    assert!(
        matches!(result, Err(TelemetryError::ShutdownTimeout { deadline: reported }) if reported == deadline),
        "expected a deadline error, got {result:?}"
    );
    assert!(elapsed >= deadline, "returned before the deadline: {elapsed:?}");
    assert!(elapsed < deadline * 3, "overshot the deadline: {elapsed:?}");

    runtime.shutdown_background();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_an_enabled_guard_inside_the_runtime_does_not_panic() {
    let (endpoint, _stub) = spawn_stub(StubMode::Accept).await;
    let config = enabled_config(&endpoint).with_otlp_timeout(Duration::from_millis(200));
    let (guard, handles) = init_providers(&config).unwrap();
    let mut span = handles.tracer.unwrap().start("spike.span");
    span.end();
    drop(handles.meter);
    drop(guard);
}
