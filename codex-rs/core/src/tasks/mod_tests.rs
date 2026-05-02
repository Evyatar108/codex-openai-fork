use super::emit_turn_memory_metric;
use super::emit_turn_network_proxy_metric;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::SessionSource;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::Metric;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn notification(task_id: i32, exit_code: i32) -> ResponseInputItem {
    ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: format!(
                "<task_notification><task_id>{task_id}</task_id><status>completed</status><exit_code>{exit_code}</exit_code><summary>Background shell command completed (exit code {exit_code})</summary></task_notification>"
            ),
        }],
        phase: None,
    }
}

fn message(text: &str) -> ResponseInputItem {
    ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

fn text(item: &ResponseInputItem) -> &str {
    let ResponseInputItem::Message { content, .. } = item else {
        panic!("expected message item");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected single input text item");
    };
    text
}

fn test_session_telemetry() -> SessionTelemetry {
    let exporter = InMemoryMetricExporter::default();
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "codex-core", env!("CARGO_PKG_VERSION"), exporter)
            .with_runtime_reader(),
    )
    .expect("in-memory metrics client");
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.4",
        "gpt-5.4",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "tty".to_string(),
        SessionSource::Cli,
    )
    .with_metrics_without_metadata_tags(metrics)
}

fn find_metric<'a>(resource_metrics: &'a ResourceMetrics, name: &str) -> &'a Metric {
    for scope_metrics in resource_metrics.scope_metrics() {
        for metric in scope_metrics.metrics() {
            if metric.name() == name {
                return metric;
            }
        }
    }
    panic!("metric {name} missing");
}

fn attributes_to_map<'a>(
    attributes: impl Iterator<Item = &'a KeyValue>,
) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().to_string()))
        .collect()
}

fn metric_point(resource_metrics: &ResourceMetrics, name: &str) -> (BTreeMap<String, String>, u64) {
    let metric = find_metric(resource_metrics, name);
    match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => {
                let points: Vec<_> = sum.data_points().collect();
                assert_eq!(points.len(), 1);
                let point = points[0];
                (attributes_to_map(point.attributes()), point.value())
            }
            _ => panic!("unexpected counter aggregation"),
        },
        _ => panic!("unexpected counter data type"),
    }
}

#[test]
fn emit_turn_network_proxy_metric_records_active_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ true,
        ("tmp_mem_enabled", "true"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "true".to_string()),
            ("tmp_mem_enabled".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn coalesce_background_notifications_merges_consecutive_run() {
    // [notif-A, notif-B, notif-C, ordinary-input] → [coalesced(A+B+C), ordinary-input]
    let output = super::coalesce_background_notifications(vec![
        notification(101, 0),
        notification(202, 7),
        notification(303, -1),
        message("ordinary queued input"),
    ]);

    assert_eq!(output.len(), 2);
    let coalesced = text(&output[0]);
    assert!(coalesced.starts_with("<task_notification>"));
    assert!(coalesced.contains("<summary>3 background shell commands completed</summary>"));
    assert!(coalesced.contains("<tasks>"));
    assert!(coalesced.contains("<task><task_id>101</task_id><exit_code>0</exit_code></task>"));
    assert!(coalesced.contains("<task><task_id>202</task_id><exit_code>7</exit_code></task>"));
    assert!(coalesced.contains("<task><task_id>303</task_id><exit_code>-1</exit_code></task>"));
    assert_eq!(text(&output[1]), "ordinary queued input");
}

#[test]
fn coalesce_background_notifications_preserves_order_across_ordinary_input() {
    // [notif-A, ordinary-input, notif-B, notif-C] must NOT move notif-B/C before ordinary-input.
    // notif-A is a solo run (passes through), notif-B+notif-C form a run and get coalesced.
    let output = super::coalesce_background_notifications(vec![
        notification(101, 0),
        message("ordinary queued input"),
        notification(202, 7),
        notification(303, -1),
    ]);

    assert_eq!(output.len(), 3);
    assert_eq!(text(&output[0]), text(&notification(101, 0)));
    assert_eq!(text(&output[1]), "ordinary queued input");
    let coalesced = text(&output[2]);
    assert!(coalesced.starts_with("<task_notification>"));
    assert!(coalesced.contains("<summary>2 background shell commands completed</summary>"));
    assert!(coalesced.contains("<task><task_id>202</task_id><exit_code>7</exit_code></task>"));
    assert!(coalesced.contains("<task><task_id>303</task_id><exit_code>-1</exit_code></task>"));
}

#[test]
fn emit_turn_network_proxy_metric_records_inactive_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ false,
        ("tmp_mem_enabled", "false"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "false".to_string()),
            ("tmp_mem_enabled".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_read_allowed_with_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ true,
        /*has_citations*/ true,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "true".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "true".to_string()),
            ("read_allowed".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_config_disabled_without_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ false,
        /*has_citations*/ false,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "false".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "false".to_string()),
            ("read_allowed".to_string(), "false".to_string()),
        ])
    );
}
