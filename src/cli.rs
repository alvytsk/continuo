//! The `continuo` command-line surface.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "continuo", about = "A keyboard-first terminal audio player")]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Play a local audio file or an HTTP(S) URL.
    Play {
        /// Path to an MP3, FLAC, WAV or M4A file, or an http(s):// URL.
        source: String,
        /// Open the source, print what was found, and exit without using a
        /// device or a terminal.
        #[arg(long)]
        probe_only: bool,
    },
}
