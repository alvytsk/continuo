//! The `continuo` command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "continuo", about = "A keyboard-first terminal audio player")]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Play a local audio file.
    Play {
        /// Path to an MP3, FLAC, WAV, or M4A file.
        path: PathBuf,
        /// Open the file, print what was found, and exit without using a device.
        #[arg(long)]
        probe_only: bool,
    },
}
