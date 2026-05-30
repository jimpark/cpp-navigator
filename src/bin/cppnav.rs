//! Short human-facing alias for `cpp-navigator`. Same engine, same args.
use std::process::exit;

fn main() {
    exit(cpp_navigator::cli::run());
}
