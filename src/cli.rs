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
    /// Run as MCP server (stdio transport; add --http to also serve over HTTP)
    Serve {
        /// Also serve over HTTP: Streamable HTTP at /mcp and SSE at /sse
        #[arg(long)]
        http: bool,
        /// Host to bind the HTTP server to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind the HTTP server to
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
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

    #[test]
    fn serve_http_flags_parse() {
        let cli =
            Cli::try_parse_from(["duckanything", "serve", "--http", "--port", "9999"]).unwrap();
        match cli.command {
            Command::Serve { http, host, port } => {
                assert!(http, "--http should set http=true");
                assert_eq!(host, "127.0.0.1", "host should default to 127.0.0.1");
                assert_eq!(port, 9999, "--port 9999 should be parsed");
            }
            _ => panic!("expected serve command"),
        }

        let cli = Cli::try_parse_from(["duckanything", "serve"]).unwrap();
        match cli.command {
            Command::Serve { http, host, port } => {
                assert!(!http, "without --http, http should be false");
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 8080);
            }
            _ => panic!("expected serve command"),
        }
    }
}
