//! Root HTTP span and HTTP metrics through a real axum router, with the
//! production layer stack scoped to the test and in-memory exporters.

use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt as _;
use opentelemetry::Value;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics, SumDataPoint};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tower::ServiceExt as _;
use tracing::Dispatch;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::EnvFilter;

use agentic_server::telemetry::build_subscriber;
use agentic_server::telemetry::http::{HttpMetrics, track_request};

const ALLOWED_SPAN_ATTRIBUTES: &[&str] = &[
    "http.request.method",
    "http.route",
    "http.response.status_code",
    "url.scheme",
    "network.protocol.version",
];

struct Harness {
    router: Router,
    dispatch: Dispatch,
    spans: InMemorySpanExporter,
    metrics: InMemoryMetricExporter,
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
}

impl Harness {
    fn new() -> Self {
        let spans = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder().with_simple_exporter(spans.clone()).build();
        let metrics = InMemoryMetricExporter::default();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build();
        let http_metrics = HttpMetrics::new(&meter_provider.meter("test"));

        let router = Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .route("/stream", get(stream_slowly))
            .layer(middleware::from_fn_with_state(http_metrics, track_request));
        let dispatch = Dispatch::new(build_subscriber(
            Some(tracer_provider.tracer("test")),
            EnvFilter::new("info"),
            io::sink,
        ));
        Self {
            router,
            dispatch,
            spans,
            metrics,
            tracer_provider,
            meter_provider,
        }
    }

    /// Send a request under the test subscriber; the returned body has not
    /// been read yet.
    async fn send(&self, path: &str) -> Response {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        self.router
            .clone()
            .oneshot(request)
            .with_subscriber(self.dispatch.clone())
            .await
            .unwrap()
    }

    fn finished_spans(&self) -> Vec<SpanData> {
        self.spans.get_finished_spans().unwrap()
    }

    /// Cumulative `http.server.active_requests` value and per-attribute-set
    /// counts of `http.server.request.duration`.
    fn snapshot(&self) -> (i64, BTreeMap<Vec<(String, String)>, u64>) {
        self.meter_provider.force_flush().unwrap();
        let exports = self.metrics.get_finished_metrics().unwrap();
        let latest = exports.last().expect("at least one export");
        (active_requests(latest), duration_counts(latest))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Providers are shut down after every assertion has read the exporters.
        let _ = self.tracer_provider.shutdown();
        let _ = self.meter_provider.shutdown();
    }
}

async fn stream_slowly() -> Response {
    let chunks = futures::stream::unfold(0u8, |sent| async move {
        if sent == 3 {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        Some((Ok::<Bytes, io::Error>(Bytes::from_static(b"chunk\n")), sent + 1))
    });
    Body::from_stream(chunks).into_response()
}

fn active_requests(export: &ResourceMetrics) -> i64 {
    export
        .scope_metrics()
        .flat_map(ScopeMetrics::metrics)
        .filter(|metric| metric.name() == "http.server.active_requests")
        .map(|metric| match metric.data() {
            AggregatedMetrics::I64(MetricData::Sum(sum)) => sum.data_points().map(SumDataPoint::value).sum::<i64>(),
            other => panic!("unexpected active_requests data: {other:?}"),
        })
        .sum()
}

fn duration_counts(export: &ResourceMetrics) -> BTreeMap<Vec<(String, String)>, u64> {
    let mut counts = BTreeMap::new();
    for metric in export
        .scope_metrics()
        .flat_map(ScopeMetrics::metrics)
        .filter(|metric| metric.name() == "http.server.request.duration")
    {
        let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() else {
            panic!("unexpected duration data: {:?}", metric.data());
        };
        for point in histogram.data_points() {
            let mut attributes: Vec<(String, String)> = point
                .attributes()
                .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                .collect();
            attributes.sort();
            *counts.entry(attributes).or_default() += point.count();
        }
    }
    counts
}

fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn assert_allow_listed(span: &SpanData) {
    for kv in &span.attributes {
        assert!(
            ALLOWED_SPAN_ATTRIBUTES.contains(&kv.key.as_str()),
            "unexpected attribute {} on span {}",
            kv.key,
            span.name
        );
    }
}

#[tokio::test]
async fn successful_request_records_span_and_metrics() {
    let harness = Harness::new();
    let response = harness.send("/ok").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"ok");

    let spans = harness.finished_spans();
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.name, "GET /ok");
    assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Server);
    assert_eq!(attribute(span, "http.request.method"), Some(&Value::from("GET")));
    assert_eq!(attribute(span, "http.route"), Some(&Value::from("/ok")));
    assert_eq!(
        attribute(span, "http.response.status_code"),
        Some(&Value::from(200_i64))
    );
    assert_eq!(attribute(span, "url.scheme"), Some(&Value::from("http")));
    assert_eq!(attribute(span, "network.protocol.version"), Some(&Value::from("1.1")));
    assert_eq!(span.status, opentelemetry::trace::Status::Unset);
    assert_allow_listed(span);

    let (active, durations) = harness.snapshot();
    assert_eq!(active, 0);
    let expected = vec![
        ("http.request.method".to_owned(), "GET".to_owned()),
        ("http.response.status_code".to_owned(), "200".to_owned()),
        ("http.route".to_owned(), "/ok".to_owned()),
        ("url.scheme".to_owned(), "http".to_owned()),
    ];
    assert_eq!(durations.get(&expected), Some(&1));
}

#[tokio::test]
async fn streaming_span_stays_open_until_the_body_completes() {
    let harness = Harness::new();
    let response = harness.send("/stream").await;
    assert_eq!(response.status(), StatusCode::OK);

    // Headers are out, the body is not: still in flight.
    assert!(harness.finished_spans().is_empty());
    let (active, _) = harness.snapshot();
    assert_eq!(active, 1);

    let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"chunk\nchunk\nchunk\n");

    let spans = harness.finished_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "GET /stream");
    let (active, durations) = harness.snapshot();
    assert_eq!(active, 0);
    assert_eq!(durations.values().sum::<u64>(), 1);
}

#[tokio::test]
async fn dropping_a_streaming_body_finalizes_once() {
    let harness = Harness::new();
    let response = harness.send("/stream").await;
    let mut chunks = response.into_body().into_data_stream();
    let first = chunks.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"chunk\n");
    assert!(harness.finished_spans().is_empty());

    // Client goes away mid-stream.
    drop(chunks);

    let spans = harness.finished_spans();
    assert_eq!(spans.len(), 1, "span closes when the body is dropped");
    let (active, durations) = harness.snapshot();
    assert_eq!(active, 0, "active-request gauge is balanced on drop");
    assert_eq!(durations.values().sum::<u64>(), 1, "duration recorded exactly once");
}

#[tokio::test]
async fn server_errors_mark_the_span() {
    let harness = Harness::new();
    let response = harness.send("/boom").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    drop(response);

    let spans = harness.finished_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        attribute(&spans[0], "http.response.status_code"),
        Some(&Value::from(500_i64))
    );
    assert!(matches!(spans[0].status, opentelemetry::trace::Status::Error { .. }));
    assert_allow_listed(&spans[0]);
}

#[tokio::test]
async fn unmatched_routes_have_no_route_attribute() {
    let harness = Harness::new();
    let response = harness.send("/missing?secret=1").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    drop(response);

    let spans = harness.finished_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "GET");
    assert_eq!(attribute(&spans[0], "http.route"), None);
    assert_allow_listed(&spans[0]);
    let (_, durations) = harness.snapshot();
    let attributes: Vec<_> = durations.keys().flatten().collect();
    assert!(!attributes.iter().any(|(key, _)| key == "http.route"), "{attributes:?}");
    assert!(
        !attributes.iter().any(
            |(key, value)| ["url.full", "url.path", "url.query"].contains(&key.as_str()) || value.contains("secret")
        ),
        "raw URL or query captured: {attributes:?}"
    );
}
