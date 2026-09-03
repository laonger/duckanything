//! Wave 2 Lane B: result formatting and type mapping.
//!
//! Converts a neutral [`QueryResult`] (column names + [`Cell`] rows) into one
//! of four text representations: JSON, aligned table, Markdown, or CSV.
//!
//! The engine layer is responsible for translating DuckDB/Arrow values into
//! [`Cell`]s; this module only formats them.
//!
//! This module is the public API surface for the engine layer (Wave 3) and is
//! not yet called from the binary, so dead-code warnings are suppressed here.

#![allow(dead_code)]

use crate::core::error::{DuckAnythingError, RowLimitExceeded};

/// Neutral row value representation (engine layer converts Arrow/DuckDB
/// values into this).
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// SQL NULL.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    Int(i64),
    /// Unsigned 64-bit integer.
    UInt(u64),
    /// Floating point — internal computation only; rendered as string on output.
    Float(f64),
    /// Exact numeric value carried as a string.
    Decimal(String),
    /// Text.
    Text(String),
    /// ISO 8601 date.
    Date(String),
    /// ISO 8601 timestamp.
    Timestamp(String),
    /// ISO 8601 time.
    Time(String),
    /// Binary data — rendered as lowercase hex.
    Blob(Vec<u8>),
    /// Rendered as a JSON array.
    List(Vec<Cell>),
    /// Rendered as a JSON object.
    Struct(Vec<(String, Cell)>),
}

/// A query result: column names plus row data.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Set when `max_rows` truncation happened during formatting.
    pub truncated: Option<RowLimitExceeded>,
}

/// Output representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Table,
    Markdown,
    Csv,
}

/// 2^53 — the largest integer exactly representable in a JS `Number`.
/// Values with magnitude >= this are rendered as strings in JSON output.
const JS_SAFE_INT: u64 = 1 << 53;

/// Format a query result.
///
/// `max_rows` truncation is applied here: if `result.rows` has more than
/// `max_rows` rows, it is truncated in place and `result.truncated` records
/// `RowLimitExceeded { returned, total }`. Truncation information is never
/// included in the returned text — the caller reads `result.truncated`.
pub fn format_result(
    result: &mut QueryResult,
    format: Format,
    max_rows: usize,
) -> Result<String, DuckAnythingError> {
    let total = result.rows.len();
    if total > max_rows {
        result.truncated = Some(RowLimitExceeded { returned: max_rows, total });
        result.rows.truncate(max_rows);
    }
    match format {
        Format::Json => format_json(result),
        Format::Table => Ok(format_table(result)),
        Format::Markdown => Ok(format_markdown(result)),
        Format::Csv => Ok(format_csv(result)),
    }
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

fn format_json(result: &QueryResult) -> Result<String, DuckAnythingError> {
    let rows: Vec<serde_json::Value> = result
        .rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (col, cell) in result.columns.iter().zip(row) {
                obj.insert(col.clone(), cell_to_json(cell));
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    Ok(json_value_to_string(&serde_json::Value::Array(rows)))
}

/// Map a [`Cell`] to a JSON value following the JS-safe-number rules:
/// integers with |n| >= 2^53 become strings, floats become strings.
fn cell_to_json(cell: &Cell) -> serde_json::Value {
    match cell {
        Cell::Null => serde_json::Value::Null,
        Cell::Bool(b) => serde_json::Value::Bool(*b),
        Cell::Int(n) => {
            if n.unsigned_abs() >= JS_SAFE_INT {
                serde_json::Value::String(n.to_string())
            } else {
                serde_json::Value::Number((*n).into())
            }
        }
        Cell::UInt(n) => {
            if *n >= JS_SAFE_INT {
                serde_json::Value::String(n.to_string())
            } else {
                serde_json::Value::Number((*n).into())
            }
        }
        Cell::Float(f) => serde_json::Value::String(format!("{f}")),
        Cell::Decimal(s) => serde_json::Value::String(s.clone()),
        Cell::Text(s) => serde_json::Value::String(s.clone()),
        Cell::Date(s) | Cell::Timestamp(s) | Cell::Time(s) => serde_json::Value::String(s.clone()),
        Cell::Blob(bytes) => serde_json::Value::String(to_hex(bytes)),
        Cell::List(cells) => serde_json::Value::Array(cells.iter().map(cell_to_json).collect()),
        Cell::Struct(fields) => {
            let mut obj = serde_json::Map::new();
            for (name, value) in fields {
                obj.insert(name.clone(), cell_to_json(value));
            }
            serde_json::Value::Object(obj)
        }
    }
}

/// Serialize a JSON value. Cannot fail: every value produced by
/// [`cell_to_json`] is JSON-safe (floats are stringified first).
fn json_value_to_string(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("JSON output is always serializable")
}

// ---------------------------------------------------------------------------
// Table / Markdown / CSV
// ---------------------------------------------------------------------------

/// Text rendering of a cell (used by Table, Markdown and CSV).
fn cell_to_string(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(n) => n.to_string(),
        Cell::UInt(n) => n.to_string(),
        Cell::Float(f) => format!("{f}"),
        Cell::Decimal(s) => s.clone(),
        Cell::Text(s) => s.clone(),
        Cell::Date(s) | Cell::Timestamp(s) | Cell::Time(s) => s.clone(),
        Cell::Blob(bytes) => to_hex(bytes),
        Cell::List(cells) => {
            json_value_to_string(&serde_json::Value::Array(cells.iter().map(cell_to_json).collect()))
        }
        Cell::Struct(fields) => {
            let mut obj = serde_json::Map::new();
            for (name, value) in fields {
                obj.insert(name.clone(), cell_to_json(value));
            }
            json_value_to_string(&serde_json::Value::Object(obj))
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Aligned plain-text table: header, dash separator, then data rows.
/// Empty results render as `(0 rows)`.
fn format_table(result: &QueryResult) -> String {
    if result.rows.is_empty() {
        return "(0 rows)".to_string();
    }
    let widths = column_widths(result);
    let mut lines: Vec<String> = Vec::with_capacity(result.rows.len() + 2);

    let mut header = String::new();
    for (i, col) in result.columns.iter().enumerate() {
        if i > 0 {
            header.push(' ');
        }
        header.push_str(&pad(col, widths[i]));
    }
    lines.push(header.trim_end().to_string());

    let mut sep = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            sep.push(' ');
        }
        sep.push_str(&"-".repeat(*w));
    }
    lines.push(sep.trim_end().to_string());

    for row in &result.rows {
        let mut line = String::new();
        for (i, (cell, w)) in row.iter().zip(&widths).enumerate() {
            if i > 0 {
                line.push(' ');
            }
            line.push_str(&pad(&cell_to_string(cell), *w));
        }
        lines.push(line.trim_end().to_string());
    }

    lines.join("\n")
}

/// Per-column display width: max over header and cells.
fn column_widths(result: &QueryResult) -> Vec<usize> {
    result
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let mut w = display_width(col);
            for row in &result.rows {
                if let Some(cell) = row.get(i) {
                    w = w.max(display_width(&cell_to_string(cell)));
                }
            }
            w
        })
        .collect()
}

/// Right-pad `s` to `width` display columns.
fn pad(s: &str, width: usize) -> String {
    let mut out = String::with_capacity(s.len() + width);
    out.push_str(s);
    for _ in display_width(s)..width {
        out.push(' ');
    }
    out
}

/// Approximate display width: East Asian wide/fullwidth chars count as 2.
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

/// East Asian wide/fullwidth ranges (wcwidth-style approximation).
fn is_wide(c: char) -> bool {
    matches!(c,
        '\u{1100}'..='\u{115F}'   // Hangul Jamo
        | '\u{2E80}'..='\u{303E}' // CJK Radicals .. CJK Symbols and Punctuation
        | '\u{3041}'..='\u{33FF}' // Hiragana .. CJK Compatibility
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
        | '\u{A000}'..='\u{A4CF}' // Yi Syllables
        | '\u{AC00}'..='\u{D7A3}' // Hangul Syllables
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{FE30}'..='\u{FE4F}' // CJK Compatibility Forms
        | '\u{FF00}'..='\u{FF60}' // Fullwidth Forms
        | '\u{FFE0}'..='\u{FFE6}' // Fullwidth Signs
    )
}

/// GitHub-flavored Markdown table: `| a | b |` header, `|---|---|` separator.
fn format_markdown(result: &QueryResult) -> String {
    let mut out = String::new();

    out.push('|');
    for col in &result.columns {
        out.push(' ');
        out.push_str(&col.replace('|', "\\|"));
        out.push_str(" |");
    }
    out.push('\n');

    out.push('|');
    for _ in &result.columns {
        out.push_str("---|");
    }
    out.push('\n');

    for row in &result.rows {
        out.push('|');
        for cell in row {
            out.push(' ');
            out.push_str(&cell_to_string(cell).replace('|', "\\|"));
            out.push_str(" |");
        }
        out.push('\n');
    }

    out.pop(); // drop trailing newline
    out
}

/// RFC 4180 CSV: header row first, values containing a comma, quote, or line
/// break are wrapped in double quotes with embedded quotes doubled.
fn format_csv(result: &QueryResult) -> String {
    let mut out = String::new();
    for (i, col) in result.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_escape(col));
    }
    for row in &result.rows {
        out.push('\n');
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&csv_escape(&cell_to_string(cell)));
        }
    }
    out
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(columns: &[&str], rows: Vec<Vec<Cell>>) -> QueryResult {
        QueryResult {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows,
            truncated: None,
        }
    }

    // 1. Null → JSON null
    #[test]
    fn json_null_is_null() {
        let mut r = make_result(&["a"], vec![vec![Cell::Null]]);
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        assert_eq!(out, r#"[{"a":null}]"#);
    }

    // 2. Int/UInt follow the JS-safe-number rule (|n| >= 2^53 → string)
    #[test]
    fn json_ints_follow_js_safe_range() {
        let mut r = make_result(&["i", "u"], vec![vec![Cell::Int(42), Cell::UInt(42)]]);
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        assert_eq!(out, r#"[{"i":42,"u":42}]"#);

        let mut r = make_result(
            &["i", "u"],
            vec![vec![Cell::Int(1 << 53), Cell::UInt(1 << 53)]],
        );
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        assert_eq!(out, r#"[{"i":"9007199254740992","u":"9007199254740992"}]"#);

        let mut r = make_result(&["i"], vec![vec![Cell::Int(-(1 << 53))]]);
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        assert_eq!(out, r#"[{"i":"-9007199254740992"}]"#);
    }

    // 3. Float → string without precision noise; Decimal stays a string
    #[test]
    fn json_float_and_decimal_are_strings() {
        let mut r = make_result(
            &["f", "d"],
            vec![vec![Cell::Float(0.1 + 0.2), Cell::Decimal("1.10".to_string())]],
        );
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        // Compare parsed values: serde_json::Map sorts keys (BTreeMap), so
        // raw-string comparison would depend on key order.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v,
            serde_json::json!([{"f": "0.30000000000000004", "d": "1.10"}])
        );
    }

    // 4. Timestamp → JSON string
    #[test]
    fn json_timestamp_is_string() {
        let mut r = make_result(
            &["ts"],
            vec![vec![Cell::Timestamp("2024-01-01T00:00:00".to_string())]],
        );
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        assert_eq!(out, r#"[{"ts":"2024-01-01T00:00:00"}]"#);
    }

    // 5. List → array, Struct → object, Blob → hex
    #[test]
    fn json_list_struct_blob() {
        let mut r = make_result(
            &["l", "s", "b"],
            vec![vec![
                Cell::List(vec![Cell::Int(1), Cell::Int(2)]),
                Cell::Struct(vec![("a".to_string(), Cell::Int(1))]),
                Cell::Blob(vec![0xde, 0xad]),
            ]],
        );
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v,
            serde_json::json!([{"l": [1, 2], "s": {"a": 1}, "b": "dead"}])
        );
    }

    // 6. JSON overall structure: one object per row, column names as keys
    #[test]
    fn json_two_rows_objects_keyed_by_column() {
        let mut r = make_result(
            &["a", "b"],
            vec![
                vec![Cell::Int(1), Cell::Text("x".to_string())],
                vec![Cell::Int(2), Cell::Text("y".to_string())],
            ],
        );
        let out = format_result(&mut r, Format::Json, 100).unwrap();
        assert_eq!(out, r#"[{"a":1,"b":"x"},{"a":2,"b":"y"}]"#);
    }

    // 7. Table alignment across columns of different widths
    #[test]
    fn table_columns_align() {
        let mut r = make_result(
            &["name", "count"],
            vec![vec![Cell::Text("alice".to_string()), Cell::Int(1)]],
        );
        let out = format_result(&mut r, Format::Table, 100).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header + separator + one data row");
        let header = lines[0];
        let data = lines[2];
        let count_pos = header.find("count").expect("header has second column");
        let one_pos = data.find('1').expect("data has second column value");
        assert_eq!(count_pos, one_pos, "second column must align between header and data");
        assert!(lines[1].chars().all(|c| c == '-' || c == ' '), "separator line");
    }

    // 8. Markdown pipe format; CSV RFC 4180 escaping
    #[test]
    fn markdown_pipe_format() {
        let mut r = make_result(
            &["a", "b"],
            vec![vec![Cell::Int(1), Cell::Text("x".to_string())]],
        );
        let out = format_result(&mut r, Format::Markdown, 100).unwrap();
        assert_eq!(out, "| a | b |\n|---|---|\n| 1 | x |");
    }

    #[test]
    fn csv_escapes_commas_and_quotes() {
        let mut r = make_result(
            &["c1", "c2"],
            vec![vec![
                Cell::Text("a,b".to_string()),
                Cell::Text("q\"x".to_string()),
            ]],
        );
        let out = format_result(&mut r, Format::Csv, 100).unwrap();
        assert_eq!(out, "c1,c2\n\"a,b\",\"q\"\"x\"");
    }

    // 9. max_rows truncation: body keeps only the first row, truncated records it
    #[test]
    fn max_rows_truncates_and_records() {
        let mut r = make_result(
            &["a"],
            vec![vec![Cell::Int(1)], vec![Cell::Int(2)], vec![Cell::Int(3)]],
        );
        let out = format_result(&mut r, Format::Json, 1).unwrap();
        assert_eq!(out, r#"[{"a":1}]"#, "body contains only the first row");
        assert_eq!(r.truncated, Some(RowLimitExceeded { returned: 1, total: 3 }));
    }

    #[test]
    fn no_truncation_when_within_limit() {
        let mut r = make_result(&["a"], vec![vec![Cell::Int(1)], vec![Cell::Int(2)]]);
        let out = format_result(&mut r, Format::Json, 5).unwrap();
        assert_eq!(out, r#"[{"a":1},{"a":2}]"#);
        assert_eq!(r.truncated, None);
    }

    // Bonus: empty table marker and CJK wide-char column widths
    #[test]
    fn table_empty_shows_zero_rows() {
        let mut r = make_result(&["a"], vec![]);
        let out = format_result(&mut r, Format::Table, 100).unwrap();
        assert_eq!(out, "(0 rows)");
    }

    #[test]
    fn table_cjk_width_counts_as_two() {
        let mut r = make_result(
            &["名称", "值"],
            vec![vec![Cell::Text("x".to_string()), Cell::Int(7)]],
        );
        let out = format_result(&mut r, Format::Table, 100).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // 名称 = 4 display columns, 值 = 2.
        assert_eq!(lines[1], "---- --");
        let header = lines[0];
        let data = lines[2];
        let h = display_width(&header[..header.find('值').unwrap()]);
        let d = display_width(&data[..data.find('7').unwrap()]);
        assert_eq!(h, d, "second column aligns in display width");
    }
}