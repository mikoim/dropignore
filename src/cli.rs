use clap::Parser;
use std::path::PathBuf;

/// Command line arguments handled by clap.
#[derive(Debug, Parser)]
#[command(
    name = "dropignore",
    version,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn version_flag_reads_cargo_metadata() {
        let cmd = CliArgs::command();
        assert_eq!(
            cmd.get_version(),
            Some(env!("CARGO_PKG_VERSION")),
            "--version must report the Cargo.toml version"
        );
    }
}
