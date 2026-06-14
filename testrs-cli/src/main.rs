mod codegen;
mod discover;
mod graph;
mod run;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "testrs",
    about = "Code generator for the testrs test framework"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover testrs fixtures and tests in a crate and print their resolved signatures.
    Discover(Target),
    /// Build and validate the fixture dependency graph for a crate.
    Graph(Target),
    /// Generate the kitest harness source for a crate (prints to stdout).
    Generate(Target),
    /// Generate and run the crate's testrs suite (no worktree changes).
    Test {
        #[command(flatten)]
        target: Target,
        /// Run via `cargo nextest` instead of `cargo test`.
        #[arg(long)]
        nextest: bool,
    },
}

#[derive(clap::Args)]
struct Target {
    /// Package to analyze.
    package: String,
    /// Path to the target crate's `Cargo.toml`.
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: PathBuf,
    /// Toolchain used to generate rustdoc JSON for the target crate.
    #[arg(long, default_value = discover::DEFAULT_TOOLCHAIN)]
    toolchain: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Discover(t) => {
            let discovery = discover::discover(&t.manifest_path, &t.package, &t.toolchain)?;
            discover::print_discovery(&discovery);
            Ok(())
        }
        Command::Graph(t) => {
            let discovery = discover::discover(&t.manifest_path, &t.package, &t.toolchain)?;
            let g = graph::build(&discovery);
            graph::print_graph(&discovery, &g);
            if g.errors.is_empty() {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Command::Generate(t) => {
            let discovery = discover::discover(&t.manifest_path, &t.package, &t.toolchain)?;
            let g = graph::build(&discovery);
            let source = codegen::generate(&discovery, &g)?;
            print!("{source}");
            Ok(())
        }
        Command::Test { target, nextest } => {
            let discovery =
                discover::discover(&target.manifest_path, &target.package, &target.toolchain)?;
            let g = graph::build(&discovery);
            if !g.errors.is_empty() {
                graph::print_graph(&discovery, &g);
                std::process::exit(1);
            }
            let code = run::run(&discovery, &g, nextest)?;
            std::process::exit(code);
        }
    }
}
