use crate::error::TelemetryError;
use tracing_subscriber::EnvFilter;

pub fn subscriber(filter: &str) -> Result<impl tracing::Subscriber + Send + Sync, TelemetryError> {
    Ok(tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter)?)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .finish())
}

pub fn init() -> Result<(), TelemetryError> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "continuo=info".into(),
        Err(error) => return Err(TelemetryError::Environment(error)),
    };
    tracing::subscriber::set_global_default(subscriber(&filter)?)?;
    Ok(())
}
