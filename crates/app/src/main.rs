//! The castaway binary. Wires the enabled protocol adapters into one session manager
//! driving one pipeline. This is the only crate that uses `anyhow` (ground rule 7).

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("castaway starting (scaffolding — adapters wired in as crates land)");
    // Full wiring lands with the `app` task once the adapter crates exist.
    Ok(())
}
