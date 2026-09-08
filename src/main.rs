use std::{error::Error, process::ExitCode};

use clap::Parser;
use continuo::{app, cli, telemetry};

fn main() -> ExitCode {
    if let Err(error) = telemetry::init() {
        eprintln!("continuo: {error}");
        let fallback = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(fallback, || {
            let mut chain = vec![error.to_string()];
            let mut source = error.source();
            while let Some(cause) = source {
                let text = cause.to_string();
                if chain.last() != Some(&text) {
                    chain.push(text);
                }
                source = cause.source();
            }
            tracing::error!(error = %error, causes = ?chain, "application startup failed");
        });
        return ExitCode::FAILURE;
    }

    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            eprint!("{error}");
            return ExitCode::FAILURE;
        }
    };

    match app::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("continuo: {error}");
            tracing::error!(error = ?error, "playback failed");
            ExitCode::FAILURE
        }
    }
}
