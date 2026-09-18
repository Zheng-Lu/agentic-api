//! Provider lifecycle behaviour against a local OTLP/HTTP stub: nothing is
//! contacted when telemetry is disabled, exported data carries the configured
//! service identity, and shutdown stays inside its deadline no matter how the
//! collector behaves.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use opentelemetry::trace::{Span as _, Tracer as _};

use agentic_server::telemetry::{TelemetryConfig, TelemetryError, init_providers};

// Only the OTLP stub is used from the shared helpers here.
#[allow(dead_code)]
mod common;
use common::otlp_stub::{OtlpStub, StubMode, service_name};

fn enabled_config(endpoint: &str) -> TelemetryConfig {
    TelemetryConfig::from_lookup(|name| match name {
        "OTEL_TRACES_EXPORTER" | "OTEL_METRICS_EXPORTER" => Some("otlp".to_owned()),
        "OTEL_SERVICE_NAME" => Some("agentic-api-test".to_owned()),
        _ => None,
    })
    .unwrap()
    .with_otlp_endpoint(endpoint)
}

#[tokio::test]
async fn disabled_configuration_builds_nothing_and_contacts_nothing() {
    let (endpoint, stub) = OtlpStub::spawn(StubMode::Accept).await;
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
    let (endpoint, stub) = OtlpStub::spawn(StubMode::Accept).await;
    let config = enabled_config(&endpoint).with_otlp_timeout(Duration::from_secs(2));

    let (guard, handles) = init_providers(&config).unwrap();
    assert!(guard.is_enabled());

    let tracer = handles.tracer.expect("traces enabled");
    let mut span = tracer.start("spike.span");
    span.end();
    let meter = handles.meter.expect("metrics enabled");
    meter.u64_counter("spike.counter").build().add(1, &[]);

    guard.shutdown(Duration::from_secs(5)).await.unwrap();

    let trace_exports = stub.trace_exports().await;
    let export = &trace_exports[0];
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

    let metric_exports = stub.metric_exports().await;
    let export = &metric_exports[0];
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
    let (endpoint, _stub) = OtlpStub::spawn(StubMode::Hang).await;
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
        let (endpoint, _stub) = OtlpStub::spawn(StubMode::Hang).await;
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
    let (endpoint, _stub) = OtlpStub::spawn(StubMode::Accept).await;
    let config = enabled_config(&endpoint).with_otlp_timeout(Duration::from_millis(200));
    let (guard, handles) = init_providers(&config).unwrap();
    let mut span = handles.tracer.unwrap().start("spike.span");
    span.end();
    drop(handles.meter);
    drop(guard);
}
