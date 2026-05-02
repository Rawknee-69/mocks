use tracing_subscriber::{fmt, EnvFilter};

/// Initialises the global tracing subscriber.
///
/// Behaviour is driven by environment variables:
/// - `LOG_LEVEL=debug`  → pretty-printed dev output with file/line info.
/// - `LOG_LEVEL=off`    → no output at all (zero overhead in prod).
/// - Unset              → falls back to `RUST_LOG`, then defaults to `info`.
///
/// Call this exactly once at process start, before any `tracing` macros fire.
pub fn init() {
    let log_level_env = std::env::var("LOG_LEVEL").unwrap_or_default();

    // Explicit opt-out
    if log_level_env.eq_ignore_ascii_case("off") {
        return;
    }

    // Build the directive: LOG_LEVEL wins, then RUST_LOG, then "info"
    let directive = if !log_level_env.is_empty() {
        log_level_env
    } else {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
    };

    let filter = EnvFilter::try_new(&directive).unwrap_or_else(|_| EnvFilter::new("info"));

    let is_debug = directive.eq_ignore_ascii_case("debug") || directive.eq_ignore_ascii_case("trace");

    if is_debug {
        fmt()
            .with_env_filter(filter)
            .with_file(true)
            .with_line_number(true)
            .with_target(true)
            .pretty()
            .init();
    } else {
        fmt().with_env_filter(filter).compact().init();
    }
}
