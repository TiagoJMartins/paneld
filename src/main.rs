//! paneld — a minimal TRMNL BYOS panel server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use paneld::app::Runtime;
use paneld::{config, http, renderer};
use time::OffsetDateTime;

/// How often the configuration file's modification time is checked.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Parser)]
#[command(
    name = "paneld",
    version,
    about = "Serves TRMNL BYOS panels from a TOML dashboard"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "paneld.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Render one device once, write the PNG to a path, and exit.
    ///
    /// Starts neither the listener nor the render loop, so it is the fast
    /// development loop for iterating on a layout.
    Preview {
        /// Device id, as configured.
        device: String,
        /// Where to write the PNG.
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "paneld=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Preview { device, output }) => preview(&cli.config, &device, &output).await,
        None => serve(&cli.config).await,
    }
}

async fn serve(config_path: &Path) -> Result<()> {
    // A parse error at startup, when there is no previous configuration to fall
    // back on, is fatal.
    let config = config::load(config_path)?;
    let listen = config.server.listen;
    let device_count = config.devices.len();

    let (runtime, wake) = Runtime::new(config)?;

    // Every configured device is rendered before the listener accepts, so a device
    // polling immediately gets a real frame rather than a placeholder.
    tracing::info!(devices = device_count, "rendering initial frames");
    runtime.render_all(OffsetDateTime::now_utc()).await;

    let mut schedule = renderer::Schedule::new();
    schedule.mark_all_rendered(&runtime, Instant::now());
    tokio::spawn(renderer::run(Arc::clone(&runtime), wake, schedule));
    tokio::spawn(watch_config(
        Arc::clone(&runtime),
        config_path.to_path_buf(),
    ));

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, "listening");

    axum::serve(listener, http::router(runtime))
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving HTTP")
}

async fn preview(config_path: &Path, device_id: &str, output: &Path) -> Result<()> {
    let config = config::load(config_path)?;
    let (runtime, _wake) = Runtime::new(config)?;

    runtime
        .render_device(device_id, OffsetDateTime::now_utc())
        .await?;
    let frame = runtime
        .frames
        .current(device_id)
        .context("the render produced no frame")?;

    std::fs::write(output, &frame.bytes)
        .with_context(|| format!("writing {}", output.display()))?;
    println!(
        "{} ({} bytes, {})",
        output.display(),
        frame.bytes.len(),
        frame.hash
    );
    Ok(())
}

/// Re-reads the configuration when the file changes on disk.
///
/// A parse or validation error leaves the previously loaded configuration in
/// effect and logs the error, so a typo never blanks the panel.
///
/// Modification time is polled rather than watched: it needs no platform-specific
/// filesystem notification API, and it cannot miss the editors that write a config
/// by renaming a temporary file over it — a case where an inode watch sees nothing.
async fn watch_config(runtime: Arc<Runtime>, path: PathBuf) {
    let mut last_seen = modified_at(&path);

    loop {
        tokio::time::sleep(CONFIG_POLL_INTERVAL).await;

        let current = modified_at(&path);
        if current == last_seen {
            continue;
        }
        last_seen = current;

        match config::load(&path) {
            Ok(config) => {
                tracing::info!(
                    path = %path.display(),
                    devices = config.devices.len(),
                    "configuration reloaded"
                );
                runtime.replace_config(config);
            }
            Err(error) => tracing::error!(
                path = %path.display(),
                error = format!("{error:#}"),
                "configuration is invalid; keeping the previous one in effect"
            ),
        }
    }
}

fn modified_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

async fn shutdown() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("installing the interrupt handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing the terminate handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}
