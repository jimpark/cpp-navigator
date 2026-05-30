//! Per-command pipelines (design-specs §7).
//!
//! Phase 0 wires the full I/O path (parse args → emit records → exit code) with
//! the pipeline stubbed to `not_found`. Subsequent phases replace the stub with
//! the candidate-finder → engine → resolver chain.

use anyhow::Result;

use crate::cli::{Cli, Command};
use crate::output::{Record, Writer};

/// Run the selected command, writing records to stdout.
pub fn dispatch(cli: &Cli) -> Result<()> {
    let (command_name, targets) = match &cli.command {
        Command::FindDef { name, .. } => ("find-def", name),
        Command::FindDecl { name } => ("find-decl", name),
        Command::FindRefs { name, .. } => ("find-refs", name),
    };

    let stdout = std::io::stdout();
    let mut writer = Writer::new(stdout.lock(), cli.format, cli.legend);

    for target in targets {
        // TODO(phase 1+): run candidate finder → engine → resolver here.
        let record = Record::not_found(command_name, target);
        writer.write(&record)?;
    }

    writer.finish()?;
    Ok(())
}
