//! The composition root: everything the process shares, wired together.
//!
//! Both `main` and the tests build a [`Runtime`], which is what makes the HTTP
//! boundary testable in-process without binding a port.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use takumi::prelude::Fonts;
use time::OffsetDateTime;
use tokio::sync::mpsc;

use crate::config::{Config, Device};
use crate::content::ContentStore;
use crate::frame::{Frame, FrameStore};
use crate::ha::{HaClient, HttpHaClient, LastGood, Reading, Reported, fetch_readings};
use crate::icon;
use crate::render::{self, RenderInputs};
use crate::status::StatusStore;
use crate::tap::{self, Taps};

/// How many device ids the render loop's wake channel can hold before a sender
/// gives up.
///
/// Dropping a wake message is harmless: the device is rendered again at its next
/// interval either way, and a full channel already means a render is imminent.
/// What must never happen is a `PUT` blocking on the renderer.
pub const WAKE_CHANNEL_CAPACITY: usize = 256;

/// Distinct unconfigured device ids whose placeholder frames are kept.
///
/// Bounded because the key is attacker-supplied: a device id nobody configured is
/// exactly the case this cache exists for.
const MAX_PLACEHOLDERS: usize = 16;

/// Frame dimensions used for a placeholder when the device reported none.
const DEFAULT_PLACEHOLDER_SIZE: (u32, u32) = (800, 480);

/// `refresh_rate` sent alongside a placeholder, in seconds.
const PLACEHOLDER_REFRESH_RATE: u32 = 300;

/// Everything shared between the HTTP handlers and the render loop.
pub struct Runtime {
    /// Swapped wholesale on a successful reload, so a malformed file leaves the
    /// previous configuration in effect.
    config: RwLock<Arc<Config>>,
    pub content: ContentStore,
    pub frames: FrameStore,
    pub status: StatusStore,
    fonts: Fonts,
    ha: Option<Box<dyn HaClient>>,
    /// The last value successfully read for each Home Assistant reading, per
    /// device, so a failed fetch mutes a cell instead of emptying it.
    ///
    /// Keyed by device because pruning is per device: each render folds one
    /// device's results, and a shared map would evict the readings of every
    /// device that was not being rendered.
    last_good: Mutex<HashMap<String, LastGood>>,
    /// Fetches and caches widget icons. `None` when the cache directory could not
    /// be created, in which case cells render without icons rather than not at
    /// all.
    icons: Option<icon::Store>,
    /// Recent tap event ids, so a client that retries does not act twice.
    taps: Taps,
    wake_tx: mpsc::Sender<String>,
    /// Placeholders for unconfigured device ids, rendered on demand.
    placeholders: Mutex<HashMap<PlaceholderKey, Frame>>,
}

type PlaceholderKey = (String, u32, u32);

impl Runtime {
    /// Builds the runtime, returning it alongside the render loop's receiving end
    /// of the wake channel.
    pub fn new(config: Config) -> Result<(Arc<Self>, mpsc::Receiver<String>)> {
        let ha = match &config.home_assistant {
            Some(settings) => Some(Box::new(HttpHaClient::new(settings)?) as Box<dyn HaClient>),
            None => None,
        };
        Self::with_home_assistant(config, ha)
    }

    /// Builds the runtime with a caller-supplied Home Assistant client, which is
    /// how tests substitute a stub without a network.
    pub fn with_home_assistant(
        config: Config,
        ha: Option<Box<dyn HaClient>>,
    ) -> Result<(Arc<Self>, mpsc::Receiver<String>)> {
        let content = ContentStore::load(config.server.content_path.clone());
        let (wake_tx, wake_rx) = mpsc::channel(WAKE_CHANNEL_CAPACITY);

        // A cache directory that cannot be created is logged once here rather than
        // on every frame: icons then simply do not resolve, which costs decoration
        // and nothing else.
        let icons = match icon::Store::new(&config.server.icon_cache_path) {
            Ok(store) => Some(store),
            Err(error) => {
                tracing::error!(
                    error = format!("{error:#}"),
                    "widget icons are disabled for this run"
                );
                None
            }
        };

        let runtime = Arc::new(Self {
            config: RwLock::new(Arc::new(config)),
            content,
            frames: FrameStore::new(),
            status: StatusStore::new(),
            fonts: render::fonts().context("loading the embedded fonts")?,
            ha,
            last_good: Mutex::new(HashMap::new()),
            icons,
            taps: Taps::new(),
            wake_tx,
            placeholders: Mutex::new(HashMap::new()),
        });
        Ok((runtime, wake_rx))
    }

    /// The configuration currently in effect. A cheap `Arc` clone, so a handler
    /// never holds the lock while it works.
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Re-reads the configuration file and swaps it in if it is valid.
    ///
    /// On failure the previously loaded configuration stays in effect and the error
    /// is handed back for the caller to log, so a typo never blanks the panel.
    pub fn reload(&self, path: &std::path::Path) -> Result<()> {
        let config = crate::config::load(path)?;
        self.replace_config(config);
        Ok(())
    }

    /// Swaps in a freshly validated configuration.
    pub fn replace_config(&self, config: Config) {
        *self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(config);
    }

    /// A sender for the render loop's wake channel.
    pub fn wake(&self) -> mpsc::Sender<String> {
        self.wake_tx.clone()
    }

    /// Asks the render loop to rebuild `device_id` as soon as it can.
    ///
    /// Never blocks and never fails the caller: a `PUT` must return as soon as the
    /// content is stored and the wake message is queued.
    pub fn request_render(&self, device_id: &str) {
        if self.wake_tx.try_send(device_id.to_owned()).is_err() {
            tracing::debug!(
                device = device_id,
                "render wake channel is full; the device will be rebuilt on its interval"
            );
        }
    }

    /// Renders one device and offers the result to the frame store.
    ///
    /// Returns whether the encoded bytes differed from the frame already being
    /// served. Either way this counts as a render for `render_count`: it did
    /// perform one, and that observable is how a wedged loop becomes visible.
    pub async fn render_device(&self, device_id: &str, now: OffsetDateTime) -> Result<bool> {
        let config = self.config();
        let device = config
            .devices
            .iter()
            .find(|device| device.id == device_id)
            .with_context(|| format!("device `{device_id}` is not configured"))?;

        // Both fetched before the pure render so that rendering itself stays
        // synchronous and reproducible.
        let ha_states = self.ha_states(device).await;
        let icons = self.icons(device).await;
        let content = self.content.snapshot();
        // Read once, here, rather than inside the renderer: a frame is a pure
        // function of its inputs, and a status bar reaching into the status store
        // mid-rasterise would be the one thing in the render path that is not.
        let telemetry = self.status.telemetry(device_id);

        // The layout engine asserts internally on degenerate geometry, so a panic
        // here is possible in a way an error is not. Containing it matters more
        // than the bug it signals: an escaping panic would kill the render task,
        // and the listener would go on serving the last frame it produced forever
        // — stale but plausible content that nothing makes visible.
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render::render_frame(
                &self.fonts,
                RenderInputs {
                    device,
                    content: &content,
                    ha_states: &ha_states,
                    icons: &icons,
                    telemetry: &telemetry,
                    now,
                },
            )
        }));
        let bytes = match rendered {
            Ok(result) => result?,
            Err(_) => anyhow::bail!(
                "the layout engine panicked rendering device `{device_id}`; \
                 the frame was skipped so the render loop keeps running"
            ),
        };

        let changed = self.frames.offer(device_id, bytes, now);
        let hash = self
            .frames
            .current(device_id)
            .map(|frame| frame.hash)
            .unwrap_or_default();
        self.status.record_render(device_id, &hash, now);

        Ok(changed)
    }

    /// Resolves every Home Assistant reading this device's dashboard references,
    /// folded against what was last known good.
    ///
    /// A missing client or a per-reading failure leaves that cell showing its last
    /// value, muted, rather than failing the frame.
    async fn ha_states(&self, device: &Device) -> HashMap<Reading, Reported> {
        // Every widget, group children included, and every configured reading
        // within each: a `list` cell is made of readings and a `weather` cell hangs
        // them off its condition, so a fetch list built from widgets alone would
        // leave those rows permanently empty.
        let mut readings: Vec<Reading> = Vec::new();
        for widget in device.all_widgets() {
            if let Some(entity) = &widget.entity {
                readings.push(match &widget.attribute {
                    Some(attribute) => Reading::attribute(entity, attribute),
                    None => Reading::state(entity),
                });
            }
            for reading in &widget.readings {
                readings.push(match &reading.attribute {
                    Some(attribute) => Reading::attribute(&reading.entity, attribute),
                    None => Reading::state(&reading.entity),
                });
            }
        }
        if readings.is_empty() {
            return HashMap::new();
        }

        let results = match &self.ha {
            Some(client) => fetch_readings(client.as_ref(), &readings).await,
            None => readings
                .into_iter()
                .map(|reading| (reading, Err("Home Assistant is not configured".to_owned())))
                .collect(),
        };

        self.last_good
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(device.id.clone())
            .or_default()
            .fold(results)
    }

    /// Resolves every icon this device's dashboard references.
    ///
    /// Keyed by the spec rather than by widget, so two cells asking for the same
    /// icon share one fetch and one cache entry. A beacon's two state icons are
    /// collected alongside every header icon, because they are drawn by the same
    /// code path and so must be resolved by the same one.
    async fn icons(&self, device: &Device) -> HashMap<String, crate::icon::Icon> {
        let Some(store) = &self.icons else {
            return HashMap::new();
        };
        // Group children are walked too: a nested cell's icon is fetched exactly
        // like a top-level one's.
        let specs: Vec<String> = device
            .all_widgets()
            .flat_map(|widget| {
                [&widget.icon, &widget.icon_on, &widget.icon_off]
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect();
        if specs.is_empty() {
            return HashMap::new();
        }
        store.fetch(&specs).await
    }

    /// Resolves a tap against a device's dashboard and performs whatever it names.
    ///
    /// Never fails and never writes a frame: a tap that changed something asks the
    /// render loop for a rebuild like every other change, so the panel sees it at
    /// its next poll rather than through a second, parallel render path.
    pub async fn tap(
        &self,
        device_id: &str,
        x: f32,
        y: f32,
        event_id: Option<&str>,
    ) -> tap::Report {
        let config = self.config();
        let Some(device) = config.devices.iter().find(|device| device.id == device_id) else {
            return tap::Report::bare(tap::Outcome::NoTarget);
        };

        // Deduplicated before the hit test, so a retry costs nothing and cannot act
        // even when it lands squarely on a cell.
        if let Some(event_id) = event_id
            && self.taps.seen(device_id, event_id)
        {
            tracing::debug!(
                device = device_id,
                event = event_id,
                "tap already handled; ignoring the repeat"
            );
            return tap::Report::bare(tap::Outcome::Deduped);
        }

        let Some(widget) = render::Layout::for_device(device).hit(device, x, y) else {
            return tap::Report::bare(tap::Outcome::NoTarget);
        };
        tap::dispatch(self.ha.as_deref(), widget).await
    }

    /// Renders every configured device once.
    ///
    /// Called before the listener starts accepting, so a device that polls
    /// immediately gets a real frame rather than a placeholder.
    pub async fn render_all(&self, now: OffsetDateTime) {
        for device_id in self.config().devices.iter().map(|device| device.id.clone()) {
            if let Err(error) = self.render_device(&device_id, now).await {
                tracing::error!(
                    device = %device_id,
                    error = format!("{error:#}"),
                    "initial render failed; this device will serve a placeholder until it succeeds"
                );
            }
        }
    }

    /// The placeholder frame for an unconfigured device id.
    ///
    /// The only frame the HTTP layer ever rasterises, and it cannot be
    /// pre-rendered because the whole point is that the id was not anticipated.
    /// Caching keeps a device polling a mistyped URL every few minutes from
    /// re-rendering every time.
    pub fn placeholder(&self, requested: &str, size: Option<(u32, u32)>) -> Result<Frame> {
        let (width, height) = size.unwrap_or(DEFAULT_PLACEHOLDER_SIZE);
        let key = (requested.to_owned(), width, height);

        let mut cache = self
            .placeholders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(frame) = cache.get(&key) {
            return Ok(frame.clone());
        }

        let config = self.config();
        let configured: Vec<String> = config
            .devices
            .iter()
            .map(|device| device.id.clone())
            .collect();

        // Bayer rather than error diffusion: this frame is static, and an ordered
        // dither keeps it byte-identical between renders.
        let bytes = render::render_placeholder(
            &self.fonts,
            requested,
            &configured,
            width,
            height,
            crate::config::Palette::Gray16,
            crate::config::Dither::Bayer,
        )?;
        let frame = Frame {
            hash: render::frame_hash(&bytes),
            bytes: bytes.into(),
            rendered_at: OffsetDateTime::now_utc(),
        };

        // Wholesale eviction rather than an LRU: the cache exists to absorb one
        // misconfigured device, and a flood of distinct ids is not worth tracking
        // recency for.
        if cache.len() >= MAX_PLACEHOLDERS {
            cache.clear();
        }
        cache.insert(key, frame.clone());
        Ok(frame)
    }

    /// A placeholder frame previously handed out, looked up by hash so the image
    /// URL in a placeholder response is fetchable.
    pub fn placeholder_by_hash(&self, hash: &str) -> Option<Frame> {
        self.placeholders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .find(|frame| frame.hash == hash)
            .cloned()
    }

    /// `refresh_rate` to send alongside a placeholder.
    pub fn placeholder_refresh_rate(&self) -> u32 {
        PLACEHOLDER_REFRESH_RATE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_DEVICE: &str = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "bayer"
refresh_rate = 300
grid = { cols = 1, rows = 1 }
"#;

    /// A config file in a per-test temporary directory, removed on drop.
    struct Fixture {
        path: std::path::PathBuf,
    }

    impl Fixture {
        fn new(contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "paneld-reload-{}-{:?}.toml",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&path, contents).unwrap();
            Self { path }
        }

        fn write(&self, contents: &str) {
            std::fs::write(&self.path, contents).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn runtime(toml: &str) -> Arc<Runtime> {
        let mut config = crate::config::parse(toml).unwrap();
        config.server.content_path = std::env::temp_dir()
            .join(format!(
                "paneld-reload-content-{}-{:?}.json",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned();
        Runtime::with_home_assistant(config, None).unwrap().0
    }

    #[test]
    fn a_valid_reload_takes_effect() {
        let fixture = Fixture::new(ONE_DEVICE);
        let runtime = runtime(ONE_DEVICE);
        assert_eq!(runtime.config().devices[0].refresh_rate, 300);

        fixture.write(&ONE_DEVICE.replace("refresh_rate = 300", "refresh_rate = 600"));
        runtime.reload(&fixture.path).unwrap();
        assert_eq!(runtime.config().devices[0].refresh_rate, 600);
    }

    #[test]
    fn a_malformed_reload_leaves_the_previous_configuration_in_effect() {
        // A typo must never blank the panel.
        let fixture = Fixture::new(ONE_DEVICE);
        let runtime = runtime(ONE_DEVICE);

        fixture.write("this is not TOML [[[");
        let error = runtime
            .reload(&fixture.path)
            .expect_err("a malformed file must be rejected");
        assert!(format!("{error:#}").contains("TOML"), "{error:#}");
        assert_eq!(
            runtime.config().devices.len(),
            1,
            "the previous configuration must still be in effect"
        );
        assert_eq!(runtime.config().devices[0].refresh_rate, 300);
    }

    #[test]
    fn a_reload_that_fails_validation_leaves_the_previous_configuration_in_effect() {
        // Parses as TOML but breaks a rule: still must not take effect.
        let fixture = Fixture::new(ONE_DEVICE);
        let runtime = runtime(ONE_DEVICE);

        fixture.write(&ONE_DEVICE.replace("refresh_rate = 300", "refresh_rate = 1"));
        let error = runtime.reload(&fixture.path).expect_err("out of range");
        assert!(format!("{error:#}").contains("refresh_rate 1"), "{error:#}");
        assert_eq!(runtime.config().devices[0].refresh_rate, 300);
    }

    #[test]
    fn a_reload_of_a_missing_file_leaves_the_previous_configuration_in_effect() {
        let runtime = runtime(ONE_DEVICE);
        let missing = std::env::temp_dir().join("paneld-does-not-exist.toml");
        assert!(runtime.reload(&missing).is_err());
        assert_eq!(runtime.config().devices.len(), 1);
    }
}
