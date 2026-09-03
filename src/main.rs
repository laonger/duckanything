mod cli;
mod core;
mod mcp;

use clap::Parser as _;

fn main() {
    let cli = cli::Cli::parse();
    match cli.command {
        cli::Command::Serve => {
            eprintln!("serve: not implemented yet");
            std::process::exit(2);
        }
        cli::Command::Query { file: _, sql: _ } => {
            eprintln!("query: not implemented yet");
            std::process::exit(2);
        }
        cli::Command::Describe { file: _ } => {
            eprintln!("describe: not implemented yet");
            std::process::exit(2);
        }
    }
}
