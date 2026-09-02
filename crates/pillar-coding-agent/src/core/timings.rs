//! Port of packages/coding-agent/src/core/timings.ts (pi v0.84.3): central
//! startup timing instrumentation, enabled with `PI_TIMING=1`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

/// Upstream `TimingLabel` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimingLabel {
    Main,
    Extensions,
}

impl TimingLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            TimingLabel::Main => "main",
            TimingLabel::Extensions => "extensions",
        }
    }
}

struct TimingNamespace {
    timings: Vec<(String, u128)>,
    last_time: Instant,
}

fn enabled() -> bool {
    std::env::var("PI_TIMING").ok().as_deref() == Some("1")
}

fn namespaces() -> &'static Mutex<BTreeMap<TimingLabel, TimingNamespace>> {
    static NAMESPACES: std::sync::OnceLock<Mutex<BTreeMap<TimingLabel, TimingNamespace>>> =
        std::sync::OnceLock::new();
    NAMESPACES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Reset a timing namespace (upstream `resetTimings`).
pub fn reset_timings(namespace: TimingLabel) {
    if !enabled() {
        return;
    }
    namespaces().lock().unwrap().insert(
        namespace,
        TimingNamespace {
            timings: Vec::new(),
            last_time: Instant::now(),
        },
    );
}

/// Record a named elapsed delta since the previous `time` call in the
/// namespace (upstream `time`).
pub fn time(label: &str, namespace: TimingLabel) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let mut map = namespaces().lock().unwrap();
    map.entry(namespace).or_insert_with(|| TimingNamespace {
        timings: Vec::new(),
        last_time: now,
    });
    let ns = map.get_mut(&namespace).unwrap();
    ns.timings.push((
        label.to_string(),
        now.duration_since(ns.last_time).as_millis(),
    ));
    ns.last_time = now;
}

fn print_timing_group(title: &str, timings: &[(String, u128)]) {
    let printable: Vec<&(String, u128)> = timings.iter().collect();
    if printable.is_empty() {
        return;
    }
    eprintln!("\n--- {title} ---");
    for (label, ms) in &printable {
        eprintln!("  {label}: {ms}ms");
    }
    let total: u128 = printable.iter().map(|(_, ms)| ms).sum();
    eprintln!("  TOTAL: {total}ms");
    eprintln!("{}\n", "-".repeat(title.len() + 8));
}

/// Print all recorded timing groups to stderr (upstream `printTimings`).
pub fn print_timings() {
    if !enabled() {
        return;
    }
    for (namespace, ns) in namespaces().lock().unwrap().iter() {
        print_timing_group(
            &format!("Startup Timings: {}", namespace.as_str()),
            &ns.timings,
        );
    }
}
