use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Json,
    Table,
    Markdown,
    Csv,
}

#[derive(Parser)]
#[command(name = "duckanything", version, about = "Query local data files with DuckDB")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Output format for query results (CLI mode)
    #[arg(long, value_enum, default_value_t = Format::Table, global = true)]
    pub format: Format,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run as MCP server (stdio transport)
    Serve,
    /// Run a single SQL query against a data file
    Query {
        /// Path to the data file (csv/json/parquet)
        file: String,
        /// SQL to execute (table name = file stem)
        sql: String,
    },
    /// Show the schema of a data file
    Describe {
        /// Path to the data file (csv/json/parquet)
        file: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn help_lists_three_subcommands() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("serve"), "help should mention serve");
        assert!(help.contains("query"), "help should mention query");
        assert!(help.contains("describe"), "help should mention describe");
    }
}
