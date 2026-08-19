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
use crate::ha::{HaClient, HttpHaClient, fetch_states};
use crate::render::{self, RenderInputs};
use crate::status::StatusStore;

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

        let runtime = Arc::new(Self {
            config: RwLock::new(Arc::new(config)),
            content,
            frames: FrameStore::new(),
            status: StatusStore::new(),
            fonts: render::fonts().context("loading the embedded fonts")?,
            ha,
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

        // Fetched before the pure render so that rendering itself stays
        // synchronous and reproducible.
        let ha_states = self.fetch_ha_states(device).await;
        let content = self.content.snapshot();

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

    /// Resolves every Home Assistant entity this device's dashboard references.
    ///
    /// A missing client or a per-entity failure leaves that cell unavailable
    /// rather than failing the frame.
    async fn fetch_ha_states(&self, device: &Device) -> HashMap<String, Result<String, String>> {
        let entities: Vec<String> = device
            .widgets
            .iter()
            .filter_map(|widget| widget.entity.clone())
            .collect();
        if entities.is_empty() {
            return HashMap::new();
        }

        match &self.ha {
            Some(client) => fetch_states(client.as_ref(), &entities).await,
            None => entities
                .into_iter()
                .map(|entity| (entity, Err("Home Assistant is not configured".to_owned())))
                .collect(),
        }
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
