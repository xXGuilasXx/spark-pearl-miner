//! The Prometheus parser on a constructed vLLM exposition.

use spm_coexist::prom::{gauge_sum, parse_line, parse_vllm_load, PromError, RUNNING, WAITING};
use spm_coexist::VllmLoad;

const FIXTURE: &str = include_str!("fixtures/vllm-metrics.prom");

#[test]
fn fixture_sums_every_series_and_ignores_look_alike_metrics() {
    let load = parse_vllm_load(FIXTURE).unwrap();
    assert_eq!(load, VllmLoad { running: 3.0, waiting: 4.0 });
    assert!(!load.is_idle());
}

#[test]
fn idle_server() {
    let mut idle = FIXTURE.to_string();
    for (engine, metric, value) in
        [("0", RUNNING, "2.0"), ("1", RUNNING, "1.0"), ("0", WAITING, "4.0")]
    {
        let series = format!("{metric}{{engine=\"{engine}\",model_name=\"example-model\"}}");
        let from = format!("{series} {value}\n");
        assert!(idle.contains(&from), "{from}");
        idle = idle.replace(&from, &format!("{series} 0.0\n"));
    }
    let load = parse_vllm_load(&idle).unwrap();
    assert!(load.is_idle(), "{load:?}");
}

#[test]
fn single_engine_without_labels_and_with_timestamps() {
    let text = "vllm:num_requests_running 0\nvllm:num_requests_waiting 1 1790448895320\n";
    assert_eq!(parse_vllm_load(text).unwrap(), VllmLoad { running: 0.0, waiting: 1.0 });
}

#[test]
fn missing_gauges_are_reported() {
    assert_eq!(parse_vllm_load("# nothing\n"), Err(PromError::Missing(RUNNING)));
    assert_eq!(
        parse_vllm_load("vllm:num_requests_running{engine=\"0\"} 0.0\n"),
        Err(PromError::Missing(WAITING))
    );
    // A different server on the port.
    assert!(matches!(parse_vllm_load("<html>Not Found</html>"), Err(PromError::Missing(_))));
}

#[test]
fn malformed_lines_of_our_gauges_fail_closed() {
    let cases = [
        "vllm:num_requests_running{engine=\"0\" 1.0",
        "vllm:num_requests_running{engine=\"0\"}",
        "vllm:num_requests_running{engine=\"0\"} busy",
        "vllm:num_requests_running{engine=\"0\"} NaN",
        "vllm:num_requests_running{engine=\"0\"} -1",
        "vllm:num_requests_running{engine=\"0\"} +Inf",
        "vllm:num_requests_running 1 2 3",
    ];
    for line in cases {
        let text = format!("{line}\nvllm:num_requests_waiting 0\n");
        assert!(
            matches!(parse_vllm_load(&text), Err(PromError::BadLine { line: 1, .. })),
            "{line}"
        );
    }
}

#[test]
fn odd_lines_of_other_metrics_do_not_matter() {
    let text = "garbage without value\nsome_metric{a=\"}\" 1\nvllm:num_requests_running 0\n\
                vllm:num_requests_waiting 0\n";
    assert!(parse_vllm_load(text).unwrap().is_idle());
}

#[test]
fn line_parser_handles_quoted_braces_and_escapes() {
    let s = parse_line(r#"m{a="x}y",b="say \"hi\" \\"} 2.5e+00 123"#).unwrap().unwrap();
    assert_eq!(s.name, "m");
    assert_eq!(s.labels, r#"a="x}y",b="say \"hi\" \\""#);
    assert_eq!(s.value, 2.5);
    assert_eq!(parse_line("# HELP m help").unwrap(), None);
    assert_eq!(parse_line("   ").unwrap(), None);
    assert_eq!(gauge_sum(FIXTURE, "vllm:kv_cache_usage_perc").unwrap(), Some(0.42));
    assert_eq!(gauge_sum(FIXTURE, "vllm:absent").unwrap(), None);
}
