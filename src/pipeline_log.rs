/// Structured pipeline logging to `logs.txt`.
///
/// Append-only structured log entries written to `logs.txt` in the working directory.
/// Each entry is timestamped and tagged with a section identifier:
///   [SCREEN], [UIA], [VISION], [MODEL_INPUT], [MODEL_OUTPUT],
///   [PIPELINE], [EXEC], [SNAP], [VALIDATE]
///
/// Errors are silently ignored — logging must never crash the pipeline.
use std::io::Write;

use crate::core::coords::{PointL, PointP, PointS};

const LOG_FILE: &str = "logs.txt";

fn now_timestamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    format!("{secs}.{millis:03}")
}

fn append(line: &str) {
    let ts = now_timestamp();
    let formatted = format!("[{ts}] {line}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)
        .and_then(|mut f| f.write_all(formatted.as_bytes()));
}

pub fn log_screen(display_index: usize, width: f64, height: f64, origin_x: f64, origin_y: f64, dpi_scale: f64) {
    append(&format!(
        "[SCREEN] display[{display_index}] resolution={width:.0}x{height:.0} origin=({origin_x:.0},{origin_y:.0}) dpi={dpi_scale:.2}"
    ));
}

pub fn log_uia(count: usize, sample_elements: &[(String, String, f64, f64, f64, f64)]) {
    append(&format!("[UIA] count={count}"));
    for (name, role, x, y, w, h) in sample_elements.iter().take(10) {
        append(&format!(
            "[UIA]   name='{name}' role='{role}' rect=({x:.0},{y:.0},{w:.0},{h:.0})"
        ));
    }
}

pub fn log_vision(count: usize, sample_elements: &[(f64, f64, f64, f64)]) {
    append(&format!("[VISION] count={count}"));
    for (x, y, w, h) in sample_elements.iter().take(10) {
        append(&format!(
            "[VISION]   rect=({x:.0},{y:.0},{w:.0},{h:.0})"
        ));
    }
}

pub fn log_model_input(query: &str, elements_sent: usize) {
    append(&format!("[MODEL_INPUT] query=\"{query}\" elements_sent={elements_sent}"));
}

pub fn log_model_output(intent_label: Option<&str>, region_hint: Option<&str>, confidence: Option<f32>) {
    append(&format!(
        "[MODEL_OUTPUT] label={} region={} confidence={}",
        intent_label.unwrap_or("?"),
        region_hint.unwrap_or("?"),
        confidence.map(|c| format!("{c:.2}")).unwrap_or_else(|| "?".into()),
    ));
}

pub fn log_pipeline_trace(
    model: Option<(f64, f64)>,
    screenshot: Option<PointS>,
    logical: Option<PointL>,
    refined: Option<PointL>,
    snapped: Option<PointL>,
    physical: Option<PointP>,
    cursor: Option<PointP>,
) {
    let fmt = |opt: Option<(f64, f64)>| opt.map_or("?".to_string(), |(a, b)| format!("({a:.0},{b:.0})"));
    let fmt_s = |opt: Option<PointS>| opt.map_or("?".to_string(), |p| format!("({:.0},{:.0})", p.x, p.y));
    let fmt_l = |opt: Option<PointL>| opt.map_or("?".to_string(), |p| format!("({:.0},{:.0})", p.x, p.y));
    let fmt_p = |opt: Option<PointP>| opt.map_or("?".to_string(), |p| format!("({:.0},{:.0})", p.x, p.y));
    append(&format!(
        "[PIPELINE] model={} screenshot={} logical={} refined={} snapped={} physical={} cursor={}",
        fmt(model), fmt_s(screenshot), fmt_l(logical), fmt_l(refined), fmt_l(snapped), fmt_p(physical), fmt_p(cursor),
    ));
}

pub fn log_snap(source: &str, coord: PointL, snapped: Option<PointL>, radius: f64) {
    let snapped_str = snapped.map_or("rejected".to_string(), |p| format!("({:.0},{:.0})", p.x, p.y));
    append(&format!(
        "[SNAP] source={source} input=({:.0},{:.0}) output={snapped_str} radius={radius:.0}",
        coord.x, coord.y,
    ));
}

pub fn log_validate(coord: PointL, passed: bool, reason: &str) {
    let status = if passed { "PASS" } else { "FAIL" };
    append(&format!(
        "[VALIDATE] ({:.0},{:.0}) status={status} reason={reason}",
        coord.x, coord.y,
    ));
}

pub fn log_exec(target_x: i32, target_y: i32, actual_x: i32, actual_y: i32, attempt: u32, status: &str) {
    append(&format!(
        "[EXEC] target=({target_x},{target_y}) actual=({actual_x},{actual_y}) attempt={attempt} status={status}"
    ));
}

pub fn log_decision(path: &str, label: &str, detail: &str) {
    append(&format!("[DECISION] path={path} label='{label}' {detail}"));
}

pub fn log_uia_match(intent: &str, candidate: &str, normalized_intent: &str, normalized_candidate: &str, score: f64, accepted: bool) {
    let status = if accepted { "ACCEPT" } else { "REJECT" };
    append(&format!(
        "[UIA_MATCH] intent='{intent}' candidate='{candidate}' norm_intent='{normalized_intent}' norm_candidate='{normalized_candidate}' score={score:.2} {status}",
    ));
}

pub fn log_raw(section: &str, message: &str) {
    append(&format!("[{section}] {message}"));
}
