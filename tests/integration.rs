//! Three seams the protocol suite does not reach: Home Assistant degradation
//! through the real composition root, device independence, and render cadence.
//!
//! Nothing here performs network I/O. The Home Assistant cases inject a stub at
//! `Runtime::with_home_assistant`, which is the only place the process ever
//! constructs the real HTTP client, so the `base_url` in these fixtures is never
//! resolved let alone contacted.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{Harness, TWO_DEVICES, is_png, png_dimensions};
use paneld::app::Runtime;
use paneld::frame::Frame;
use paneld::ha::HaClient;
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;

/// Matches `public_base_url` in every fixture here, so an `image_url` off the wire
/// can be turned back into a request path.
const BASE_URL: &str = "http://192.168.0.50:4444";

/// Short enough that no interval has elapsed.
const NO_TIME: Duration = Duration::from_secs(0);

// ---------------------------------------------------------------------------
// Home Assistant, through the real composition root
// ---------------------------------------------------------------------------

/// One `ha_entity` cell, and the `[home_assistant]` section config validation
/// demands before it will accept one.
const ONE_ENTITY: &str = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[home_assistant]
base_url = "http://homeassistant.invalid:8123"
token = "never-sent-anywhere"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "bayer"
refresh_rate = 300
render_interval = 300
grid = { cols = 1, rows = 1 }

[[device.widget]]
id = "office_temp"
kind = "ha_entity"
col = 0
row = 0
label = "Office"
unit = "C"
entity = "sensor.office_temperature"
"#;

/// Two `ha_entity` cells, so one can fail while the other answers.
const TWO_ENTITIES: &str = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[home_assistant]
base_url = "http://homeassistant.invalid:8123"
token = "never-sent-anywhere"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "bayer"
refresh_rate = 300
render_interval = 300
grid = { cols = 2, rows = 1 }

[[device.widget]]
id = "office_temp"
kind = "ha_entity"
col = 0
row = 0
label = "Office"
unit = "C"
entity = "sensor.office_temperature"

[[device.widget]]
id = "hall_door"
kind = "ha_entity"
col = 1
row = 0
label = "Hall"
entity = "binary_sensor.hall_door"
"#;

/// A [`HaClient`] with canned answers and a call counter.
///
/// The counter is the evidence that a render genuinely consulted Home Assistant
/// rather than short-circuiting; it is an atomic rather than a `Mutex` because the
/// renderer fetches a dashboard's entities concurrently and a lock here would let
/// the stub itself serialise what we are trying to observe.
struct StubHa {
    answers: HashMap<String, Result<String, String>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl HaClient for StubHa {
    async fn read(&self, reading: &paneld::ha::Reading) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match self.answers.get(reading.entity_id.as_str()) {
            Some(Ok(state)) => Ok(state.clone()),
            Some(Err(message)) => Err(anyhow::anyhow!(message.clone())),
            None => Err(anyhow::anyhow!("the stub has no answer for `{reading}`")),
        }
    }
}

/// A runtime built over a stub Home Assistant, driven by `render_device` directly.
///
/// Separate from [`Harness`] because the injection point is the constructor, and
/// because these cases care about frame bytes rather than the HTTP surface.
struct HaFixture {
    runtime: Arc<Runtime>,
    calls: Arc<AtomicUsize>,
    /// Held only to keep the wake channel open for the runtime's lifetime.
    _wake: Receiver<String>,
    content_path: PathBuf,
}

impl HaFixture {
    /// `tag` must be unique per fixture: it names a private content file, so two
    /// fixtures sharing a tag would share a content store and confound a
    /// byte-for-byte comparison.
    async fn start(toml: &str, tag: &str, answers: &[(&str, Result<&str, &str>)]) -> Self {
        let content_path = std::env::temp_dir().join(format!(
            "paneld-integration-{}-{tag}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&content_path);

        let mut config = paneld::config::parse(toml).expect("fixture config should be valid");
        config.server.content_path = content_path.to_string_lossy().into_owned();

        let calls = Arc::new(AtomicUsize::new(0));
        let stub = StubHa {
            answers: answers
                .iter()
                .map(|(entity, answer)| {
                    let answer = match answer {
                        Ok(state) => Ok((*state).to_owned()),
                        Err(message) => Err((*message).to_owned()),
                    };
                    ((*entity).to_owned(), answer)
                })
                .collect(),
            calls: Arc::clone(&calls),
        };

        let (runtime, wake) = Runtime::with_home_assistant(config, Some(Box::new(stub)))
            .expect("runtime should build");
        Self {
            runtime,
            calls,
            _wake: wake,
            content_path,
        }
    }

    /// Renders one device and returns the frame now being served.
    async fn render(&self, device: &str, now: OffsetDateTime) -> Frame {
        self.runtime
            .render_device(device, now)
            .await
            .expect("a Home Assistant failure must never fail the whole render");
        self.runtime
            .frames
            .current(device)
            .expect("a completed render must leave a frame to serve")
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Drop for HaFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.content_path);
    }
}

#[tokio::test]
async fn a_reachable_home_assistant_entity_renders_a_frame_of_the_configured_size() {
    let fixture = HaFixture::start(
        ONE_ENTITY,
        "ha-healthy",
        &[("sensor.office_temperature", Ok("21.4"))],
    )
    .await;

    let frame = fixture.render("kindle", OffsetDateTime::now_utc()).await;

    assert!(is_png(&frame.bytes));
    assert_eq!(png_dimensions(&frame.bytes), (400, 300));
    assert_eq!(
        fixture.calls(),
        1,
        "the render must actually read the entity, once"
    );
}

#[tokio::test]
async fn an_unreachable_home_assistant_entity_still_renders_a_full_size_frame() {
    // The failure mode this defends against is a blank panel: an integration that
    // is down must cost one cell, not the dashboard.
    let fixture = HaFixture::start(
        ONE_ENTITY,
        "ha-failing",
        &[("sensor.office_temperature", Err("connection refused"))],
    )
    .await;

    let frame = fixture.render("kindle", OffsetDateTime::now_utc()).await;

    assert!(is_png(&frame.bytes));
    assert_eq!(png_dimensions(&frame.bytes), (400, 300));
    assert_eq!(
        fixture.calls(),
        1,
        "a failing entity must still have been attempted"
    );
}

#[tokio::test]
async fn an_unavailable_cell_is_drawn_differently_from_a_healthy_one() {
    // Both renders are handed the same instant, so any byte difference is the
    // entity's state and nothing else.
    let now = OffsetDateTime::now_utc();
    let healthy = HaFixture::start(
        ONE_ENTITY,
        "ha-pair-ok",
        &[("sensor.office_temperature", Ok("21.4"))],
    )
    .await;
    let failing = HaFixture::start(
        ONE_ENTITY,
        "ha-pair-err",
        &[("sensor.office_temperature", Err("connection refused"))],
    )
    .await;

    let healthy = healthy.render("kindle", now).await;
    let failing = failing.render("kindle", now).await;

    assert_ne!(
        failing.bytes, healthy.bytes,
        "an unavailable cell that renders identically to a value is a silent lie"
    );
    assert_ne!(
        failing.hash, healthy.hash,
        "the device's cache key must change too, or the panel never repaints"
    );
}

#[tokio::test]
async fn one_failing_entity_does_not_blank_a_dashboard_whose_other_entity_answers() {
    let now = OffsetDateTime::now_utc();
    let both_healthy = HaFixture::start(
        TWO_ENTITIES,
        "ha-two-ok",
        &[
            ("sensor.office_temperature", Ok("21.4")),
            ("binary_sensor.hall_door", Ok("off")),
        ],
    )
    .await;
    let one_failing = HaFixture::start(
        TWO_ENTITIES,
        "ha-two-mixed",
        &[
            ("sensor.office_temperature", Ok("21.4")),
            ("binary_sensor.hall_door", Err("504 Gateway Timeout")),
        ],
    )
    .await;

    let intact = both_healthy.render("kindle", now).await;
    let degraded = one_failing.render("kindle", now).await;

    assert_eq!(png_dimensions(&degraded.bytes), (400, 300));
    assert_ne!(
        degraded.bytes, intact.bytes,
        "the failing cell must be visibly unavailable, not silently identical"
    );
    assert_eq!(
        one_failing.calls(),
        2,
        "one entity failing must not abandon the other"
    );
}

// ---------------------------------------------------------------------------
// Multi-device independence
// ---------------------------------------------------------------------------

/// Two devices that share one widget id and each own a private one, so fan-out
/// and its absence are both observable.
const TWO_INDEPENDENT: &str = r#"
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
render_interval = 300
grid = { cols = 2, rows = 1 }

[[device.widget]]
id = "shared"
kind = "value"
col = 0
row = 0
label = "Shared"

[[device.widget]]
id = "kindle_only"
kind = "value"
col = 1
row = 0
label = "Kindle"

[[device]]
id = "kitchen"
width = 240
height = 160
palette = "mono"
dither = "bayer"
refresh_rate = 900
render_interval = 900
grid = { cols = 2, rows = 1 }

[[device.widget]]
id = "shared"
kind = "text"
col = 0
row = 0
label = "Shared"

[[device.widget]]
id = "kitchen_only"
kind = "text"
col = 1
row = 0
label = "Kitchen"
"#;

/// The bytes a device is currently being served, fetched over HTTP the way the
/// firmware does: poll, then GET the `image_url` it was handed.
async fn served_bytes(harness: &Harness, device: &str) -> Vec<u8> {
    let body = harness.poll(device).await;
    let url = body["image_url"]
        .as_str()
        .expect("image_url should be a string");
    let (status, bytes) = harness
        .get_bytes(
            url.strip_prefix(BASE_URL)
                .unwrap_or_else(|| panic!("`{url}` should sit under the configured base url")),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    bytes
}

#[tokio::test]
async fn each_device_serves_its_own_dashboard_and_never_another_devices_frame() {
    let harness = Harness::start(TWO_DEVICES).await;

    let kindle = served_bytes(&harness, "kindle").await;
    let kitchen = served_bytes(&harness, "kitchen").await;

    assert_eq!(png_dimensions(&kindle), (400, 300));
    assert_eq!(png_dimensions(&kitchen), (240, 160));
    assert_ne!(
        kindle, kitchen,
        "two devices differing in size and palette must not share bytes"
    );
    assert_ne!(
        harness.filename("kindle").await,
        harness.filename("kitchen").await,
        "the cache key is per-device or a panel repaints with its neighbour's frame"
    );
}

#[tokio::test]
async fn rendering_one_device_leaves_the_other_devices_filename_alone() {
    // Driven by the interval rather than a push: kindle rebuilds every 300s and
    // kitchen every 900s, so one tick moves exactly one device.
    let mut harness = Harness::start(TWO_INDEPENDENT).await;
    let kitchen_before = harness.filename("kitchen").await;
    let kindle_before = harness.filename("kindle").await;

    let (status, _) = harness
        .put("/api/content/kindle_only", json!({ "value": 42 }))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let rendered = harness.tick(Duration::from_secs(301)).await;
    assert_eq!(rendered, ["kindle"], "only kindle's interval has elapsed");

    assert_ne!(
        harness.filename("kindle").await,
        kindle_before,
        "kindle's content changed, so its cache key must have"
    );
    assert_eq!(
        harness.filename("kitchen").await,
        kitchen_before,
        "kitchen was not rendered, so it must not be told to repaint"
    );
    assert_eq!(
        harness.render_count("kitchen").await,
        1,
        "startup render only"
    );
}

#[tokio::test]
async fn a_widget_on_both_dashboards_rebuilds_both_devices_once_each() {
    let mut harness = Harness::start(TWO_INDEPENDENT).await;

    let (status, _) = harness
        .put("/api/content/shared", json!({ "value": 7, "render": true }))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let mut rendered = harness.tick(NO_TIME).await;
    rendered.sort();
    assert_eq!(rendered, ["kindle", "kitchen"]);

    // Startup render plus this one. A second render per device would mean the
    // fan-out queued a device twice.
    assert_eq!(harness.render_count("kindle").await, 2);
    assert_eq!(harness.render_count("kitchen").await, 2);
}

#[tokio::test]
async fn a_widget_on_one_dashboard_rebuilds_only_that_device() {
    let mut harness = Harness::start(TWO_INDEPENDENT).await;

    let (status, _) = harness
        .put(
            "/api/content/kitchen_only",
            json!({ "value": "boil the kettle", "render": true }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    assert_eq!(harness.tick(NO_TIME).await, ["kitchen"]);
    assert_eq!(harness.render_count("kitchen").await, 2);
    assert_eq!(
        harness.render_count("kindle").await,
        1,
        "a push to a widget kindle does not declare must not cost it a render"
    );
}

// ---------------------------------------------------------------------------
// Cadence configuration
// ---------------------------------------------------------------------------

/// A single-device fixture whose cadence lines the test supplies.
fn cadence_fixture(cadence: &str) -> String {
    format!(
        r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "bayer"
{cadence}
grid = {{ cols = 1, rows = 1 }}

[[device.widget]]
id = "reading"
kind = "value"
col = 0
row = 0
label = "Reading"
"#
    )
}

#[tokio::test]
async fn render_interval_defaults_to_refresh_rate_when_absent() {
    let mut harness = Harness::start(&cadence_fixture("refresh_rate = 300")).await;

    assert_eq!(
        harness.tick(Duration::from_secs(299)).await,
        Vec::<String>::new(),
        "nothing is due before the inherited 300-second interval elapses"
    );
    assert_eq!(harness.tick(Duration::from_secs(2)).await, ["kindle"]);
}

#[tokio::test]
async fn an_explicit_render_interval_is_independent_of_refresh_rate() {
    // Rebuilding faster than the device polls is a legitimate configuration —
    // a fresher frame waiting when it next wakes — so the two clocks must not be
    // conflated.
    let mut harness =
        Harness::start(&cadence_fixture("refresh_rate = 300\nrender_interval = 5")).await;

    assert_eq!(
        harness.poll("kindle").await["refresh_rate"],
        300,
        "the device is still told to sleep for its own refresh_rate"
    );
    assert_eq!(
        harness.tick(Duration::from_secs(5)).await,
        ["kindle"],
        "the server rebuilds on render_interval, not on refresh_rate"
    );
}

#[tokio::test]
async fn a_device_is_not_rendered_twice_for_one_elapsed_interval() {
    let mut harness =
        Harness::start(&cadence_fixture("refresh_rate = 300\nrender_interval = 5")).await;

    assert_eq!(harness.tick(Duration::from_secs(6)).await, ["kindle"]);
    assert_eq!(
        harness.tick(NO_TIME).await,
        Vec::<String>::new(),
        "the elapsed interval was consumed by the render it caused"
    );
    assert_eq!(harness.render_count("kindle").await, 2, "startup plus one");
}

#[tokio::test]
async fn two_devices_with_different_render_intervals_become_due_independently() {
    let fixture = format!(
        "{}{}",
        cadence_fixture("refresh_rate = 300\nrender_interval = 5"),
        r#"
[[device]]
id = "kitchen"
width = 240
height = 160
palette = "mono"
dither = "bayer"
refresh_rate = 300
render_interval = 60
grid = { cols = 1, rows = 1 }

[[device.widget]]
id = "kettle"
kind = "text"
col = 0
row = 0
label = "Kettle"
"#
    );
    let mut harness = Harness::start(&fixture).await;

    assert_eq!(
        harness.tick(Duration::from_secs(6)).await,
        ["kindle"],
        "the fast device must not drag the slow one along with it"
    );

    let mut rendered = harness.tick(Duration::from_secs(54)).await;
    rendered.sort();
    assert_eq!(
        rendered,
        ["kindle", "kitchen"],
        "kitchen becomes due on its own 60-second clock"
    );
}
