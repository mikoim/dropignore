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
    /// Scan the tree once, mark matches, and exit without watching.
    #[arg(long = "scan-once", default_value_t = false)]
    pub(crate) scan_once: bool,
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

    #[test]
    fn scan_once_flag_parses_and_defaults_off() {
        let on = CliArgs::parse_from(["dropignore", "--scan-once", "/tmp"]);
        assert!(on.scan_once, "--scan-once must set the flag");
        assert_eq!(on.root, PathBuf::from("/tmp"));

        let off = CliArgs::parse_from(["dropignore", "/tmp"]);
        assert!(!off.scan_once, "flag must default to false");
    }
}
