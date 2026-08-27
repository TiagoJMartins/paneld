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

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::post;
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

/// Binds a stub bridge on an ephemeral loopback port and serves
/// `POST /print/raster`, capturing what arrives.
async fn stub_bridge() -> (String, Captured) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback port");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let captured: Captured = Arc::new(Mutex::new(None));

    async fn accept(
        State(captured): State<Captured>,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        let content_type = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        *captured.lock() = Some((
            uri.query().unwrap_or("").to_owned(),
            content_type,
            body.to_vec(),
        ));
        StatusCode::OK
    }

    let router = Router::new()
        .route("/print/raster", post(accept))
        .with_state(Arc::clone(&captured));
    tokio::spawn(axum::serve(listener, router).into_future());
    (url, captured)
}

#[tokio::test]
async fn a_manual_print_delivers_the_served_frame_as_a_packed_raster() {
    let (bridge_url, captured) = stub_bridge().await;
    let harness = Harness::start(&fixture(&bridge_url)).await;

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

    let (query, content_type, raster) = captured
        .lock()
        .clone()
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
    let (bridge_url, captured) = stub_bridge().await;
    let mut harness = Harness::start(&ticket_fixture(&bridge_url)).await;

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
        let raster = captured
            .lock()
            .clone()
            .expect("the bridge should have been posted to")
            .2;
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
    let (bridge_url, captured) = stub_bridge().await;
    let mut harness = Harness::start(&fixture(&bridge_url)).await;

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
        captured.lock().is_none(),
        "no frame may reach the bridge without a POST to /api/print"
    );
}

#[tokio::test]
async fn a_device_without_a_sink_cannot_print() {
    let (bridge_url, _) = stub_bridge().await;
    let harness = Harness::start(&fixture(&bridge_url)).await;

    let (status, body) = harness.post_raw("/api/print/kindle", Vec::new()).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["printed"], false);
}

#[tokio::test]
async fn an_unknown_device_cannot_print() {
    let (bridge_url, _) = stub_bridge().await;
    let harness = Harness::start(&fixture(&bridge_url)).await;

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
