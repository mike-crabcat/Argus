use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use argus_server::config::Config;
use argus_server::recorder::{self, StreamEvent};
use argus_store::{ChunkWriter, Index};
use clap::Parser;
use tokio::sync::broadcast;
use tracing::info;

#[derive(Parser)]
#[command(version, about = "Argus NVR server")]
struct Args {
    /// Path to argus.toml.
    #[arg(short, long, default_value = "argus.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,retina=warn".into()),
        )
        .init();
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    std::fs::create_dir_all(&cfg.storage.root)
        .with_context(|| format!("creating {}", cfg.storage.root.display()))?;
    let index = Arc::new(Mutex::new(Index::open(
        &cfg.storage.root.join("index.sqlite"),
    )?));

    let mut ingest = Vec::new();
    let mut recorders = Vec::new();
    for cam in &cfg.cameras {
        for (stream, source) in &cam.streams {
            let (tx, rx) = broadcast::channel::<StreamEvent>(1024);
            let writer = ChunkWriter::new(
                &cfg.storage.root,
                &cam.name,
                stream,
                cfg.storage.chunk_seconds,
                index.clone(),
            );
            recorders.push(tokio::task::spawn_blocking(move || {
                recorder::record(rx, writer)
            }));
            ingest.push(tokio::spawn(recorder::supervise(
                cam.name.clone(),
                stream.clone(),
                source.clone(),
                tx,
            )));
        }
    }
    info!(streams = ingest.len(), root = %cfg.storage.root.display(), "recording");

    tokio::signal::ctrl_c().await?;
    info!("shutting down, closing open chunks");
    for t in &ingest {
        t.abort(); // drops each broadcast sender, which ends its recorder
    }
    for r in recorders {
        r.await??;
    }
    Ok(())
}
