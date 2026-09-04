mod cli;
mod core;
mod mcp;

use std::path::Path;
use std::sync::Arc;

use clap::Parser as _;

use core::error::DuckAnythingError;
use core::format::Format as OutFormat;
use core::security::Config;
use core::{engine::Engine, format};

fn main() {
    core::logging::init();
    let cli = cli::Cli::parse();
    let exit_code = match cli.command {
        cli::Command::Serve => cmd_serve(),
        cli::Command::Query { ref file, ref sql } => cmd_query(file, sql, cli.format),
        cli::Command::Describe { ref file } => cmd_describe(file),
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn cmd_serve() -> i32 {
    match Config::from_env(None).and_then(|config| {
        tokio::runtime::Runtime::new()
            .map_err(|e| DuckAnythingError::Io(std::io::Error::other(e)))?
            .block_on(mcp::server::run_server(&config))
    }) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cli_format_to_out(f: cli::Format) -> OutFormat {
    match f {
        cli::Format::Json => OutFormat::Json,
        cli::Format::Table => OutFormat::Table,
        cli::Format::Markdown => OutFormat::Markdown,
        cli::Format::Csv => OutFormat::Csv,
    }
}

fn cmd_query(file: &str, sql: &str, out_format: cli::Format) -> i32 {
    let config = match Config::from_env(None) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let engine = match Engine::new(&config) {
        Ok(e) => Arc::new(e),
        Err(e) => return fail(&e),
    };
    if let Err(e) = engine.mount_file(Path::new(file), None) {
        return fail(&e);
    }
    let mut result = match engine.run_query(sql) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    match format::format_result(&mut result, cli_format_to_out(out_format), config.max_rows) {
        Ok(text) => {
            println!("{text}");
            if let Some(rl) = &result.truncated {
                eprintln!("[truncated] showing {} of {} rows", rl.returned, rl.total);
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn cmd_describe(file: &str) -> i32 {
    let config = match Config::from_env(None) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let engine = match Engine::new(&config) {
        Ok(e) => e,
        Err(e) => return fail(&e),
    };
    let mount = match engine.mount_file(Path::new(file), None) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    match engine.describe_view(&mount.view_name) {
        Ok(desc) => {
            println!("view: {}", desc.view_name);
            for col in &desc.columns {
                let samples: Vec<String> =
                    col.samples.iter().map(format::cell_to_string).collect();
                println!("{}\t{}\t{}", col.name, col.type_name, samples.join(","));
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn fail(e: &DuckAnythingError) -> i32 {
    eprintln!("error: {e}");
    1
}
