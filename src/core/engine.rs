// Wave 3: DuckDB engine wrapper (mount views, run queries).
//
// The engine owns a single in-memory DuckDB connection behind a mutex
// (single-connection serialization; the MCP server is single-client).
// Files are mounted as read-only views over DuckDB's auto-detecting readers
// (`read_csv_auto` / `read_json_auto` / `read_parquet`); queries run with a
// wall-clock timeout enforced via `InterruptHandle` from a worker thread.
//
// Value mapping route: Arrow. `Statement::query_arrow` uses DuckDB's native
// Arrow export, which handles every DuckDB type (DECIMAL, HUGEINT →
// Decimal128(38,0), nested LIST/STRUCT/MAP, ...). The alternative
// `query_map` + `ValueRef` route cannot decode DECIMAL/HUGEINT/INTERVAL/ENUM
// (the scalar converter in duckdb-rs errors on those logical types), so it
// was rejected.
//
// This module is the public API surface for the MCP layer (Wave 4) and is
// not yet called from the binary, so dead-code warnings are suppressed here.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use duckdb::arrow::array::types::{
    Date32Type, Date64Type, Decimal128Type, Decimal256Type, Decimal32Type, Decimal64Type,
    Float16Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
    Time32MillisecondType, Time32SecondType, Time64MicrosecondType, Time64NanosecondType,
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use duckdb::arrow::array::cast::AsArray;
use duckdb::arrow::array::{Array, GenericListArray, OffsetSizeTrait};
use duckdb::arrow::datatypes::{DataType, TimeUnit};
use duckdb::Connection;

use crate::core::error::DuckAnythingError;
use crate::core::format::{Cell, QueryResult};
use crate::core::security::{is_readonly_sql, validate_path, Config};

/// How long to wait for a worker thread to unwind after `interrupt()` before
/// declaring the connection stuck and rebuilding it.
const INTERRUPT_GRACE_SECS: u64 = 5;

/// Supported data file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Csv,
    Json,
    Parquet,
}

impl FileFormat {
    /// DuckDB table function used to read this format.
    fn read_fn(&self) -> &'static str {
        match self {
            FileFormat::Csv => "read_csv_auto",
            FileFormat::Json => "read_json_auto",
            FileFormat::Parquet => "read_parquet",
        }
    }
}

/// Result of a successful mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub view_name: String,
    pub file_path: PathBuf,
    pub format: FileFormat,
}

/// A mounted file record kept by the engine (returned by [`Engine::list_views`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedFile {
    pub view_name: String,
    pub file_path: PathBuf,
    pub format: FileFormat,
}

/// One column of a [`DescribeResult`].
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    /// DuckDB type name as reported by the catalog (e.g. `BIGINT`, `VARCHAR`,
    /// `DECIMAL(18,3)`).
    pub type_name: String,
    /// Up to 3 sample values from the first rows of the view.
    pub samples: Vec<Cell>,
}

/// Result of [`Engine::describe_view`].
#[derive(Debug, Clone, PartialEq)]
pub struct DescribeResult {
    pub view_name: String,
    pub columns: Vec<ColumnInfo>,
}

/// Mutable engine state behind the mutex.
struct EngineInner {
    /// `None` while a query worker thread has taken the connection.
    conn: Option<Connection>,
    /// Interrupt handle for the current connection (refreshed on rebuild).
    interrupt: Arc<duckdb::InterruptHandle>,
    /// Mounted-file records, used by `list_views` and re-mounting on rebuild.
    mounted: Vec<MountedFile>,
    /// Incrementing counter for `view_<N>` fallback names.
    view_counter: usize,
}

/// DuckDB-backed query engine.
///
/// All operations are serialized on a single connection. `run_query` executes
/// on a worker thread so a wall-clock timeout can be enforced with
/// `InterruptHandle::interrupt()`; if the connection does not recover within
/// a grace period it is discarded and rebuilt with all views re-mounted.
pub struct Engine {
    inner: Mutex<EngineInner>,
    timeout_secs: u64,
    allow_dir: Option<PathBuf>,
}

impl Engine {
    /// Opens an in-memory DuckDB connection with the given configuration.
    pub fn new(config: &Config) -> Result<Self, DuckAnythingError> {
        let conn = Connection::open_in_memory()?;
        let interrupt = conn.interrupt_handle();
        Ok(Self {
            inner: Mutex::new(EngineInner {
                conn: Some(conn),
                interrupt,
                mounted: Vec::new(),
                view_counter: 0,
            }),
            timeout_secs: config.timeout_secs,
            allow_dir: config.allow_dir.clone(),
        })
    }

    /// Mounts a data file as a read-only view.
    ///
    /// `alias` (when given) must already be a legal view name
    /// (`[a-z0-9_]+`, not starting with a digit); it is validated but never
    /// silently cleaned. Without an alias the view name is derived from the
    /// file name (see `derive_view_name`).
    pub fn mount_file(
        &self,
        path: &Path,
        alias: Option<&str>,
    ) -> Result<MountInfo, DuckAnythingError> {
        match self.mount_file_impl(path, alias) {
            Ok(info) => {
                tracing::info!(
                    view = %info.view_name,
                    path = %info.file_path.display(),
                    format = ?info.format,
                    "mounted view"
                );
                Ok(info)
            }
            Err(e) => {
                tracing::warn!(error = %e, "mount failed");
                Err(e)
            }
        }
    }

    /// Inner mount implementation; [`Engine::mount_file`] adds logging around
    /// this without touching the error-handling logic.
    fn mount_file_impl(
        &self,
        path: &Path,
        alias: Option<&str>,
    ) -> Result<MountInfo, DuckAnythingError> {
        validate_path(path, self.allow_dir.as_deref())?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let format = match ext.as_str() {
            "csv" => FileFormat::Csv,
            "json" => FileFormat::Json,
            "parquet" => FileFormat::Parquet,
            other => return Err(DuckAnythingError::UnsupportedFormat(other.to_string())),
        };

        let mut guard = self.inner.lock().unwrap();
        let view_name = match alias {
            Some(a) => {
                validate_alias(a)?;
                if guard.mounted.iter().any(|m| m.view_name == a) {
                    return Err(DuckAnythingError::PathValidation(format!(
                        "alias '{a}' conflicts with an existing view"
                    )));
                }
                a.to_string()
            }
            None => {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                // Disjoint field borrows so `view_counter` and `mounted` can be
                // borrowed simultaneously.
                let EngineInner {
                    conn: _,
                    interrupt: _,
                    mounted,
                    view_counter,
                } = &mut *guard;
                derive_view_name(stem, view_counter, mounted)
            }
        };

        // View names are restricted to [a-z0-9_] so double-quoting is safe and
        // sidesteps DuckDB reserved words entirely (e.g. a `select.csv` file
        // mounts as view `select`).
        let sql = format!(
            "CREATE VIEW \"{view_name}\" AS SELECT * FROM {}({})",
            format.read_fn(),
            sql_string_literal(&path.to_string_lossy())
        );
        let conn = guard
            .conn
            .as_ref()
            .ok_or_else(|| DuckAnythingError::Sql("engine connection is busy".to_string()))?;
        conn.execute_batch(&sql)?;

        let info = MountInfo {
            view_name: view_name.clone(),
            file_path: path.to_path_buf(),
            format,
        };
        guard.mounted.push(MountedFile {
            view_name,
            file_path: path.to_path_buf(),
            format,
        });
        Ok(info)
    }

    /// Runs a read-only SQL query with the configured timeout.
    ///
    /// Multi-statement input is rejected up front (see
    /// [`has_multiple_statements`]); `duckdb-rs`'s `prepare` executes
    /// intermediate statements, so relying on it alone would be unsafe.
    pub fn run_query(&self, sql: &str) -> Result<QueryResult, DuckAnythingError> {
        tracing::debug!(sql = %sql, "run_query");
        if !is_readonly_sql(sql) {
            return Err(DuckAnythingError::ReadOnlyViolation(sql.to_string()));
        }
        if has_multiple_statements(sql) {
            return Err(DuckAnythingError::Sql(
                "multiple statements are not allowed in a single query".to_string(),
            ));
        }

        let mut guard = self.inner.lock().unwrap();
        let mut conn = guard.conn.take().ok_or_else(|| {
            DuckAnythingError::Sql("engine connection is busy".to_string())
        })?;
        let interrupt = guard.interrupt.clone();
        let timeout = self.timeout_secs;
        let sql = sql.to_string();

        let (tx, rx) = channel();
        let _ = std::thread::spawn(move || {
            let result = execute_query(&mut conn, &sql);
            let _ = tx.send((result, conn));
        });

        match rx.recv_timeout(Duration::from_secs(timeout)) {
            Ok((result, conn)) => {
                guard.conn = Some(conn);
                result
            }
            Err(RecvTimeoutError::Timeout) => {
                interrupt.interrupt();
                match rx.recv_timeout(Duration::from_secs(INTERRUPT_GRACE_SECS)) {
                    // The worker unwound after the interrupt; the connection
                    // is still usable, only the query was cancelled.
                    Ok((_result, conn)) => {
                        guard.conn = Some(conn);
                        tracing::warn!(timeout_secs = timeout, rebuilt = false, "query timed out");
                        Err(DuckAnythingError::Timeout(timeout))
                    }
                    // The worker is stuck even after interrupt: discard the
                    // old connection (the worker thread keeps it and will
                    // drop it when it eventually finishes) and rebuild.
                    Err(_) => {
                        drop(guard);
                        tracing::warn!(timeout_secs = timeout, rebuilt = true, "query timed out; rebuilding connection");
                        self.rebuild()?;
                        Err(DuckAnythingError::Timeout(timeout))
                    }
                }
            }
            // Worker panicked; the connection was dropped during unwind.
            Err(RecvTimeoutError::Disconnected) => {
                drop(guard);
                tracing::error!("query worker thread panicked; rebuilding connection");
                self.rebuild()?;
                Err(DuckAnythingError::Sql(
                    "query worker thread panicked".to_string(),
                ))
            }
        }
    }

    /// Describes a mounted view: column names, DuckDB type names, and up to
    /// three sample values per column.
    pub fn describe_view(&self, view: &str) -> Result<DescribeResult, DuckAnythingError> {
        // Sample values (respects the configured timeout).
        let sample_sql = format!("SELECT * FROM \"{view}\" LIMIT 3");
        let sample = self.run_query(&sample_sql)?;

        // Native DuckDB type names from the catalog.
        let type_sql = format!(
            "SELECT column_name, data_type FROM duckdb_columns() \
             WHERE table_name = '{}' ORDER BY column_index",
            escape_sql_string(view)
        );
        let type_rows = self.run_query(&type_sql)?;
        let mut type_map: HashMap<String, String> = HashMap::new();
        for row in &type_rows.rows {
            if let (Some(Cell::Text(name)), Some(Cell::Text(ty))) = (row.first(), row.get(1)) {
                type_map.insert(name.clone(), ty.clone());
            }
        }

        let columns = sample
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| ColumnInfo {
                name: name.clone(),
                type_name: type_map
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "UNKNOWN".to_string()),
                samples: sample.rows.iter().take(3).map(|r| r[i].clone()).collect(),
            })
            .collect();

        Ok(DescribeResult {
            view_name: view.to_string(),
            columns,
        })
    }

    /// Returns the currently mounted views.
    pub fn list_views(&self) -> Vec<MountedFile> {
        self.inner.lock().unwrap().mounted.clone()
    }

    /// Discards the current connection and re-creates it with all mounted
    /// views re-mounted. Called when a timed-out query leaves the connection
    /// in an unrecoverable state.
    fn rebuild(&self) -> Result<(), DuckAnythingError> {
        let mut guard = self.inner.lock().unwrap();
        let new_conn = match Connection::open_in_memory() {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "rebuild failed");
                return Err(e.into());
            }
        };
        let interrupt = new_conn.interrupt_handle();
        let mounted = guard.mounted.clone();
        for m in &mounted {
            let sql = format!(
                "CREATE VIEW \"{}\" AS SELECT * FROM {}({})",
                m.view_name,
                m.format.read_fn(),
                sql_string_literal(&m.file_path.to_string_lossy())
            );
            if let Err(e) = new_conn.execute_batch(&sql) {
                tracing::warn!(error = %e, view = %m.view_name, "rebuild failed");
                return Err(e.into());
            }
        }
        guard.conn = Some(new_conn);
        guard.interrupt = interrupt;
        tracing::info!(views = mounted.len(), "engine connection rebuilt");
        Ok(())
    }
}

/// Prepares and executes `sql`, converting the Arrow result into [`QueryResult`].
fn execute_query(conn: &mut Connection, sql: &str) -> Result<QueryResult, DuckAnythingError> {
    let mut stmt = conn.prepare(sql)?;
    let arrow = stmt.query_arrow([])?;
    let schema = arrow.get_schema();
    let columns: Vec<String> = schema.fields().iter().map(|f| f.name().to_string()).collect();

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    for batch in arrow {
        let ncols = batch.num_columns();
        let nrows = batch.num_rows();
        for r in 0..nrows {
            let mut row = Vec::with_capacity(ncols);
            for col in batch.columns() {
                row.push(array_value_to_cell(col.as_ref(), r));
            }
            rows.push(row);
        }
    }
    Ok(QueryResult {
        columns,
        rows,
        truncated: None,
    })
}

// ---------------------------------------------------------------------------
// Arrow → Cell value mapping
// ---------------------------------------------------------------------------

/// Converts one cell of an Arrow array to a [`Cell`].
fn array_value_to_cell(array: &dyn Array, row: usize) -> Cell {
    if array.is_null(row) {
        return Cell::Null;
    }
    match array.data_type() {
        DataType::Null => Cell::Null,
        DataType::Boolean => Cell::Bool(array.as_boolean().value(row)),
        DataType::Int8 => Cell::Int(array.as_primitive::<Int8Type>().value(row) as i64),
        DataType::Int16 => Cell::Int(array.as_primitive::<Int16Type>().value(row) as i64),
        DataType::Int32 => Cell::Int(array.as_primitive::<Int32Type>().value(row) as i64),
        DataType::Int64 => Cell::Int(array.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => Cell::UInt(array.as_primitive::<UInt8Type>().value(row) as u64),
        DataType::UInt16 => Cell::UInt(array.as_primitive::<UInt16Type>().value(row) as u64),
        DataType::UInt32 => Cell::UInt(array.as_primitive::<UInt32Type>().value(row) as u64),
        DataType::UInt64 => Cell::UInt(array.as_primitive::<UInt64Type>().value(row)),
        DataType::Float16 => Cell::Float(array.as_primitive::<Float16Type>().value(row).to_f64()),
        DataType::Float32 => Cell::Float(array.as_primitive::<Float32Type>().value(row) as f64),
        DataType::Float64 => Cell::Float(array.as_primitive::<Float64Type>().value(row)),
        DataType::Utf8 => Cell::Text(array.as_string::<i32>().value(row).to_string()),
        DataType::LargeUtf8 => Cell::Text(array.as_string::<i64>().value(row).to_string()),
        DataType::Utf8View => Cell::Text(array.as_string_view().value(row).to_string()),
        DataType::Binary => Cell::Blob(array.as_binary::<i32>().value(row).to_vec()),
        DataType::LargeBinary => Cell::Blob(array.as_binary::<i64>().value(row).to_vec()),
        DataType::BinaryView => Cell::Blob(array.as_binary_view().value(row).to_vec()),
        DataType::FixedSizeBinary(_) => {
            Cell::Blob(array.as_fixed_size_binary().value(row).to_vec())
        }
        DataType::Date32 => {
            Cell::Date(format_iso_date(array.as_primitive::<Date32Type>().value(row) as i64))
        }
        DataType::Date64 => {
            let millis = array.as_primitive::<Date64Type>().value(row);
            Cell::Date(format_iso_date(millis.div_euclid(86_400_000)))
        }
        DataType::Timestamp(unit, _) => {
            let (secs, subsec_nanos) = match unit {
                TimeUnit::Second => {
                    let v = array.as_primitive::<TimestampSecondType>().value(row);
                    (v, 0)
                }
                TimeUnit::Millisecond => {
                    let v = array.as_primitive::<TimestampMillisecondType>().value(row);
                    (v.div_euclid(1000), (v.rem_euclid(1000) * 1_000_000) as u32)
                }
                TimeUnit::Microsecond => {
                    let v = array.as_primitive::<TimestampMicrosecondType>().value(row);
                    (v.div_euclid(1_000_000), (v.rem_euclid(1_000_000) * 1000) as u32)
                }
                TimeUnit::Nanosecond => {
                    let v = array.as_primitive::<TimestampNanosecondType>().value(row);
                    (v.div_euclid(1_000_000_000), v.rem_euclid(1_000_000_000) as u32)
                }
            };
            Cell::Timestamp(format_iso_timestamp(secs, subsec_nanos))
        }
        DataType::Time32(unit) => {
            let nanos = match unit {
                TimeUnit::Second => {
                    array.as_primitive::<Time32SecondType>().value(row) as i64 * 1_000_000_000
                }
                TimeUnit::Millisecond => {
                    array.as_primitive::<Time32MillisecondType>().value(row) as i64 * 1_000_000
                }
                // Time32 only supports Second/Millisecond per the Arrow spec.
                _ => 0,
            };
            Cell::Time(format_iso_time(nanos))
        }
        DataType::Time64(unit) => {
            let nanos = match unit {
                TimeUnit::Microsecond => {
                    array.as_primitive::<Time64MicrosecondType>().value(row) * 1000
                }
                TimeUnit::Nanosecond => array.as_primitive::<Time64NanosecondType>().value(row),
                // Time64 only supports Microsecond/Nanosecond per the Arrow spec.
                _ => 0,
            };
            Cell::Time(format_iso_time(nanos))
        }
        DataType::Decimal32(_, scale) => Cell::Decimal(format_decimal_str(
            &array.as_primitive::<Decimal32Type>().value(row).to_string(),
            *scale as u8,
        )),
        DataType::Decimal64(_, scale) => Cell::Decimal(format_decimal_str(
            &array.as_primitive::<Decimal64Type>().value(row).to_string(),
            *scale as u8,
        )),
        DataType::Decimal128(_, scale) => Cell::Decimal(format_decimal_str(
            &array.as_primitive::<Decimal128Type>().value(row).to_string(),
            *scale as u8,
        )),
        DataType::Decimal256(_, scale) => Cell::Decimal(format_decimal_str(
            &array.as_primitive::<Decimal256Type>().value(row).to_string(),
            *scale as u8,
        )),
        DataType::List(_) => Cell::List(list_to_cells(array.as_list::<i32>(), row)),
        DataType::LargeList(_) => Cell::List(list_to_cells(array.as_list::<i64>(), row)),
        DataType::FixedSizeList(_, _) => {
            let values = array.as_fixed_size_list().value(row);
            Cell::List(
                (0..values.len())
                    .map(|i| array_value_to_cell(values.as_ref(), i))
                    .collect(),
            )
        }
        DataType::Struct(_) => {
            let s = array.as_struct();
            let names = s.column_names();
            let fields: Vec<(String, Cell)> = names
                .iter()
                .enumerate()
                .map(|(i, name)| (name.to_string(), array_value_to_cell(s.column(i).as_ref(), row)))
                .collect();
            Cell::Struct(fields)
        }
        DataType::Map(_, _) => {
            let entries = array.as_map().value(row); // StructArray, one row per entry
            let nrows = entries.len();
            let mut list = Vec::with_capacity(nrows);
            for r in 0..nrows {
                let fields = vec![
                    (
                        "key".to_string(),
                        array_value_to_cell(entries.column(0).as_ref(), r),
                    ),
                    (
                        "value".to_string(),
                        array_value_to_cell(entries.column(1).as_ref(), r),
                    ),
                ];
                list.push(Cell::Struct(fields));
            }
            Cell::List(list)
        }
        DataType::Dictionary(_, _) => dictionary_value(array, row),
        // Rare in CSV/JSON/Parquet workflows; degrade to a Text placeholder.
        // TODO: map Interval/Duration/Union/RunEndEncoded/ListView to richer
        // Cell representations if these types become relevant.
        DataType::Interval(_)
        | DataType::Duration(_)
        | DataType::Union(_, _)
        | DataType::RunEndEncoded(_, _)
        | DataType::ListView(_)
        | DataType::LargeListView(_) => Cell::Text(format!("<{}>", array.data_type())),
    }
}

/// Converts a list row into a `Vec<Cell>` (recursive).
fn list_to_cells<O: OffsetSizeTrait>(list: &GenericListArray<O>, row: usize) -> Vec<Cell> {
    let len = list.value_length(row).to_usize().unwrap_or(0);
    let values = list.value(row);
    (0..len)
        .map(|i| array_value_to_cell(values.as_ref(), i))
        .collect()
}

/// Converts a dictionary-encoded value by looking up the key in the values array.
fn dictionary_value(array: &dyn Array, row: usize) -> Cell {
    let dict = array.as_any_dictionary();
    let keys = dict.keys();
    let values = dict.values();
    let key: Option<usize> = match keys.data_type() {
        DataType::Int8 => Some(keys.as_primitive::<Int8Type>().value(row) as usize),
        DataType::Int16 => Some(keys.as_primitive::<Int16Type>().value(row) as usize),
        DataType::Int32 => Some(keys.as_primitive::<Int32Type>().value(row) as usize),
        DataType::Int64 => Some(keys.as_primitive::<Int64Type>().value(row) as usize),
        DataType::UInt8 => Some(keys.as_primitive::<UInt8Type>().value(row) as usize),
        DataType::UInt16 => Some(keys.as_primitive::<UInt16Type>().value(row) as usize),
        DataType::UInt32 => Some(keys.as_primitive::<UInt32Type>().value(row) as usize),
        DataType::UInt64 => Some(keys.as_primitive::<UInt64Type>().value(row) as usize),
        _ => None,
    };
    match key {
        Some(k) if k < values.len() => array_value_to_cell(values.as_ref(), k),
        _ => Cell::Null,
    }
}

// ---------------------------------------------------------------------------
// View-name derivation and validation
// ---------------------------------------------------------------------------

/// Applies the view-name cleaning pipeline: non `[a-zA-Z0-9_]` → `_`,
/// collapse runs of `_`, trim leading/trailing `_`, lowercase.
fn clean_view_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Derives a unique view name from a file stem.
///
/// Empty or digit-leading cleaned names fall back to `view_<N>` with an
/// incrementing counter; collisions with existing views get `_2`, `_3`, ...
fn derive_view_name(stem: &str, counter: &mut usize, mounted: &[MountedFile]) -> String {
    let cleaned = clean_view_name(stem);
    let used = |name: &str| mounted.iter().any(|m| m.view_name == name);
    if cleaned.is_empty() || cleaned.starts_with(|c: char| c.is_ascii_digit()) {
        loop {
            *counter += 1;
            let candidate = format!("view_{}", *counter);
            if !used(&candidate) {
                return candidate;
            }
        }
    } else {
        let mut name = cleaned.clone();
        let mut n = 2;
        while used(&name) {
            name = format!("{cleaned}_{n}");
            n += 1;
        }
        name
    }
}

/// Validates an explicit alias: it must already be in legal view-name form
/// (`[a-z0-9_]+`, non-empty, not starting with a digit). Aliases are never
/// silently cleaned.
fn validate_alias(alias: &str) -> Result<(), DuckAnythingError> {
    if alias.is_empty() {
        return Err(DuckAnythingError::PathValidation(
            "alias must not be empty".to_string(),
        ));
    }
    if clean_view_name(alias) != alias || alias.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(DuckAnythingError::PathValidation(format!(
            "alias '{alias}' is not a valid view name \
             (must match [a-z0-9_]+ and not start with a digit)"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-statement detection
// ---------------------------------------------------------------------------

/// Returns `true` if `sql` contains more than one statement.
///
/// `duckdb-rs`'s `Connection::prepare` executes all intermediate statements
/// of a multi-statement script (see `inner_connection.rs`), so the engine
/// must reject multi-statement input itself. The scan tracks single-quoted
/// string literals, double-quoted identifiers, `--` line comments and
/// `/* */` block comments; a `;` outside those that is followed by anything
/// other than whitespace/comments marks a second statement.
fn has_multiple_statements(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'-' if bytes.get(i + 1) == Some(&b'-') => in_line_comment = true,
            b'/' if bytes.get(i + 1) == Some(&b'*') => in_block_comment = true,
            b';' if !rest_is_empty_or_comment(&bytes[i + 1..]) => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Whether the bytes after a `;` contain only whitespace and/or comments.
fn rest_is_empty_or_comment(rest: &[u8]) -> bool {
    let mut i = 0;
    loop {
        while i < rest.len() && rest[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= rest.len() {
            return true;
        }
        if rest[i..].starts_with(b"--") {
            while i < rest.len() && rest[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if rest[i..].starts_with(b"/*") {
            match rest[i + 2..].windows(2).position(|w| w == b"*/") {
                Some(pos) => {
                    i += 2 + pos + 2;
                    continue;
                }
                None => return false, // unterminated block comment counts as content
            }
        }
        return false;
    }
}

// ---------------------------------------------------------------------------
// SQL literal helpers
// ---------------------------------------------------------------------------

/// Wraps `s` in single quotes, doubling embedded quotes (SQL string literal).
fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Escapes a string for use inside a single-quoted SQL literal.
fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

// ---------------------------------------------------------------------------
// Date / time / decimal formatting (dependency-free)
// ---------------------------------------------------------------------------

/// Formats a decimal integer string with the given scale (e.g. `"12345"`, 2
/// → `"123.45"`).
fn format_decimal_str(int_str: &str, scale: u8) -> String {
    if scale == 0 {
        return int_str.to_string();
    }
    let negative = int_str.starts_with('-');
    let digits = int_str.trim_start_matches('-');
    let scale = scale as usize;
    if digits.len() <= scale {
        let mut s = String::with_capacity(scale + 2);
        if negative {
            s.push('-');
        }
        s.push_str("0.");
        for _ in 0..(scale - digits.len()) {
            s.push('0');
        }
        s.push_str(digits);
        s
    } else {
        let (int_part, frac_part) = digits.split_at(digits.len() - scale);
        let mut s = String::with_capacity(digits.len() + 1);
        if negative {
            s.push('-');
        }
        s.push_str(int_part);
        s.push('.');
        s.push_str(frac_part);
        s
    }
}

/// Formats an ISO-8601 date from days since the Unix epoch.
fn format_iso_date(days_since_epoch: i64) -> String {
    let (y, m, d) = civil_from_days(days_since_epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats an ISO-8601 timestamp (naive UTC) from seconds since the epoch
/// plus sub-second nanoseconds.
fn format_iso_timestamp(secs: i64, subsec_nanos: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    let mut out = format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
    if subsec_nanos > 0 {
        let mut frac = format!("{subsec_nanos:09}");
        while frac.ends_with('0') {
            frac.pop();
        }
        out.push('.');
        out.push_str(&frac);
    }
    out
}

/// Formats an ISO-8601 time from nanoseconds since midnight.
fn format_iso_time(nanos_since_midnight: i64) -> String {
    let h = nanos_since_midnight / 3_600_000_000_000;
    let rem = nanos_since_midnight % 3_600_000_000_000;
    let mi = rem / 60_000_000_000;
    let s = rem % 60_000_000_000;
    let secs = s / 1_000_000_000;
    let nanos = (s % 1_000_000_000) as u32;
    let mut out = format!("{h:02}:{mi:02}:{secs:02}");
    if nanos > 0 {
        let mut frac = format!("{nanos:09}");
        while frac.ends_with('0') {
            frac.pop();
        }
        out.push('.');
        out.push_str(&frac);
    }
    out
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's
/// civil-from-days algorithm; handles negative epochs correctly.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Test-only: run a script on the engine's raw connection (used to
    /// generate fixtures such as Parquet files).
    impl Engine {
        fn execute_script(&self, sql: &str) -> Result<(), DuckAnythingError> {
            let mut guard = self.inner.lock().unwrap();
            let conn = guard.conn.as_mut().ok_or_else(|| {
                DuckAnythingError::Sql("engine connection is busy".to_string())
            })?;
            conn.execute_batch(sql)?;
            Ok(())
        }
    }

    #[test]
    fn mount_csv_and_json_then_query() {
        let dir = tempdir().unwrap();
        let csv = dir.path().join("data.csv");
        fs::write(&csv, "a,b\n1,x\n2,y\n").unwrap();
        let json = dir.path().join("people.json");
        fs::write(&json, "[{\"a\":1,\"b\":\"x\"}]").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        let info = engine.mount_file(&csv, None).unwrap();
        assert_eq!(info.view_name, "data");
        assert_eq!(info.format, FileFormat::Csv);
        let info = engine.mount_file(&json, None).unwrap();
        assert_eq!(info.view_name, "people");
        assert_eq!(info.format, FileFormat::Json);

        let result = engine.run_query("SELECT * FROM data").unwrap();
        assert_eq!(result.columns, vec!["a", "b"]);
        assert_eq!(
            result.rows,
            vec![
                vec![Cell::Int(1), Cell::Text("x".into())],
                vec![Cell::Int(2), Cell::Text("y".into())],
            ]
        );

        let result = engine.run_query("SELECT * FROM people").unwrap();
        assert_eq!(result.columns, vec!["a", "b"]);
        assert_eq!(result.rows, vec![vec![Cell::Int(1), Cell::Text("x".into())]]);
    }

    #[test]
    fn mount_parquet() {
        let dir = tempdir().unwrap();
        let pq = dir.path().join("data.parquet");
        let engine = Engine::new(&Config::default()).unwrap();
        // Generate the fixture with the engine's own connection.
        engine
            .execute_script(&format!(
                "COPY (SELECT 1 AS a, 'x' AS b) TO '{}' (FORMAT PARQUET)",
                pq.display()
            ))
            .unwrap();

        let info = engine.mount_file(&pq, None).unwrap();
        assert_eq!(info.view_name, "data");
        assert_eq!(info.format, FileFormat::Parquet);

        let result = engine.run_query("SELECT * FROM data").unwrap();
        assert_eq!(result.columns, vec!["a", "b"]);
        assert_eq!(result.rows, vec![vec![Cell::Int(1), Cell::Text("x".into())]]);
    }

    #[test]
    fn unsupported_formats_rejected() {
        let dir = tempdir().unwrap();
        let xlsx = dir.path().join("book.xlsx");
        fs::write(&xlsx, "x").unwrap();
        let txt = dir.path().join("notes.txt");
        fs::write(&txt, "x").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        let err = engine.mount_file(&xlsx, None).unwrap_err();
        assert!(matches!(err, DuckAnythingError::UnsupportedFormat(_)), "{err:?}");
        let err = engine.mount_file(&txt, None).unwrap_err();
        assert!(matches!(err, DuckAnythingError::UnsupportedFormat(_)), "{err:?}");
    }

    #[test]
    fn view_name_derivation() {
        let dir = tempdir().unwrap();
        let data = dir.path().join("data.csv");
        fs::write(&data, "a\n1").unwrap();
        let mydata = dir.path().join("my-data.json");
        fs::write(&mydata, "[{\"a\":1}]").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        assert_eq!(engine.mount_file(&data, None).unwrap().view_name, "data");
        assert_eq!(engine.mount_file(&mydata, None).unwrap().view_name, "my_data");
    }

    #[test]
    fn non_ascii_digit_leading_name_falls_back() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("2023Q4 报表.csv");
        fs::write(&f, "a\n1").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        assert_eq!(engine.mount_file(&f, None).unwrap().view_name, "view_1");
    }

    #[test]
    fn digit_leading_name_falls_back() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("2023.csv");
        fs::write(&f, "a\n1").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        assert_eq!(engine.mount_file(&f, None).unwrap().view_name, "view_1");
    }

    #[test]
    fn same_stem_in_different_dirs_gets_suffix() {
        let dir = tempdir().unwrap();
        let d1 = dir.path().join("one");
        let d2 = dir.path().join("two");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        let f1 = d1.join("data.csv");
        let f2 = d2.join("data.csv");
        fs::write(&f1, "a\n1").unwrap();
        fs::write(&f2, "a\n2").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        assert_eq!(engine.mount_file(&f1, None).unwrap().view_name, "data");
        assert_eq!(engine.mount_file(&f2, None).unwrap().view_name, "data_2");
    }

    #[test]
    fn reserved_word_view_name_is_queryable() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("select.csv");
        fs::write(&f, "a\n1").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        let info = engine.mount_file(&f, None).unwrap();
        assert_eq!(info.view_name, "select");
        // CREATE VIEW uses double quotes, so reserved words are fine.
        let result = engine.run_query("SELECT * FROM \"select\"").unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn alias_validation() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("data.csv");
        fs::write(&f, "a\n1").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        // A legal alias is used verbatim.
        let info = engine.mount_file(&f, Some("my_data")).unwrap();
        assert_eq!(info.view_name, "my_data");
        // An illegal alias is rejected without silent cleaning.
        let err = engine.mount_file(&f, Some("my alias!")).unwrap_err();
        assert!(matches!(err, DuckAnythingError::PathValidation(_)), "{err:?}");
        // An alias colliding with an existing view is rejected too.
        let err = engine.mount_file(&f, Some("my_data")).unwrap_err();
        assert!(matches!(err, DuckAnythingError::PathValidation(_)), "{err:?}");
    }

    #[test]
    fn select_all_returns_expected_columns_and_rows() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("data.csv");
        fs::write(&f, "a,b\n1,x\n2,y\n3,z\n").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        engine.mount_file(&f, None).unwrap();
        let result = engine.run_query("SELECT * FROM data").unwrap();
        assert_eq!(result.columns, vec!["a", "b"]);
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[2], vec![Cell::Int(3), Cell::Text("z".into())]);
        assert!(result.truncated.is_none());
    }

    #[test]
    fn multi_statement_rejected() {
        let engine = Engine::new(&Config::default()).unwrap();
        let err = engine.run_query("SELECT 1; DROP VIEW x").unwrap_err();
        assert!(matches!(err, DuckAnythingError::Sql(_)), "{err:?}");
    }

    #[test]
    fn write_statement_rejected() {
        let engine = Engine::new(&Config::default()).unwrap();
        let err = engine.run_query("DROP TABLE t").unwrap_err();
        assert!(matches!(err, DuckAnythingError::ReadOnlyViolation(_)), "{err:?}");
    }

    #[test]
    fn query_timeout_then_connection_recovers() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("data.csv");
        fs::write(&f, "a\n1\n2\n3\n").unwrap();

        let mut config = Config::default();
        config.timeout_secs = 1;
        let engine = Engine::new(&config).unwrap();
        engine.mount_file(&f, None).unwrap();

        let err = engine.run_query("SELECT count(*) FROM range(10000000000)").unwrap_err();
        assert!(matches!(err, DuckAnythingError::Timeout(1)), "{err:?}");

        // The connection must still be usable afterwards.
        let result = engine.run_query("SELECT 1 AS x").unwrap();
        assert_eq!(result.rows[0][0], Cell::Int(1));

        // The mounted view must still be queryable (rebuild path re-mounts).
        let result = engine.run_query("SELECT * FROM data").unwrap();
        assert_eq!(result.rows.len(), 3);
    }

    #[test]
    fn list_views_records_mounts() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("data.csv");
        fs::write(&f, "a\n1").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        assert!(engine.list_views().is_empty());
        engine.mount_file(&f, None).unwrap();

        let views = engine.list_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].view_name, "data");
        assert_eq!(views[0].file_path, f);
        assert_eq!(views[0].format, FileFormat::Csv);
    }

    #[test]
    fn describe_view_reports_columns_types_samples() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("data.csv");
        fs::write(&f, "a,b\n1,x\n2,y\n3,z\n").unwrap();

        let engine = Engine::new(&Config::default()).unwrap();
        engine.mount_file(&f, None).unwrap();
        let desc = engine.describe_view("data").unwrap();
        assert_eq!(desc.view_name, "data");
        assert_eq!(desc.columns.len(), 2);
        assert_eq!(desc.columns[0].name, "a");
        assert_eq!(desc.columns[0].type_name, "BIGINT");
        assert_eq!(
            desc.columns[0].samples,
            vec![Cell::Int(1), Cell::Int(2), Cell::Int(3)]
        );
        assert_eq!(desc.columns[1].name, "b");
        assert_eq!(desc.columns[1].type_name, "VARCHAR");
        assert_eq!(
            desc.columns[1].samples,
            vec![
                Cell::Text("x".into()),
                Cell::Text("y".into()),
                Cell::Text("z".into())
            ]
        );
    }

    #[test]
    fn multi_statement_scanner() {
        assert!(!has_multiple_statements("SELECT 1"));
        assert!(!has_multiple_statements("SELECT 1;"));
        assert!(!has_multiple_statements("SELECT ';' AS x"));
        assert!(!has_multiple_statements("SELECT \"a;b\" FROM t"));
        assert!(!has_multiple_statements("SELECT 1; -- trailing comment"));
        assert!(!has_multiple_statements("SELECT 1; /* trailing */"));
        assert!(!has_multiple_statements("SELECT 1; /* trailing */"));
        assert!(has_multiple_statements("SELECT 1; SELECT 2"));
        assert!(has_multiple_statements("SELECT 1; DROP VIEW x"));
        assert!(has_multiple_statements("SELECT 1;\nSELECT 2"));
        assert!(has_multiple_statements("SELECT 1 /* c */ ; SELECT 2"));
    }

    #[test]
    fn date_time_decimal_formatting() {
        assert_eq!(format_iso_date(0), "1970-01-01");
        assert_eq!(format_iso_date(19_723), "2024-01-01");
        assert_eq!(format_iso_date(-1), "1969-12-31");
        assert_eq!(format_iso_timestamp(0, 0), "1970-01-01T00:00:00");
        assert_eq!(format_iso_timestamp(1_700_000_000, 123_000_000), "2023-11-14T22:13:20.123");
        assert_eq!(format_iso_time(36_000_000_000_000), "10:00:00");
        assert_eq!(format_iso_time(36_000_000_000_500), "10:00:00.0000005");
        assert_eq!(format_decimal_str("12345", 2), "123.45");
        assert_eq!(format_decimal_str("5", 2), "0.05");
        assert_eq!(format_decimal_str("-5", 2), "-0.05");
        assert_eq!(format_decimal_str("12345", 0), "12345");
    }
}