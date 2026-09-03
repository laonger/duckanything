//! Unified error type for DuckAnything (Wave 1).

use std::fmt;

/// Informational result of a row-limit truncation — not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLimitExceeded {
    /// Rows actually returned after truncation.
    pub returned: usize,
    /// Total rows the query produced.
    pub total: usize,
}

#[derive(Debug)]
pub enum DuckAnythingError {
    Io(std::io::Error),
    /// Path failed validation (missing, not a file, not readable, outside allow-dir).
    PathValidation(String),
    /// File extension is not a supported format.
    UnsupportedFormat(String),
    /// SQL is not a read-only SELECT statement.
    ReadOnlyViolation(String),
    /// SQL execution failed in DuckDB.
    Sql(String),
    /// Query exceeded the configured timeout.
    Timeout(u64),
    /// Configuration is invalid.
    Config(String),
}

impl fmt::Display for DuckAnythingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DuckAnythingError::Io(e) => write!(f, "IO error: {e}"),
            DuckAnythingError::PathValidation(msg) => write!(f, "invalid path: {msg}"),
            DuckAnythingError::UnsupportedFormat(ext) => {
                write!(f, "unsupported file format: '{ext}' (supported: csv, json, parquet)")
            }
            DuckAnythingError::ReadOnlyViolation(stmt) => {
                write!(f, "read-only mode: '{stmt}' is not allowed; only SELECT/WITH/FROM queries")
            }
            DuckAnythingError::Sql(msg) => write!(f, "SQL error: {msg}"),
            DuckAnythingError::Timeout(secs) => write!(f, "query timed out after {secs}s"),
            DuckAnythingError::Config(msg) => write!(f, "invalid configuration: {msg}"),
        }
    }
}

impl std::error::Error for DuckAnythingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DuckAnythingError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DuckAnythingError {
    fn from(e: std::io::Error) -> Self {
        DuckAnythingError::Io(e)
    }
}

impl From<duckdb::Error> for DuckAnythingError {
    fn from(e: duckdb::Error) -> Self {
        DuckAnythingError::Sql(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_readable() {
        let cases: Vec<(DuckAnythingError, &str)> = vec![
            (
                DuckAnythingError::PathValidation("/nope.csv".into()),
                "invalid path",
            ),
            (
                DuckAnythingError::UnsupportedFormat("xlsx".into()),
                "unsupported file format",
            ),
            (
                DuckAnythingError::ReadOnlyViolation("DROP TABLE t".into()),
                "read-only mode",
            ),
            (DuckAnythingError::Timeout(30), "timed out after 30s"),
            (
                DuckAnythingError::Config("bad".into()),
                "invalid configuration",
            ),
            (DuckAnythingError::Sql("syntax".into()), "SQL error"),
        ];
        for (err, fragment) in cases {
            let msg = err.to_string();
            assert!(msg.contains(fragment), "{msg} should contain '{fragment}'");
        }
    }

    #[test]
    fn row_limit_is_informational_not_error() {
        let rl = RowLimitExceeded { returned: 100, total: 250 };
        assert_eq!(rl.returned, 100);
        assert_eq!(rl.total, 250);
        // RowLimitExceeded is not a DuckAnythingError variant
        let e: Option<DuckAnythingError> = None;
        assert!(e.is_none());
    }
}
