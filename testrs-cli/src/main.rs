mod discover;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "testrs", about = "Code generator for the testrs test framework")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover testrs fixtures and tests in a crate and print their resolved signatures.
    Discover {
        /// Package to analyze.
        package: String,
        /// Path to the target crate's `Cargo.toml`.
        #[arg(long, default_value = "Cargo.toml")]
        manifest_path: PathBuf,
        /// Toolchain used to generate rustdoc JSON for the target crate.
        #[arg(long, default_value = discover::DEFAULT_TOOLCHAIN)]
        toolchain: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Discover {
            package,
            manifest_path,
            toolchain,
        } => {
            let discovery = discover::discover(&manifest_path, &package, &toolchain)?;
            discover::print_discovery(&discovery);
            Ok(())
        }
    }
}
