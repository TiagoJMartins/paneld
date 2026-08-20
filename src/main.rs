//! paneld — a minimal TRMNL BYOS panel server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use paneld::app::Runtime;
use paneld::{config, http, renderer};
use time::OffsetDateTime;

/// How often the configuration's modification time is checked.
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
    /// Parse and validate the configuration, print what it describes, and exit.
    ///
    /// Exits non-zero on any parse or validation error, so it works as a
    /// pre-commit check and as a container startup probe: it proves the config
    /// that is actually mounted is one this binary accepts.
    Validate,

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
        Some(Command::Validate) => validate(&cli.config),
        Some(Command::Preview { device, output }) => preview(&cli.config, &device, &output).await,
        None => serve(&cli.config).await,
    }
}

/// Reports what the configuration describes, or why it is not usable.
fn validate(config_path: &Path) -> Result<()> {
    let config = config::load(config_path)?;
    let sources = config::sources(config_path);

    println!("{} ok", config_path.display());
    // Which fragments are in effect is the first question a `<file>.d` directory
    // raises, and it is a question the main file's own text cannot answer.
    println!("  files            {}", sources.len());
    for source in &sources {
        println!("    - {}", source.display());
    }
    println!("  listen           {}", config.server.listen);
    println!("  public_base_url  {}", config.server.public_base_url);
    println!("  content_path     {}", config.server.content_path);
    println!(
        "  home_assistant   {}",
        match &config.home_assistant {
            Some(ha) => &ha.base_url,
            None => "not configured",
        }
    );

    if config.devices.is_empty() {
        println!("  devices          none");
    }
    for device in &config.devices {
        let cells = device.grid.cols * device.grid.rows;
        let occupied: u32 = device.widgets.iter().map(|w| w.col_span * w.row_span).sum();
        println!(
            "  device {} {}x{} {:?}/{:?} refresh {}s render {}s",
            device.id,
            device.width,
            device.height,
            device.palette,
            device.dither,
            device.refresh_rate,
            device.render_interval,
        );
        println!(
            "    grid {}x{}, {} of {} cells used by {} widget(s), gap {} padding {} border {}",
            device.grid.cols,
            device.grid.rows,
            occupied,
            cells,
            device.widgets.len(),
            device.chrome.gap,
            device.chrome.padding,
            device.chrome.border,
        );
        if let Some(bar) = &device.status_bar {
            let fields: Vec<String> = bar
                .fields
                .iter()
                .map(|field| format!("{field:?}").to_lowercase())
                .collect();
            println!(
                "    status bar on the {} edge, {}px, {} (IANA {}): {}",
                bar.edge,
                bar.thickness,
                bar.timezone.name(),
                config::TZDATA_VERSION,
                fields.join(" ")
            );
            // The clock is the one field that costs a repaint per render, so an
            // author reading this report should be told which they configured.
            if bar.fields.contains(&config::StatusField::Time) {
                println!(
                    "      the clock changes every frame, so this panel repaints \
                     every {}s",
                    device.render_interval
                );
            }
        }
        for widget in &device.widgets {
            print_widget(widget, 4);
            // A group's children are widgets in their own right — each with its own
            // push address — so a report that stopped at the group would leave an
            // author unable to see the ids they are meant to publish to.
            for child in widget.group.iter().flat_map(|group| &group.widgets) {
                print_widget(child, 8);
            }
        }
    }
    Ok(())
}

/// One widget's line in the `validate` report, indented to its depth.
///
/// Names the id first because that is what an author looks one up by: it is the
/// content push address as well as the config key.
fn print_widget(widget: &paneld::config::Widget, indent: usize) {
    let pad = " ".repeat(indent);
    print!(
        "{pad}- {:<16} {:?} at col {} row {} span {}x{}",
        widget.id, widget.kind, widget.col, widget.row, widget.col_span, widget.row_span,
    );
    if let Some(group) = &widget.group {
        print!(
            ", a {}x{} sub-grid of {}",
            group.grid.cols,
            group.grid.rows,
            group.widgets.len()
        );
    }
    if !widget.readings.is_empty() {
        print!(", {} reading(s)", widget.readings.len());
    }
    println!();
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

/// Re-reads the configuration when anything it is built from changes on disk.
///
/// That is the main file, its `<file name>.d` drop-in directory *and* every
/// fragment in it, which is what [`config::modified_at`] folds into one instant.
/// The directory's own timestamp is in there deliberately: adding or deleting a
/// fragment modifies no file, so a check that stats only files never notices a
/// dropped-in override arriving, and keeps rendering a deleted one long after it
/// stopped existing.
///
/// A parse or validation error leaves the previously loaded configuration in
/// effect and logs the error, so a typo never blanks the panel.
///
/// Modification time is polled rather than watched: it needs no platform-specific
/// filesystem notification API, and it cannot miss the editors that write a config
/// by renaming a temporary file over it — a case where an inode watch sees nothing.
async fn watch_config(runtime: Arc<Runtime>, path: PathBuf) {
    let mut last_seen = config::modified_at(&path);

    loop {
        tokio::time::sleep(CONFIG_POLL_INTERVAL).await;

        let current = config::modified_at(&path);
        if current == last_seen {
            continue;
        }
        last_seen = current;

        match runtime.reload(&path) {
            Ok(()) => tracing::info!(
                path = %path.display(),
                devices = runtime.config().devices.len(),
                "configuration reloaded"
            ),
            Err(error) => tracing::error!(
                path = %path.display(),
                error = format!("{error:#}"),
                "configuration is invalid; keeping the previous one in effect"
            ),
        }
    }
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
