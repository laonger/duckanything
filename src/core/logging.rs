//! Logging initialization: stderr tracing output controlled by the
//! `DUCKANYTHING_LOG` environment variable.
//!
//! stdout is reserved for JSON-RPC in serve mode, so the subscriber is
//! explicitly configured to write to stderr (`tracing_subscriber::fmt()`
//! defaults to stdout, which would corrupt the protocol stream).

use tracing_subscriber::EnvFilter;

/// Initializes the global tracing subscriber.
///
/// Reads the `DUCKANYTHING_LOG` environment variable (default `warn` when
/// unset) and writes formatted log lines to stderr. Uses `try_init` so a
/// second call (e.g. from a test binary) is a no-op rather than an error.
pub fn init() {
    let filter = EnvFilter::new(
        std::env::var("DUCKANYTHING_LOG").unwrap_or_else(|_| "warn".to_string()),
    );
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::filter::LevelFilter;

    #[test]
    fn default_filter_is_warn() {
        // The default directive string used by `init` when DUCKANYTHING_LOG
        // is unset: warn and above pass, debug is filtered out.
        let filter = EnvFilter::new("warn");
        assert_eq!(filter.max_level_hint(), Some(LevelFilter::WARN));
    }

    #[test]
    fn debug_directive_parses_and_enables_debug() {
        let filter = EnvFilter::new("debug");
        assert_eq!(filter.max_level_hint(), Some(LevelFilter::DEBUG));
    }
}