//! The print path, end to end: a real frame rendered through the composition
//! root, delivered over real HTTP to a stub nanoprint bridge on loopback.
//!
//! This is deliberately the one suite that performs network I/O — the whole
//! point of the sink is the bytes that leave the process, so the assertion has
//! to sit on the receiving end. The stub binds an ephemeral loopback port and
//! the fixture is formatted around it.

mod common;

use std::future::IntoFuture;
use std::sync::Arc;

use parking_lot::Mutex;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::Harness;

/// The printhead's row width in bytes: 384 dots, one bit each.
const ROW_BYTES: usize = 384 / 8;

/// What the stub bridge saw: the request's query string, content type and raw
/// body.
type Captured = Arc<Mutex<Option<(String, String, Vec<u8>)>>>;

/// A kindle without a sink beside a printer with one, so the same harness
/// answers both "this device prints" and "this one cannot".
fn fixture(bridge_url: &str) -> String {
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
refresh_rate = 300
grid = {{ cols = 1, rows = 1 }}

[[device.widget]]
id = "note"
kind = "text"
col = 0
row = 0
label = "Note"

[[device]]
id = "printer"
width = 384
height = 768
palette = "mono"
dither = "floyd-steinberg"
refresh_rate = 300
grid = {{ cols = 1, rows = 1 }}
sink = {{ kind = "nanoprint", url = "{bridge_url}", density = 2 }}

[[device.widget]]
id = "ticket"
kind = "text"
col = 0
row = 0
label = "Today"
"#
    )
}

/// A printer laid out the way a ticket wants: rows sized to their content, pinned
/// to a legible 20 pixels, and a pushed list as the last cell on the grid.
///
/// The roll is declared long on purpose. `height` is the longest ticket this device
/// may print, not the ticket: what the layout leaves blank the sink trims off.
fn ticket_fixture(bridge_url: &str) -> String {
    format!(
        r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "printer"
width = 384
height = 1200
palette = "mono"
dither = "floyd-steinberg"
refresh_rate = 300
grid = {{ cols = 1, rows = 2, fit = "content" }}
chrome = {{ border = 0, padding = 2, gap = 6 }}
style = {{ row_type = 20, row_width = "full", reading_ceiling = 1.8 }}
sink = {{ kind = "nanoprint", url = "{bridge_url}", density = 2 }}

[[device.widget]]
id = "heading"
kind = "value"
col = 0
row = 0

[[device.widget]]
id = "shopping"
kind = "list"
col = 0
row = 1
"#
    )
}

/// A stub nanoprint bridge on an ephemeral loopback port, serving the two
/// endpoints the sink uses: `GET /status` and `POST /print/raster`.
///
/// The status is settable because refusing to print is as much of the contract as
/// printing: an out-of-paper printer acknowledges a raster like any other, so the
/// only way paneld can keep its promise that a `200` means paper moved is to ask
/// first — and the only way to test that is a bridge that can say no.
struct Bridge {
    url: String,
    captured: Captured,
    status: Arc<Mutex<serde_json::Value>>,
}

impl Bridge {
    /// Binds a bridge whose printer is idle, loaded, cool and charged.
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding a loopback port");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let captured: Captured = Arc::new(Mutex::new(None));
        let status = Arc::new(Mutex::new(ready_status()));

        async fn accept(
            State(state): State<BridgeState>,
            uri: Uri,
            headers: HeaderMap,
            body: Bytes,
        ) -> StatusCode {
            let content_type = headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned();
            *state.captured.lock() = Some((
                uri.query().unwrap_or("").to_owned(),
                content_type,
                body.to_vec(),
            ));
            StatusCode::OK
        }

        async fn serve_status(State(state): State<BridgeState>) -> Json<serde_json::Value> {
            Json(state.status.lock().clone())
        }

        let state = BridgeState {
            captured: Arc::clone(&captured),
            status: Arc::clone(&status),
        };
        let router = Router::new()
            .route("/print/raster", post(accept))
            .route("/status", get(serve_status))
            .with_state(state);
        tokio::spawn(axum::serve(listener, router).into_future());
        Self {
            url,
            captured,
            status,
        }
    }

    /// What the bridge was posted to, if it was.
    fn captured(&self) -> Option<(String, String, Vec<u8>)> {
        self.captured.lock().clone()
    }

    /// The raster the bridge received.
    fn raster(&self) -> Vec<u8> {
        self.captured()
            .expect("the bridge should have been posted to")
            .2
    }

    /// Raises one status flag, leaving the rest of a healthy printer alone.
    fn set_flag(&self, flag: &str, value: bool) {
        let mut status = self.status.lock();
        status[flag] = serde_json::json!(value);
        // `ready` is the bridge's own summary of the same flags, so a stub that
        // left it true while claiming no paper would be testing a bridge that
        // does not exist.
        status["ready"] = serde_json::json!(
            !(status["printing"].as_bool().unwrap()
                || status["cover_open"].as_bool().unwrap()
                || status["paper_empty"].as_bool().unwrap()
                || status["overheating"].as_bool().unwrap())
        );
    }
}

#[derive(Clone)]
struct BridgeState {
    captured: Captured,
    status: Arc<Mutex<serde_json::Value>>,
}

/// The status of a printer with nothing wrong with it.
fn ready_status() -> serde_json::Value {
    serde_json::json!({
        "battery": 87,
        "ready": true,
        "printing": false,
        "cover_open": false,
        "paper_empty": false,
        "low_battery": false,
        "overheating": false,
        "charging": false,
        "model": "A2Y",
        "firmware": "V1.06LY",
    })
}

#[tokio::test]
async fn a_manual_print_delivers_the_served_frame_as_a_packed_raster() {
    let bridge = Bridge::start().await;
    let harness = Harness::start(&fixture(&bridge.url)).await;

    let (status, body) = harness.post_raw("/api/print/printer", Vec::new()).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["printed"], true);
    let bytes = body["bytes"].as_u64().expect("bytes should be a number") as usize;
    let height = body["height_px"]
        .as_u64()
        .expect("height_px should be a number") as usize;
    assert!(bytes > 0, "the label ink must produce a non-empty raster");
    assert_eq!(
        bytes % ROW_BYTES,
        0,
        "the raster must be whole 48-byte rows"
    );
    assert_eq!(height, bytes / ROW_BYTES);

    let (query, content_type, raster) = bridge
        .captured()
        .expect("the bridge should have been posted to");
    assert_eq!(
        query, "density=2",
        "the configured density rides the query string"
    );
    assert_eq!(
        content_type, "application/octet-stream",
        "the bridge documents the raster body as an octet stream"
    );
    assert_eq!(
        raster.len(),
        bytes,
        "the reply must describe the bytes that left"
    );
    assert!(
        raster.iter().any(|&byte| byte != 0),
        "a dashboard with a label prints some ink"
    );
    assert!(
        raster[raster.len() - ROW_BYTES..]
            .iter()
            .any(|&byte| byte != 0),
        "trailing blank rows are trimmed: the last row must hold ink"
    );
}

/// The paper a ticket costs is the paper its content costs.
///
/// The complaint this pins was measured on the roll: one cell stretched to the
/// frame, its type sized to fill it, and a one-item list costing the same paper as a
/// ten-item one. Here the whole path runs twice — push, render, encode, decode,
/// trim, deliver — and what the printhead is sent has to grow by the rows that were
/// added and by nothing else.
#[tokio::test]
async fn a_pushed_lists_ticket_is_as_long_as_the_list() {
    let bridge = Bridge::start().await;
    let mut harness = Harness::start(&ticket_fixture(&bridge.url)).await;

    let (status, _) = harness
        .put(
            "/api/content/heading",
            serde_json::json!({ "value": "Saturday", "render": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let mut printed = Vec::new();
    for count in [3_usize, 10] {
        let rows: Vec<_> = (0..count)
            .map(|index| serde_json::json!({ "label": format!("item {index}"), "value": index }))
            .collect();
        let (status, _) = harness
            .put(
                "/api/content/shopping",
                serde_json::json!({ "rows": rows, "render": true }),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        harness.tick(std::time::Duration::ZERO).await;

        let (status, body) = harness.post_raw("/api/print/printer", Vec::new()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let height = body["height_px"]
            .as_u64()
            .expect("height_px should be a number");
        let raster = bridge.raster();
        assert_eq!(
            raster.len(),
            height as usize * ROW_BYTES,
            "the raster is whole rows of the printhead's width"
        );
        assert!(
            raster[raster.len() - ROW_BYTES..]
                .iter()
                .any(|&byte| byte != 0),
            "the trim must leave the last row inked: a {count}-item ticket that ends \
             in blank paper is paper spent on nothing"
        );
        printed.push(height);
    }

    let (short, long) = (printed[0], printed[1]);
    // Seven more items, and each of them costs a row of paper: at least the line box
    // it is set in and at most its pitch, plus the few pixels the cell's own header
    // gap grows by once its cell is taller.
    let grown = long - short;
    assert!(
        (7 * 24..=8 * 32).contains(&grown),
        "seven more items must cost seven rows of paper: {short} rows against {long} \
         is {grown}"
    );
    assert!(
        long + 200 < 1200,
        "and the roll must not be what decides it: a ten-item ticket took {long} of \
         the 1200 rows this device may print"
    );
}

#[tokio::test]
async fn printing_is_never_automatic() {
    let bridge = Bridge::start().await;
    let mut harness = Harness::start(&fixture(&bridge.url)).await;

    // Startup rendered every device, and a push plus a tick renders the printer
    // again with changed bytes — the exact moment an auto-printer would fire.
    let (status, _) = harness
        .put(
            "/api/content/ticket",
            serde_json::json!({ "value": "buy milk", "render": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    harness.tick(std::time::Duration::from_secs(1)).await;

    assert!(
        bridge.captured().is_none(),
        "no frame may reach the bridge without a POST to /api/print"
    );
}

#[tokio::test]
async fn a_device_without_a_sink_cannot_print() {
    let bridge = Bridge::start().await;
    let harness = Harness::start(&fixture(&bridge.url)).await;

    let (status, body) = harness.post_raw("/api/print/kindle", Vec::new()).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["printed"], false);
}

#[tokio::test]
async fn an_unknown_device_cannot_print() {
    let bridge = Bridge::start().await;
    let harness = Harness::start(&fixture(&bridge.url)).await;

    let (status, body) = harness.post_raw("/api/print/toaster", Vec::new()).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["printed"], false);
}

#[tokio::test]
async fn an_unreachable_bridge_is_the_bridge_s_fault() {
    // A port that was bound and dropped again: nothing listens there.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let harness = Harness::start(&fixture(&dead_url)).await;

    let (status, body) = harness.post_raw("/api/print/printer", Vec::new()).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["printed"], false);
}

/// A printer that cannot print is not a bridge failure and not a caller mistake,
/// so it is neither a `502` nor a `404`: the frame is fine and the request was
/// right, the paper is the problem. And nothing may be sent — the point of asking
/// first is that the raster never leaves.
#[tokio::test]
async fn a_printer_in_no_state_to_print_is_refused_before_anything_is_sent() {
    for (flag, expected) in [
        ("paper_empty", "out of paper"),
        ("cover_open", "cover is open"),
        ("overheating", "too hot"),
        ("printing", "already printing"),
    ] {
        let bridge = Bridge::start().await;
        let harness = Harness::start(&fixture(&bridge.url)).await;
        bridge.set_flag(flag, true);

        let (status, body) = harness.post_raw("/api/print/printer", Vec::new()).await;

        assert_eq!(status, StatusCode::CONFLICT, "{flag}: {body}");
        assert_eq!(body["printed"], false, "{flag}");
        let error = body["error"].as_str().expect("the error names the reason");
        assert!(
            error.contains(expected),
            "{flag} should be reported as `{expected}`, got `{error}`"
        );
        assert!(
            bridge.captured().is_none(),
            "{flag}: no raster may be posted to a printer that cannot print it"
        );
    }
}

/// A flat battery only refuses while unplugged: on the charger the printhead has
/// the power it needs, and refusing then would be refusing the fix.
#[tokio::test]
async fn a_low_battery_refuses_only_while_unplugged() {
    let bridge = Bridge::start().await;
    let harness = Harness::start(&fixture(&bridge.url)).await;
    bridge.set_flag("low_battery", true);

    let (status, body) = harness.post_raw("/api/print/printer", Vec::new()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(bridge.captured().is_none());

    bridge.set_flag("charging", true);
    let (status, body) = harness.post_raw("/api/print/printer", Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        bridge.captured().is_some(),
        "a charging printer prints despite the warning"
    );
}
