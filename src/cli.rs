use clap::Parser;
use std::path::PathBuf;

/// Command line arguments handled by clap.
#[derive(Debug, Parser)]
#[command(
    name = "dropignore",
    about = "Watch a directory and tag matching paths with the Dropbox ignore attribute."
)]
pub(crate) struct CliArgs {
    /// Root directory to watch recursively.
    #[arg(value_name = "DIRECTORY")]
    pub(crate) root: PathBuf,
    /// Skip calling setxattr and only log intended actions.
    #[arg(short = 'n', long = "dry-run", default_value_t = false)]
    pub(crate) dry_run: bool,
}
