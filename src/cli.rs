//! Command-line interface (design-specs §12).
//!
//! Parses args, dispatches to a command pipeline, and maps the outcome to the
//! process exit code (design-specs §9):
//!   0 = a well-formed answer was produced (including `not_found`),
//!   2 = usage error (handled by clap),
//!   3 = internal tool error.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::commands;

/// Output presentation profile (design-specs §8.8). Record *data* is identical
/// across profiles; this only selects the wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// One JSON object per line (default).
    Jsonl,
    /// A single fenced block for a human to paste into a chat.
    Bundle,
    /// Human-readable text for terminal use; uses ANSI color when stdout is a TTY.
    Human,
}

#[derive(Parser, Debug)]
#[command(
    name = "cpp-navigator",
    version,
    about = "LLM-optimized C++ codebase navigator (JSONL output)",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Search root (default: cwd). Repeatable.
    #[arg(long, global = true, value_name = "PATH")]
    pub root: Vec<PathBuf>,

    /// Enable Stage 2 semantic resolution (requires compile_commands.json and
    /// a build with `--features semantic`).
    #[arg(long, global = true)]
    pub semantic: bool,

    /// Explicit path to compile_commands.json.
    #[arg(long, global = true, value_name = "PATH")]
    pub compile_db: Option<PathBuf>,

    /// Restrict to these comma-separated extensions.
    #[arg(long, global = true, value_name = "EXT", value_delimiter = ',')]
    pub lang: Vec<String>,

    /// Cap candidate files before parsing.
    #[arg(long, global = true, value_name = "N", default_value_t = 200)]
    pub max_candidates: usize,

    /// When multiple definitions/declarations match (overloads), show full
    /// content for up to this many results instead of just ambiguous locations.
    #[arg(long, global = true, value_name = "N", default_value_t = 3)]
    pub max_results: usize,

    /// Fallback context window in lines.
    #[arg(long, global = true, value_name = "N", default_value_t = 10)]
    pub window: usize,

    /// Parser threads (default: number of cores).
    #[arg(long, global = true, value_name = "N")]
    pub jobs: Option<usize>,

    /// Do not honor .gitignore/.ignore.
    #[arg(long, global = true)]
    pub no_ignore: bool,

    /// Output profile.
    #[arg(long, global = true, value_enum, default_value_t = Format::Jsonl)]
    pub format: Format,

    /// In bundle mode, prepend a one-time field legend.
    #[arg(long, global = true)]
    pub legend: bool,

    /// Run multiple queries from a file, one per line.
    #[arg(long, global = true, value_name = "PATH")]
    pub manifest: Option<PathBuf>,

    /// Cap estimated output tokens; selection-only trim.
    #[arg(long, global = true, value_name = "N")]
    pub budget: Option<usize>,

    /// Treat NAME as an empty annotation macro when parsing (like clang's
    /// `-DNAME=`). Repeatable. Helps tree-sitter through dllimport/export and
    /// calling-convention macros between a return type and a function name.
    /// UPPER_CASE annotation macros are also detected automatically.
    #[arg(long = "empty-macro", global = true, value_name = "NAME")]
    pub empty_macro: Vec<String>,

    /// Suppress stderr diagnostics.
    #[arg(long, global = true)]
    pub quiet: bool,
}

/// The three query commands (design-specs §7).
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Find the definition(s) of a symbol.
    FindDef {
        /// Target identifier(s). Bare (`f`) or qualified (`A::B::f`).
        #[arg(required = true)]
        name: Vec<String>,
        /// If the match is a class member, return the enclosing class/struct.
        #[arg(long)]
        scope: bool,
    },
    /// Find the declaration/signature of a symbol (header-biased).
    FindDecl {
        #[arg(required = true)]
        name: Vec<String>,
    },
    /// Find references/usages of a symbol.
    FindRefs {
        #[arg(required = true)]
        name: Vec<String>,
        /// Also emit the enclosing function/template body of each hit.
        #[arg(long)]
        context: bool,
    },
}

/// Parse args and run, returning the process exit code.
pub fn run() -> i32 {
    // clap handles --help/--version (exit 0) and usage errors (exit 2) itself.
    let cli = Cli::parse();
    match commands::dispatch(&cli) {
        Ok(()) => 0,
        Err(e) => {
            if !cli.quiet {
                eprintln!("cpp-navigator: error: {e:#}");
            }
            3
        }
    }
}
