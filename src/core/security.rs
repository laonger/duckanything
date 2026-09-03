//! Wave 2 Lane A: security (path validation, readonly check, layered config).
//!
//! This module is a public API surface consumed by later waves (engine, CLI);
//! until then the items are intentionally unused, so dead-code warnings are
//! suppressed at module scope.

#![allow(dead_code)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::DuckAnythingError;

/// Environment variable names for layered configuration.
pub const ENV_MAX_ROWS: &str = "DUCKANYTHING_MAX_ROWS";
pub const ENV_TIMEOUT_SECS: &str = "DUCKANYTHING_TIMEOUT_SECS";
pub const ENV_ALLOW_DIR: &str = "DUCKANYTHING_ALLOW_DIR";

/// Read-only SQL prefixes (ASCII, case-insensitive).
const READONLY_PREFIXES: [&str; 3] = ["SELECT", "WITH", "FROM"];

/// Returns `true` if `sql` is a read-only statement.
///
/// Rules (design final):
/// - Leading/trailing whitespace and trailing semicolons are trimmed first.
/// - The statement must start with `SELECT`, `WITH`, or `FROM`
///   (ASCII, case-insensitive).
/// - No string-level semicolon inspection: multi-statement safety is enforced
///   by the engine layer's `prepare()`; this only checks the prefix.
pub fn is_readonly_sql(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';').trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let upper = trimmed.to_ascii_uppercase();
    READONLY_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Validates that `path` exists, is a regular file, and is readable.
///
/// When `allow_dir` is `Some`, the canonicalized path must be inside the
/// canonicalized allow directory; `..` traversal is rejected naturally after
/// canonicalization. When `allow_dir` is `None`, no directory restriction is
/// applied (design final).
pub fn validate_path(path: &Path, allow_dir: Option<&Path>) -> Result<(), DuckAnythingError> {
    let meta = fs::metadata(path).map_err(|e| {
        DuckAnythingError::PathValidation(format!("cannot access '{}': {e}", path.display()))
    })?;
    if !meta.is_file() {
        return Err(DuckAnythingError::PathValidation(format!(
            "'{}' is not a regular file",
            path.display()
        )));
    }
    fs::File::open(path).map_err(|e| {
        DuckAnythingError::PathValidation(format!("'{}' is not readable: {e}", path.display()))
    })?;

    if let Some(dir) = allow_dir {
        let canonical_path = path.canonicalize().map_err(|e| {
            DuckAnythingError::PathValidation(format!(
                "cannot canonicalize '{}': {e}",
                path.display()
            ))
        })?;
        let canonical_dir = dir.canonicalize().map_err(|e| {
            DuckAnythingError::PathValidation(format!(
                "cannot canonicalize allow_dir '{}': {e}",
                dir.display()
            ))
        })?;
        if !canonical_path.starts_with(&canonical_dir) {
            return Err(DuckAnythingError::PathValidation(format!(
                "'{}' is outside the allowed directory '{}'",
                path.display(),
                dir.display()
            )));
        }
    }
    Ok(())
}

/// Layered runtime configuration.
///
/// Precedence: built-in defaults < environment variables < CLI arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Maximum rows returned by a query. Default: 100.
    pub max_rows: usize,
    /// Query timeout in seconds. Default: 30.
    pub timeout_secs: u64,
    /// Optional directory restriction for data files. Default: None.
    pub allow_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_rows: 100,
            timeout_secs: 30,
            allow_dir: None,
        }
    }
}

/// Partial configuration: only fields explicitly set are applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigPartial {
    pub max_rows: Option<usize>,
    pub timeout_secs: Option<u64>,
    pub allow_dir: Option<PathBuf>,
}

impl Config {
    /// Combines configuration layers: built-in defaults, then `env_override`,
    /// then `cli` (highest precedence).
    pub fn from_layers(env_override: Option<ConfigPartial>, cli: Option<ConfigPartial>) -> Config {
        let mut cfg = Config::default();
        if let Some(env) = env_override {
            cfg.apply(env);
        }
        if let Some(cli) = cli {
            cfg.apply(cli);
        }
        cfg
    }

    /// Loads configuration from the `DUCKANYTHING_*` environment variables,
    /// then applies optional CLI overrides.
    ///
    /// Invalid environment values (e.g. non-numeric) yield a `Config` error.
    pub fn from_env(cli: Option<ConfigPartial>) -> Result<Config, DuckAnythingError> {
        let env_partial = read_env_partial()?;
        Ok(Config::from_layers(Some(env_partial), cli))
    }

    fn apply(&mut self, partial: ConfigPartial) {
        if let Some(v) = partial.max_rows {
            self.max_rows = v;
        }
        if let Some(v) = partial.timeout_secs {
            self.timeout_secs = v;
        }
        if let Some(v) = partial.allow_dir {
            self.allow_dir = Some(v);
        }
    }
}

/// Reads the three `DUCKANYTHING_*` environment variables into a partial
/// config. Missing variables are skipped; invalid numeric values are a
/// `Config` error.
fn read_env_partial() -> Result<ConfigPartial, DuckAnythingError> {
    let mut partial = ConfigPartial::default();
    if let Ok(raw) = env::var(ENV_MAX_ROWS) {
        partial.max_rows = Some(raw.parse::<usize>().map_err(|e| {
            DuckAnythingError::Config(format!("{ENV_MAX_ROWS}='{raw}' is not a valid usize: {e}"))
        })?);
    }
    if let Ok(raw) = env::var(ENV_TIMEOUT_SECS) {
        partial.timeout_secs = Some(raw.parse::<u64>().map_err(|e| {
            DuckAnythingError::Config(format!(
                "{ENV_TIMEOUT_SECS}='{raw}' is not a valid u64: {e}"
            ))
        })?);
    }
    if let Ok(raw) = env::var(ENV_ALLOW_DIR) {
        if !raw.trim().is_empty() {
            partial.allow_dir = Some(PathBuf::from(raw));
        }
    }
    Ok(partial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn readonly_prefixes_pass() {
        for sql in [
            "select 1;",
            "  select 1;  ",
            "with x as (select 1) select * from x",
            "from t",
            "SELECT 1",
            "select 1",
            "  select 1",
            "SeLeCt 1",
            "select 1;;",
        ] {
            assert!(is_readonly_sql(sql), "expected read-only: {sql:?}");
        }
    }

    #[test]
    fn write_statements_rejected() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DROP TABLE t",
            "CREATE TABLE t (a int)",
            "PRAGMA table_info(t)",
            "CALL pragma_version()",
            "ATTACH 'x.db'",
        ] {
            assert!(!is_readonly_sql(sql), "expected rejected: {sql:?}");
        }
    }

    #[test]
    fn empty_or_whitespace_rejected() {
        assert!(!is_readonly_sql(""));
        assert!(!is_readonly_sql("   "));
        assert!(!is_readonly_sql(";"));
    }

    #[test]
    fn validate_path_rejects_dir_and_missing() {
        let dir = tempdir().unwrap();
        // Directory is rejected.
        let err = validate_path(dir.path(), None).unwrap_err();
        assert!(matches!(err, DuckAnythingError::PathValidation(_)));
        // Nonexistent path is rejected.
        let missing = dir.path().join("nope.csv");
        let err = validate_path(&missing, None).unwrap_err();
        assert!(matches!(err, DuckAnythingError::PathValidation(_)));
        // A real file passes.
        let file = dir.path().join("data.csv");
        fs::write(&file, "a,b\n1,2\n").unwrap();
        validate_path(&file, None).unwrap();
    }

    #[test]
    fn allow_dir_whitelist() {
        let root = tempdir().unwrap();
        let allowed = root.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let sub = allowed.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let inside = sub.join("data.csv");
        fs::write(&inside, "x\n1\n").unwrap();
        let outside = root.path().join("outside.csv");
        fs::write(&outside, "x\n1\n").unwrap();

        // Nested file inside the whitelist passes.
        validate_path(&inside, Some(&allowed)).unwrap();
        // File outside the whitelist is rejected.
        let err = validate_path(&outside, Some(&allowed)).unwrap_err();
        assert!(matches!(err, DuckAnythingError::PathValidation(_)));
        // The whitelist dir itself is a directory → rejected.
        let err = validate_path(&allowed, Some(&allowed)).unwrap_err();
        assert!(matches!(err, DuckAnythingError::PathValidation(_)));
    }

    #[test]
    fn no_allow_dir_accepts_any_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("data.csv");
        fs::write(&file, "x\n1\n").unwrap();
        validate_path(&file, None).unwrap();
    }

    #[test]
    fn config_from_layers_priority() {
        let env = ConfigPartial { max_rows: Some(200), ..Default::default() };
        let cli = ConfigPartial {
            max_rows: Some(300),
            timeout_secs: Some(5),
            ..Default::default()
        };
        let cfg = Config::from_layers(Some(env), Some(cli));
        assert_eq!(cfg.max_rows, 300);
        assert_eq!(cfg.timeout_secs, 5);
        assert_eq!(cfg.allow_dir, None);

        // No layers → built-in defaults.
        let cfg = Config::from_layers(None, None);
        assert_eq!(cfg.max_rows, 100);
        assert_eq!(cfg.timeout_secs, 30);
        assert_eq!(cfg.allow_dir, None);
    }

    /// All env-var manipulation lives in this single test so it runs
    /// serially (edition 2024: `set_var`/`remove_var` are unsafe).
    #[test]
    fn config_layers_default_env_cli() {
        unsafe {
            std::env::set_var(ENV_MAX_ROWS, "500");
            std::env::set_var(ENV_TIMEOUT_SECS, "7");
            std::env::set_var(ENV_ALLOW_DIR, "/tmp/duckanything-test");
        }

        // Env alone overrides defaults.
        let cfg = Config::from_env(None).unwrap();
        assert_eq!(cfg.max_rows, 500);
        assert_eq!(cfg.timeout_secs, 7);
        assert_eq!(
            cfg.allow_dir.as_deref(),
            Some(Path::new("/tmp/duckanything-test"))
        );

        // CLI overrides env.
        let cli = ConfigPartial {
            max_rows: Some(42),
            timeout_secs: Some(9),
            allow_dir: Some(PathBuf::from("/tmp/other")),
        };
        let cfg = Config::from_env(Some(cli)).unwrap();
        assert_eq!(cfg.max_rows, 42);
        assert_eq!(cfg.timeout_secs, 9);
        assert_eq!(cfg.allow_dir.as_deref(), Some(Path::new("/tmp/other")));

        // Partial CLI override: only max_rows overridden, env keeps the rest.
        let cli = ConfigPartial { max_rows: Some(1), ..Default::default() };
        let cfg = Config::from_env(Some(cli)).unwrap();
        assert_eq!(cfg.max_rows, 1);
        assert_eq!(cfg.timeout_secs, 7);
        assert_eq!(
            cfg.allow_dir.as_deref(),
            Some(Path::new("/tmp/duckanything-test"))
        );

        // Invalid env value → Config error.
        unsafe {
            std::env::set_var(ENV_MAX_ROWS, "not-a-number");
        }
        let err = Config::from_env(None).unwrap_err();
        assert!(matches!(err, DuckAnythingError::Config(_)));

        // Cleanup so no other test sees these vars.
        unsafe {
            std::env::remove_var(ENV_MAX_ROWS);
            std::env::remove_var(ENV_TIMEOUT_SECS);
            std::env::remove_var(ENV_ALLOW_DIR);
        }
    }
}