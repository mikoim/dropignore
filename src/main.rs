//! CLI tool that watches for newly created paths under a user supplied root and marks
//! matching entries with Dropbox's `user.com.dropbox.ignored` extended attribute.
//! The implementation focuses on:
//! - Efficient recursive monitoring using inotify with a dynamic watch set.
//! - A pluggable rule system so new matching conditions can be added easily.
//! - A dry-run mode for safe inspection without mutating the filesystem.

mod app;
mod cli;
mod discovery;
mod dropbox;
mod rules;
#[cfg(test)]
mod test_util;
mod watch;

use anyhow::Result;
use clap::Parser;
use env_logger::Env;

fn main() -> Result<()> {
    let args = cli::CliArgs::parse();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    app::run(args)
}
