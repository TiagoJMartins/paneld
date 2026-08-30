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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{Harness, TWO_DEVICES, is_png, png_dimensions};
use http_body_util::BodyExt;
use paneld::app::Runtime;
use paneld::config::ServiceCall;
use paneld::frame::Frame;
use paneld::ha::HaClient;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use tower::ServiceExt;

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
    /// Every service call posted, in order. A `Mutex` rather than a counter
    /// because a tap's whole point is the body it sends, and asserting on that is
    /// the only way to know the right entity was actuated.
    services: Arc<Mutex<Vec<ServiceCall>>>,
    /// What Home Assistant says when asked to call a service, so the refusal path
    /// is exercised over the same wire as the success path.
    refuses: Option<String>,
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

    async fn call(&self, call: &ServiceCall) -> anyhow::Result<()> {
        self.services.lock().unwrap().push(call.clone());
        match &self.refuses {
            Some(message) => Err(anyhow::anyhow!(message.clone())),
            None => Ok(()),
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
    services: Arc<Mutex<Vec<ServiceCall>>>,
    /// Held only to keep the wake channel open for the runtime's lifetime.
    _wake: Receiver<String>,
    state_path: PathBuf,
}

impl HaFixture {
    /// `tag` must be unique per fixture: it names a private state file, so two
    /// fixtures sharing a tag would share a store and confound a byte-for-byte
    /// comparison.
    async fn start(toml: &str, tag: &str, answers: &[(&str, Result<&str, &str>)]) -> Self {
        Self::start_with(toml, tag, answers, None).await
    }

    /// Starts with a Home Assistant that refuses every service call, so a tap's
    /// failure path runs through the same composition root as its success path.
    async fn start_refusing(
        toml: &str,
        tag: &str,
        answers: &[(&str, Result<&str, &str>)],
        message: &str,
    ) -> Self {
        Self::start_with(toml, tag, answers, Some(message.to_owned())).await
    }

    async fn start_with(
        toml: &str,
        tag: &str,
        answers: &[(&str, Result<&str, &str>)],
        refuses: Option<String>,
    ) -> Self {
        let state_path = std::env::temp_dir().join(format!(
            "paneld-integration-{}-{tag}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&state_path);

        let mut config = paneld::config::parse(toml).expect("fixture config should be valid");
        config.server.state_path = state_path.to_string_lossy().into_owned();

        let calls = Arc::new(AtomicUsize::new(0));
        let services = Arc::new(Mutex::new(Vec::new()));
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
            services: Arc::clone(&services),
            refuses,
        };

        let (runtime, wake) = Runtime::with_home_assistant(config, Some(Box::new(stub)))
            .expect("runtime should build");
        Self {
            runtime,
            calls,
            services,
            _wake: wake,
            state_path,
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

    /// Every service call Home Assistant was asked to make, in order.
    fn services(&self) -> Vec<ServiceCall> {
        self.services.lock().unwrap().clone()
    }

    fn router(&self) -> Router {
        paneld::http::router(Arc::clone(&self.runtime))
    }

    /// `POST /d/{device}/api/tap`, returning the HTTP status and the body.
    ///
    /// The status is returned rather than asserted so a case can pin the
    /// always-200 rule for itself.
    async fn tap(&self, device: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(format!("/d/{device}/api/tap"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.send(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self
            .router()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("reading the response body")
            .to_bytes();
        let json = serde_json::from_slice(&body).unwrap_or_else(|error| {
            panic!(
                "response body is not JSON ({error}): {}",
                String::from_utf8_lossy(&body)
            )
        });
        (status, json)
    }
}

impl Drop for HaFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.state_path);
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

/// One device whose dashboard puts two cells inside a group, so that a push has to
/// resolve through a nested widget to reach a device.
const GROUPED: &str = r#"
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
id = "beside"
kind = "value"
col = 0
row = 0
label = "Beside"

[[device.widget]]
id = "box"
kind = "group"
col = 1
row = 0
grid = { cols = 1, rows = 2 }

[[device.widget.widget]]
id = "nested"
kind = "value"
col = 0
row = 0
label = "Nested"

[[device.widget.widget]]
id = "also_nested"
kind = "text"
col = 0
row = 1
label = "Also"
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

#[tokio::test]
async fn a_push_to_a_cell_inside_a_group_rebuilds_its_device() {
    // A group's children have push addresses like any other cell, and the lookup
    // that turns an address into a device used to walk only the widgets beside the
    // group. A publisher was told `200 OK`, nothing was queued, and the panel went
    // on showing the previous value until its render interval came round.
    let mut harness = Harness::start(GROUPED).await;

    let (status, _) = harness
        .put("/api/content/nested", json!({ "value": 7, "render": true }))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    assert_eq!(harness.tick(NO_TIME).await, ["kindle"]);
    assert_eq!(harness.render_count("kindle").await, 2, "startup plus one");
}

/// One device whose Slack indicator lives on the status bar rather than in a cell.
const ALERTING: &str = r#"
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
grid = { cols = 1, rows = 1 }

[device.status_bar]
edge = "bottom"
fields = ["date"]

[[device.status_bar.alert]]
id = "slack_unread"
label = "SLACK"

[[device.widget]]
id = "only"
kind = "value"
col = 0
row = 0
"#;

#[tokio::test]
async fn a_push_to_a_status_bar_alert_rebuilds_its_device() {
    // An alert exists to appear when it is triggered. If the address that raises it
    // resolved to no device, the panel would not show it until the render interval
    // came round — five minutes late, which for a notification is not late but
    // wrong.
    let mut harness = Harness::start(ALERTING).await;

    let (status, _) = harness
        .put(
            "/api/content/slack_unread",
            json!({ "value": "on", "state": "alert", "render": true }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    assert_eq!(harness.tick(NO_TIME).await, ["kindle"]);
    assert_eq!(harness.render_count("kindle").await, 2, "startup plus one");
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

// ---------------------------------------------------------------------------
// Tap actions, end to end over the HTTP surface
// ---------------------------------------------------------------------------

/// A 400x300 panel on a 2x2 grid, with one tappable cell, one inert cell, and two
/// cells left empty.
const TAPPABLE: &str = r#"
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
grid = { cols = 2, rows = 2 }

[[device.widget]]
id = "desk_lamp"
kind = "ha_entity"
col = 0
row = 0
label = "Desk"
entity = "light.desk"
tap = "light.toggle"

[[device.widget]]
id = "office_temp"
kind = "ha_entity"
col = 1
row = 0
label = "Office"
unit = "C"
entity = "sensor.office_temperature"
"#;

/// The states [`TAPPABLE`]'s two cells read, so a render succeeds and the frame the
/// poll serves is a real one.
const TAPPABLE_STATES: &[(&str, Result<&str, &str>)] = &[
    ("light.desk", Ok("off")),
    ("sensor.office_temperature", Ok("21.4")),
];

// Points on [`TAPPABLE`]'s frame, derived here rather than from the renderer so
// that these cases would catch the geometry changing under them.
//
// The smallest side of a cell is min(400/2, 300/2) = 150, so the gutter is
// (150 * 0.06).clamp(1, 10) = 9. A cell is then (400 - 9*3)/2 = 186.5 wide and
// (300 - 9*3)/2 = 136.5 tall, and cell (col, row) starts at
// 9 + col * 195.5, 9 + row * 145.5.

/// The middle of the tappable cell at (0, 0).
const ON_DESK_LAMP: (f32, f32) = (102.25, 77.25);

/// The middle of the inert cell at (1, 0).
const ON_OFFICE_TEMP: (f32, f32) = (297.75, 77.25);

/// Between the two columns: cell (0, 0) ends at 195.5 and cell (1, 0) begins at
/// 204.5, so this belongs to neither.
const IN_A_GUTTER: (f32, f32) = (200.0, 77.25);

/// The middle of the empty cell at (0, 1).
const ON_AN_EMPTY_CELL: (f32, f32) = (102.25, 222.75);

fn at(point: (f32, f32), event_id: Option<&str>) -> Value {
    match event_id {
        Some(event_id) => json!({ "x": point.0, "y": point.1, "event_id": event_id }),
        None => json!({ "x": point.0, "y": point.1 }),
    }
}

#[tokio::test]
async fn a_tap_on_a_widget_dispatches_exactly_one_service_call() {
    let fixture = HaFixture::start(TAPPABLE, "tap-dispatch", TAPPABLE_STATES).await;

    let (status, body) = fixture.tap("kindle", at(ON_DESK_LAMP, Some("e1"))).await;

    assert_eq!(status, StatusCode::OK, "a tap always answers 200");
    assert_eq!(body["status"], 0, "0 means the tap was understood");
    assert_eq!(body["outcome"], "dispatched");
    assert_eq!(body["widget"], "desk_lamp");
    assert_eq!(body["detail"], "light.toggle");

    let services = fixture.services();
    assert_eq!(services.len(), 1, "one tap is one call: {services:?}");
    assert_eq!(services[0].domain, "light");
    assert_eq!(services[0].service, "toggle");
    assert_eq!(
        Value::Object(services[0].data.clone()),
        json!({ "entity_id": "light.desk" }),
        "the body is the widget's own entity, resolved by config"
    );
}

#[tokio::test]
async fn a_replayed_event_id_dispatches_nothing_more() {
    let fixture = HaFixture::start(TAPPABLE, "tap-dedupe", TAPPABLE_STATES).await;

    let (_, first) = fixture.tap("kindle", at(ON_DESK_LAMP, Some("e7"))).await;
    assert_eq!(first["outcome"], "dispatched");

    let (status, replay) = fixture.tap("kindle", at(ON_DESK_LAMP, Some("e7"))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["outcome"], "deduped");
    assert_eq!(
        fixture.services().len(),
        1,
        "a client that retries must not toggle the light twice"
    );

    // A different id is a different tap, which is the line the ledger must not
    // blur: swallowing a deliberate second press is worse than the retry it guards.
    let (_, again) = fixture.tap("kindle", at(ON_DESK_LAMP, Some("e8"))).await;
    assert_eq!(again["outcome"], "dispatched");
    assert_eq!(fixture.services().len(), 2);
}

#[tokio::test]
async fn a_tap_that_lands_on_no_cell_dispatches_nothing() {
    let fixture = HaFixture::start(TAPPABLE, "tap-no-target", TAPPABLE_STATES).await;

    for (what, point) in [
        ("a gutter", IN_A_GUTTER),
        ("an empty cell", ON_AN_EMPTY_CELL),
    ] {
        let (status, body) = fixture.tap("kindle", at(point, None)).await;

        assert_eq!(status, StatusCode::OK, "{what}");
        assert_eq!(body["outcome"], "no_target", "{what}");
        assert_eq!(body["widget"], Value::Null, "{what}");
        assert_eq!(body["detail"], Value::Null, "{what}");
    }
    assert!(
        fixture.services().is_empty(),
        "a miss must never fire whichever action happened to be closest"
    );
}

#[tokio::test]
async fn a_tap_on_a_widget_with_no_tap_reports_that_it_is_inert() {
    let fixture = HaFixture::start(TAPPABLE, "tap-no-action", TAPPABLE_STATES).await;

    let (status, body) = fixture.tap("kindle", at(ON_OFFICE_TEMP, None)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "no_action");
    assert_eq!(
        body["widget"], "office_temp",
        "the cell is still named, which is how a caller tells this from a miss"
    );
    assert!(fixture.services().is_empty());
}

#[tokio::test]
async fn a_home_assistant_failure_is_reported_and_the_server_keeps_serving() {
    let fixture = HaFixture::start_refusing(
        TAPPABLE,
        "tap-failed",
        TAPPABLE_STATES,
        "Home Assistant returned HTTP 400: entity light.desk is unknown",
    )
    .await;
    fixture.render("kindle", OffsetDateTime::now_utc()).await;

    let (status, body) = fixture.tap("kindle", at(ON_DESK_LAMP, Some("e1"))).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "an unreachable integration is never a transport failure"
    );
    assert_eq!(body["outcome"], "failed");
    assert_eq!(body["widget"], "desk_lamp");
    assert_eq!(
        body["detail"], "light.toggle",
        "the detail names the action; Home Assistant's reason goes to the log"
    );
    assert_eq!(fixture.services().len(), 1, "it was attempted");

    let (status, poll) = fixture.get("/d/kindle/api/display").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(poll["status"], 0, "the poll is unaffected: {poll}");
    assert!(
        poll["filename"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "the panel is still being served a frame: {poll}"
    );
}

#[tokio::test]
async fn an_unconfigured_device_id_is_an_error_in_the_body_not_the_status_line() {
    let fixture = HaFixture::start(TAPPABLE, "tap-unknown-device", TAPPABLE_STATES).await;

    let (status, body) = fixture.tap("kindl", at(ON_DESK_LAMP, None)).await;

    assert_eq!(status, StatusCode::OK, "the same rule as the display poll");
    assert_eq!(body["status"], 500);
    assert_eq!(body["outcome"], "no_target");
    assert!(fixture.services().is_empty());
}

/// tesserae spells the same two numbers `x0` and `y0`, and a client written against
/// it must work here unchanged.
#[tokio::test]
async fn x0_and_y0_are_accepted_as_aliases() {
    let fixture = HaFixture::start(TAPPABLE, "tap-aliases", TAPPABLE_STATES).await;

    let (status, body) = fixture
        .tap(
            "kindle",
            json!({ "x0": ON_DESK_LAMP.0, "y0": ON_DESK_LAMP.1 }),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "dispatched", "{body}");
    assert_eq!(fixture.services().len(), 1);
}

#[tokio::test]
async fn a_body_that_is_not_a_tap_is_refused_without_a_failing_status_line() {
    let fixture = HaFixture::start(TAPPABLE, "tap-malformed", TAPPABLE_STATES).await;

    let (status, body) = fixture.tap("kindle", json!({ "x": "over there" })).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "everything under /d/ answers 200, so no firmware learns to back off"
    );
    assert_eq!(body["status"], 400);
    assert_eq!(body["outcome"], "no_target");
    assert!(fixture.services().is_empty());
}

/// The path for a client that can only decorate the request it already makes: the
/// tap must take effect on this wake, and the poll must answer exactly as it would
/// have without it.
#[tokio::test]
async fn a_tap_carried_on_a_display_poll_dispatches_and_still_answers_the_poll() {
    let fixture = HaFixture::start(TAPPABLE, "tap-on-poll", TAPPABLE_STATES).await;
    let frame = fixture.render("kindle", OffsetDateTime::now_utc()).await;

    let (status, body) = fixture
        .get(&format!(
            "/d/kindle/api/display?touch_x={}&touch_y={}&touch_event_id=e1",
            ON_DESK_LAMP.0, ON_DESK_LAMP.1
        ))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], 0, "{body}");
    assert_eq!(
        body["filename"],
        format!("{}.png", frame.hash),
        "the poll answers with the frame it would have answered with anyway"
    );
    assert_eq!(
        body["image_url"],
        format!("{BASE_URL}/d/kindle/frames/{}.png", frame.hash)
    );
    assert_eq!(body["refresh_rate"], 300);

    let services = fixture.services();
    assert_eq!(services.len(), 1, "the tap took effect on this wake");
    assert_eq!(services[0].to_string(), "light.toggle");

    // The event id came along, so the same decorated poll repeated is one tap.
    let (_, repeat) = fixture
        .get(&format!(
            "/d/kindle/api/display?touch_x={}&touch_y={}&touch_event_id=e1",
            ON_DESK_LAMP.0, ON_DESK_LAMP.1
        ))
        .await;
    assert_eq!(repeat["status"], 0);
    assert_eq!(fixture.services().len(), 1, "a repeated poll is not a tap");
}

/// A poll carrying nonsense where its coordinates should be must answer exactly as
/// an undecorated poll does, because the alternative is a panel that stops
/// refreshing over a client-side bug in a feature it does not use.
#[tokio::test]
async fn a_malformed_touch_query_leaves_the_poll_untouched() {
    let fixture = HaFixture::start(TAPPABLE, "tap-bad-query", TAPPABLE_STATES).await;
    let frame = fixture.render("kindle", OffsetDateTime::now_utc()).await;

    let plain = fixture.get("/d/kindle/api/display").await;
    for query in [
        "?touch_x=over-there&touch_y=77.25",
        "?touch_x=102.25",
        "?touch_y=77.25",
        "?touch_event_id=e1",
        "?touch_x=&touch_y=",
    ] {
        let decorated = fixture.get(&format!("/d/kindle/api/display{query}")).await;
        assert_eq!(decorated, plain, "`{query}` changed the poll's answer");
    }

    assert!(
        fixture.services().is_empty(),
        "half a coordinate is a client bug, not a tap"
    );
    assert_eq!(
        plain.1["filename"],
        format!("{}.png", frame.hash),
        "and the frame served is unchanged throughout"
    );
}
