use continuo::{error::TelemetryError, telemetry};
use std::{error::Error, process::ExitCode};

fn run() -> Result<(), TelemetryError> {
    telemetry::init()?;
    tracing::info!("Continuo foundation initialized");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
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
            ExitCode::FAILURE
        }
    }
}
