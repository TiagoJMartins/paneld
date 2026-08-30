//! The HTTP surface.
//!
//! Two different error conventions live here, deliberately.
//!
//! The device endpoints under `/d/{device}/` **always answer HTTP 200**. The
//! firmware accepts only 200, 301 and 429; a well-formed 401, 404 or 500 is
//! treated as a transport failure and the device backs off. Error conditions are
//! signalled in the body's `status` field, never in the HTTP status line.
//!
//! The operator endpoints under `/api/` use ordinary HTTP status codes, because no
//! firmware reads them and a `404` for "nothing stored" is the clearest answer a
//! script can get.
//!
//! A device poll is a pure read of the frame store. It never renders, so poll
//! latency is flat and independent of how expensive a dashboard is.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::app::Runtime;
use crate::config::{Config, Device, REFRESH_RATE_BOUNDS};
use crate::content::{ContentBody, PutError};
use crate::frame::Frame;
use crate::render::frame_hash;
use crate::tap::{Outcome, Report};
use crate::telemetry::Telemetry;

/// Longest device log line kept, in bytes.
///
/// The endpoint exists only because one client family stops polling if it 404s, so
/// the body is logged and discarded. Capped so a misbehaving client cannot fill
/// the disk.
const MAX_LOG_BYTES: usize = 2_048;

/// Longest tap request body accepted, in bytes.
///
/// Three numbers and a short id. Capped because this endpoint is reachable by
/// anything on the network and the body is buffered before it is parsed.
const MAX_TAP_BYTES: usize = 1_024;

/// Builds the router.
///
/// The slash-collapsing middleware wraps the routes from the outside rather than
/// being layered onto them. `Router::layer` runs *after* routing has already
/// decided, which is too late to rewrite a path so that it matches: a doubled
/// slash would 404 before the rewrite ever ran. An outer router with no routes of
/// its own sends everything to its layered fallback, so the rewrite happens first
/// and the inner router routes on the corrected path.
pub fn router(runtime: Arc<Runtime>) -> Router {
    Router::new()
        .fallback_service(routes(runtime))
        .layer(middleware::from_fn(collapse_slashes))
}

fn routes(runtime: Arc<Runtime>) -> Router {
    Router::new()
        .route("/d/{device}/api/display", get(display))
        .route("/d/{device}/api/setup", get(setup))
        .route("/d/{device}/api/tap", post(tap))
        .route("/d/{device}/api/log", post(device_log))
        .route("/d/{device}/frames/{file}", get(frame_file))
        .route("/d/{device}/current.png", get(current_frame))
        .route(
            "/api/content/{widget_id}",
            put(put_content).get(get_content),
        )
        .route("/api/status", get(status))
        .route("/api/battery", get(battery))
        .route("/api/print/{device}", post(print))
        .with_state(runtime)
}

/// Collapses repeated slashes in the request path before routing.
///
/// A base URL with a trailing slash is the most likely cause of a silently blank
/// panel: both client families build their request URL by plain string
/// concatenation without normalising, so a configured
/// `http://host:4444/d/kindle/` yields `.../d/kindle//api/display`. Rewriting here
/// rather than registering a doubled-slash variant of every route keeps one
/// definition per endpoint and covers the frame URLs too.
async fn collapse_slashes(mut request: Request, next: Next) -> Response {
    if request.uri().path().contains("//") {
        let mut collapsed = String::with_capacity(request.uri().path().len());
        let mut last_was_slash = false;
        for character in request.uri().path().chars() {
            if character == '/' && last_was_slash {
                continue;
            }
            last_was_slash = character == '/';
            collapsed.push(character);
        }
        if let Some(query) = request.uri().query() {
            collapsed.push('?');
            collapsed.push_str(query);
        }

        let mut parts = request.uri().clone().into_parts();
        // A rewrite that cannot be reassembled is left alone rather than turned
        // into an error: the original path still routes, or 404s honestly.
        if let Ok(path_and_query) = collapsed.parse() {
            parts.path_and_query = Some(path_and_query);
            if let Ok(uri) = Uri::from_parts(parts) {
                *request.uri_mut() = uri;
            }
        }
    }
    next.run(request).await
}

/// The display poll response.
#[derive(Debug, Serialize)]
struct DisplayResponse {
    /// `0` means success — not `200`. A body status of `200` falls through the
    /// firmware's switch and the device does nothing at all.
    status: u8,
    image_url: String,
    /// The device's cache key: not the URL, and not the bytes. An unchanged
    /// filename means it repaints from its own flash without downloading.
    filename: String,
    /// Seconds. Always positive: a missing or zero value becomes a deep-sleep
    /// timer of zero, the device wakes instantly, and the battery is flat within
    /// hours.
    refresh_rate: u32,
    // The tail is safe constants forever, hardcoded rather than computed.
    // `reset_firmware: true` is a factory reset that wipes the device's
    // credentials and WiFi configuration.
    update_firmware: bool,
    firmware_url: Option<String>,
    reset_firmware: bool,
    special_function: &'static str,
}

impl DisplayResponse {
    fn serving(base_url: &str, device_id: &str, frame: &Frame, refresh_rate: u32) -> Self {
        Self {
            status: 0,
            image_url: frame_url(base_url, device_id, &frame.hash),
            filename: format!("{}.png", frame.hash),
            refresh_rate: clamp_refresh_rate(refresh_rate),
            update_firmware: false,
            firmware_url: None,
            reset_firmware: false,
            special_function: "none",
        }
    }

    /// The response when no frame can be produced at all.
    ///
    /// Deliberately still `status: 0` with a real `refresh_rate`: the device tries
    /// to fetch an image that is not there, the fetch fails, and it keeps showing
    /// the last frame it successfully downloaded. Signalling an error status the
    /// firmware may not handle risks worse than a failed fetch.
    fn unavailable(refresh_rate: u32) -> Self {
        Self {
            status: 0,
            image_url: String::new(),
            filename: String::new(),
            refresh_rate: clamp_refresh_rate(refresh_rate),
            update_firmware: false,
            firmware_url: None,
            reset_firmware: false,
            special_function: "none",
        }
    }
}

/// Clamps to the protocol's safe range.
///
/// Config validation already rejects an out-of-range value; this is the belt to
/// that braces, because a zero here flattens a battery.
fn clamp_refresh_rate(seconds: u32) -> u32 {
    seconds.clamp(*REFRESH_RATE_BOUNDS.start(), *REFRESH_RATE_BOUNDS.end())
}

/// Frame URLs live under the device's own prefix.
///
/// One client family attaches its auth headers only when the image URL
/// string-prefixes its configured base URL, so keeping frames under the prefix
/// keeps that consistent.
fn frame_url(base_url: &str, device_id: &str, hash: &str) -> String {
    format!("{base_url}/d/{device_id}/frames/{hash}.png")
}

/// `GET /d/{device}/api/display` — the poll. A pure read of the frame store; it
/// never renders.
///
/// `touch_x` / `touch_y` / `touch_event_id` may ride along on the query string, for
/// a client that can only decorate the request it already makes. They are the one
/// thing here with a side effect, and they still cannot change what this answers.
async fn display(
    State(runtime): State<Arc<Runtime>>,
    Path(device_id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let telemetry = Telemetry::from_headers(&headers);
    let config = runtime.config();

    let Some(device) = find_device(&config, &device_id) else {
        // Telemetry is not recorded for an unconfigured id: the key is
        // client-supplied, so doing so would let a typo storm grow the status map
        // without bound.
        tracing::warn!(
            device = %device_id,
            "poll for an unconfigured device id; serving the placeholder frame"
        );
        return placeholder_response(&runtime, &config, &device_id, &telemetry);
    };

    runtime.record_battery(&device.id, &telemetry, now);
    runtime.status.record_poll(&device.id, telemetry, now);

    // Dispatched before the frame is read, so a client that can only decorate its
    // poll gets the effect of its tap on the same wake rather than the one after.
    // Never allowed to change the answer: whatever came of the tap, the response
    // below is the one this device would have received anyway.
    if let Some(touch) = Touch::from_query(uri.query()) {
        let report = runtime
            .tap(&device.id, touch.x, touch.y, touch.event_id.as_deref())
            .await;
        rebuild_after(&runtime, &device.id, &report);
        tracing::info!(
            device = %device.id,
            outcome = %report.outcome,
            widget = report.widget.as_deref().unwrap_or("-"),
            "tap carried on a display poll"
        );
    }

    let Some(frame) = runtime.frames.current(&device.id) else {
        // Every configured device is rendered before the listener accepts, so
        // reaching this means that render failed.
        tracing::error!(
            device = %device.id,
            "no frame available for a configured device; the initial render must have failed"
        );
        return (
            StatusCode::OK,
            Json(DisplayResponse::unavailable(device.refresh_rate)),
        )
            .into_response();
    };

    tracing::info!(
        device = %device.id,
        filename = %frame.hash,
        refresh_rate = clamp_refresh_rate(device.refresh_rate),
        "poll served"
    );
    (
        StatusCode::OK,
        Json(DisplayResponse::serving(
            &config.server.public_base_url,
            &device.id,
            &frame,
            device.refresh_rate,
        )),
    )
        .into_response()
}

/// The placeholder poll response, so a mistyped base URL is diagnosable on the
/// panel itself rather than as a silent failure.
fn placeholder_response(
    runtime: &Runtime,
    config: &Config,
    device_id: &str,
    telemetry: &Telemetry,
) -> Response {
    let size = telemetry.width.zip(telemetry.height);
    let refresh_rate = runtime.placeholder_refresh_rate();

    match runtime.placeholder(device_id, size) {
        Ok(frame) => (
            StatusCode::OK,
            Json(DisplayResponse::serving(
                &config.server.public_base_url,
                device_id,
                &frame,
                refresh_rate,
            )),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(
                device = %device_id,
                error = format!("{error:#}"),
                "rendering the placeholder failed"
            );
            (
                StatusCode::OK,
                Json(DisplayResponse::unavailable(refresh_rate)),
            )
                .into_response()
        }
    }
}

/// Touch coordinates a client may decorate its display poll with.
///
/// Parsed by hand out of the raw query rather than with an extractor, because an
/// extractor's rejection is an HTTP 4xx and this poll must answer 200 whatever a
/// client appends to its URL. Nothing is percent-decoded: two numbers and an opaque
/// id have no reason to be escaped, and a decoder here would be a second parser to
/// keep honest.
struct Touch {
    x: f32,
    y: f32,
    event_id: Option<String>,
}

impl Touch {
    /// `Some` only when both coordinates are present and numeric.
    ///
    /// One coordinate alone is not half a tap, it is a client bug; supplying the
    /// other from thin air would fire an action nobody aimed at.
    fn from_query(query: Option<&str>) -> Option<Self> {
        let mut x = None;
        let mut y = None;
        let mut event_id = None;

        for pair in query?.split('&') {
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            match name {
                "touch_x" => x = value.parse().ok(),
                "touch_y" => y = value.parse().ok(),
                "touch_event_id" => event_id = Some(value.to_owned()),
                _ => {}
            }
        }

        Some(Self {
            x: x?,
            y: y?,
            event_id,
        })
    }
}

/// A tap, as a caller posts it.
///
/// `x0` / `y0` are accepted as aliases because that is tesserae's spelling for the
/// same two numbers, so a client written against it works here unchanged.
#[derive(Debug, Deserialize)]
struct TapBody {
    #[serde(alias = "x0")]
    x: f32,
    #[serde(alias = "y0")]
    y: f32,
    /// Absent means "never deduplicate", because there is nothing to compare.
    #[serde(default)]
    event_id: Option<String>,
}

/// The tap response.
///
/// Always HTTP 200, like everything under `/d/`. A caller may be firmware that
/// treats any other status as a transport failure and backs off, and a tap landing
/// in a gutter is not worth teaching a panel to sleep longer over.
#[derive(Debug, Serialize)]
struct TapResponse {
    /// `0` means the tap was understood, exactly as the display poll uses it.
    /// Anything else is an error the body then explains, never a status line.
    status: u16,
    outcome: Outcome,
    /// The widget the point landed on, `null` when it landed on nothing.
    widget: Option<String>,
    /// What the outcome was about: the `domain.service` dispatched, or why the
    /// request named nothing at all.
    detail: Option<String>,
}

impl TapResponse {
    fn understood(report: &Report) -> Self {
        Self {
            status: 0,
            outcome: report.outcome,
            widget: report.widget.clone(),
            detail: report.detail.clone(),
        }
    }

    /// A request that never got as far as a point on a dashboard.
    fn refused(status: u16, detail: &str) -> Self {
        Self {
            status,
            outcome: Outcome::NoTarget,
            widget: None,
            detail: Some(detail.to_owned()),
        }
    }
}

/// `POST /d/{device}/api/tap` — resolve a point to a cell and do what it says.
///
/// The body is taken as bytes and parsed here rather than through the `Json`
/// extractor, whose rejection would be a 4xx: this endpoint lives under `/d/` and
/// answers 200 even when it refuses.
async fn tap(
    State(runtime): State<Arc<Runtime>>,
    Path(device_id): Path<String>,
    body: Bytes,
) -> Response {
    if body.len() > MAX_TAP_BYTES {
        return tap_response(TapResponse::refused(
            400,
            "the body is longer than a tap can be",
        ));
    }

    let tap: TapBody = match serde_json::from_slice(&body) {
        Ok(tap) => tap,
        Err(error) => {
            tracing::warn!(
                device = %device_id,
                error = %error,
                "tap body could not be read"
            );
            return tap_response(TapResponse::refused(
                400,
                r#"expected {"x": <number>, "y": <number>, "event_id": <string, optional>}"#,
            ));
        }
    };

    // Checked here rather than left to the dispatcher so that an unconfigured id
    // reads as the server-side error it is, the way a poll for one does.
    if find_device(&runtime.config(), &device_id).is_none() {
        tracing::warn!(device = %device_id, "tap for an unconfigured device id");
        return tap_response(TapResponse::refused(
            500,
            "no device is configured under that id",
        ));
    }

    let report = runtime
        .tap(&device_id, tap.x, tap.y, tap.event_id.as_deref())
        .await;
    rebuild_after(&runtime, &device_id, &report);

    tracing::info!(
        device = %device_id,
        x = tap.x,
        y = tap.y,
        outcome = %report.outcome,
        widget = report.widget.as_deref().unwrap_or("-"),
        "tap handled"
    );
    tap_response(TapResponse::understood(&report))
}

fn tap_response(body: TapResponse) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

/// Asks the render loop to rebuild the device when a tap changed something.
///
/// Without this a tap reads as having done nothing: the light is on, but the panel
/// goes on showing the frame it had before, until its own interval comes round.
fn rebuild_after(runtime: &Runtime, device_id: &str, report: &Report) {
    if matches!(report.outcome, Outcome::Dispatched | Outcome::Refreshed) {
        runtime.request_render(device_id);
    }
}

/// `GET /d/{device}/api/setup`.
///
/// Called by one client family exactly once, and only after its base URL changes.
/// Note the in-body status of `200` here, unlike the display poll.
#[derive(Debug, Serialize)]
struct SetupResponse {
    status: u16,
    api_key: String,
    friendly_id: String,
    image_url: String,
    message: &'static str,
}

async fn setup(State(runtime): State<Arc<Runtime>>, Path(device_id): Path<String>) -> Response {
    let config = runtime.config();

    // `image_url` is fetched unauthenticated and must be a real PNG or onboarding
    // fails, so it points at whatever frame this device is currently serving.
    let frame = match find_device(&config, &device_id) {
        Some(device) => runtime.frames.current(&device.id),
        None => runtime.placeholder(&device_id, None).ok(),
    };
    let image_url = frame
        .map(|frame| frame_url(&config.server.public_base_url, &device_id, &frame.hash))
        .unwrap_or_default();

    (
        StatusCode::OK,
        Json(SetupResponse {
            status: 200,
            // Any stable string. Neither client validates it, so it is derived
            // from the device id rather than stored: there is no token store and
            // no token comparison anywhere in this server.
            api_key: frame_hash(format!("paneld:{device_id}").as_bytes()),
            friendly_id: device_id,
            image_url,
            message: "ok",
        }),
    )
        .into_response()
}

/// `POST /d/{device}/api/log` — accept any body, log it, discard it.
///
/// Exists purely because one client family stops polling if this 404s. The body is
/// never parsed and never persisted.
async fn device_log(Path(device_id): Path<String>, body: Bytes) -> Response {
    let truncated = &body[..body.len().min(MAX_LOG_BYTES)];
    tracing::debug!(
        device = %device_id,
        bytes = body.len(),
        body = %String::from_utf8_lossy(truncated),
        "device log"
    );
    (StatusCode::OK, Json(json!({ "status": 200 }))).into_response()
}

/// `GET /d/{device}/frames/{file}` — the frame bytes.
async fn frame_file(
    State(runtime): State<Arc<Runtime>>,
    Path((device_id, file)): Path<(String, String)>,
) -> Response {
    let hash = file.strip_suffix(".png").unwrap_or(&file);

    // The retained previous generation is served too, because a device may be
    // mid-download of the frame a new one replaced.
    let frame = runtime
        .frames
        .by_hash(&device_id, hash)
        .or_else(|| runtime.placeholder_by_hash(hash));

    let Some(frame) = frame else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no frame `{hash}` for device `{device_id}`") })),
        )
            .into_response();
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            // The device asks for identity encoding; a proxy that recompresses
            // anyway corrupts the fetch.
            (header::CACHE_CONTROL, "no-transform"),
        ],
        frame.bytes.to_vec(),
    )
        .into_response()
}

/// `GET /d/{device}/current.png` — whatever frame is being served right now.
///
/// A convenience for humans and dashboards: one stable URL that always shows the
/// latest frame, so a browser tab can just be refreshed. The panel must never use
/// it, because a filename that never changes defeats the device's own caching and
/// would make it repaint on every poll — hence `no-store`, which is the opposite
/// of what the content-addressed frame URLs want.
async fn current_frame(
    State(runtime): State<Arc<Runtime>>,
    Path(device_id): Path<String>,
) -> Response {
    let frame = runtime
        .frames
        .current(&device_id)
        .or_else(|| runtime.placeholder(&device_id, None).ok());

    let Some(frame) = frame else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no frame for device `{device_id}`") })),
        )
            .into_response();
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        frame.bytes.to_vec(),
    )
        .into_response()
}

/// `PUT /api/content/{widget_id}` — store a pushed value.
///
/// Returns as soon as the content is stored and any wake message is queued. It
/// never blocks on rendering, and the response does not report whether a render
/// happened: the contract is "the next frame will contain this".
async fn put_content(
    State(runtime): State<Arc<Runtime>>,
    Path(widget_id): Path<String>,
    Json(body): Json<ContentBody>,
) -> Response {
    let wants_render = body.render;
    let now = OffsetDateTime::now_utc();

    let record = match runtime.content.put(&widget_id, body, now) {
        Ok(record) => record,
        Err(error) => {
            let code = match error {
                // Not the client's fault that the server is full.
                PutError::TooManyWidgets { .. } => StatusCode::INSUFFICIENT_STORAGE,
                _ => StatusCode::BAD_REQUEST,
            };
            // JSON on the error path too: this endpoint answers JSON on success,
            // and a script should not have to sniff the content type.
            return (code, Json(json!({ "error": error.to_string() }))).into_response();
        }
    };

    if let Err(error) = runtime.content.persist() {
        // The value is already stored in memory and the frame will contain it, so
        // a failed write is logged rather than failing the push. Only a restart
        // loses it.
        tracing::warn!(
            widget = %widget_id,
            error = format!("{error:#}"),
            "storing content to disk failed; it survives until a restart only"
        );
    }

    if wants_render {
        let devices = devices_using(&runtime.config(), &widget_id);
        if devices.is_empty() {
            // Not an error: publishers are frequently wired up before their widget
            // is laid out.
            tracing::debug!(
                widget = %widget_id,
                "render was requested but no device's dashboard declares this widget"
            );
        }
        for device_id in devices {
            runtime.request_render(&device_id);
        }
    }

    (StatusCode::OK, Json(record)).into_response()
}

/// `GET /api/content/{widget_id}` — read back what is stored.
async fn get_content(
    State(runtime): State<Arc<Runtime>>,
    Path(widget_id): Path<String>,
) -> Response {
    match runtime.content.get(&widget_id) {
        Some(record) => (StatusCode::OK, Json(record)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("nothing stored for widget `{widget_id}`") })),
        )
            .into_response(),
    }
}

/// `GET /api/status` — per-device operational state.
async fn status(State(runtime): State<Arc<Runtime>>) -> Response {
    (StatusCode::OK, Json(runtime.status.snapshot())).into_response()
}

/// `GET /api/battery` — per-device battery history, and the charge rate and ETA
/// read off it.
///
/// A debug surface, deliberately separate from `/api/status`: the reading a
/// dashboard wants is one number, and this is every sample behind it plus the
/// arithmetic, which is what makes a wrong ETA diagnosable rather than merely
/// wrong.
async fn battery(State(runtime): State<Arc<Runtime>>) -> Response {
    (StatusCode::OK, Json(runtime.battery.reports())).into_response()
}

/// `POST /api/print/{device}` — deliver the frame currently being served to the
/// device's printer sink.
///
/// The only way paper ever moves: nothing prints automatically, because paper
/// is not idempotent. The whole preview-then-print flow is to look at
/// `/d/{device}/current.png` (or the same dashboard on any panel), then post
/// here. Replies after the printer acknowledges the job, so a `200` means
/// paper moved.
///
/// An operator endpoint, so ordinary status codes: `404` when the device
/// cannot print (unknown, sinkless or not yet rendered), `409` for a blank
/// frame or a printer in no state to print — out of paper, cover open, hot,
/// busy or flat — and `502` when the bridge refused or could not be reached.
/// The `409` bodies name the reason, because the operator's next move differs:
/// a blank frame wants a publisher, an empty printer wants paper.
async fn print(State(runtime): State<Arc<Runtime>>, Path(device_id): Path<String>) -> Response {
    use crate::app::PrintError;

    match runtime.print_device(&device_id).await {
        Ok(delivery) => (
            StatusCode::OK,
            Json(json!({
                "printed": true,
                "height_px": delivery.height_px,
                "bytes": delivery.bytes,
            })),
        )
            .into_response(),
        Err(error) => {
            let status = match &error {
                PrintError::NoSuchDevice | PrintError::NoSink | PrintError::NoFrame => {
                    StatusCode::NOT_FOUND
                }
                PrintError::Blank | PrintError::NotReady(_) => StatusCode::CONFLICT,
                PrintError::Delivery(_) => StatusCode::BAD_GATEWAY,
            };
            tracing::warn!(device = %device_id, error = %error, "manual print failed");
            (
                status,
                Json(json!({ "printed": false, "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

fn find_device<'a>(config: &'a Config, device_id: &str) -> Option<&'a Device> {
    config.devices.iter().find(|device| device.id == device_id)
}

/// Every device whose dashboard declares `widget_id`.
///
/// Walks a group's children as well as the widgets beside it, and a status bar's
/// alerts as well as its widgets: all three are push addresses, and a push that
/// resolved to no device is a cell — or an alert — that stays as it was until the
/// render interval comes round. For an alert that is the whole feature broken: it
/// exists to appear when it is triggered, not five minutes later.
fn devices_using(config: &Config, widget_id: &str) -> Vec<String> {
    config
        .devices
        .iter()
        .filter(|device| {
            device.all_widgets().any(|widget| widget.id == widget_id)
                || device
                    .status_bar
                    .as_ref()
                    .is_some_and(|bar| bar.alerts.iter().any(|alert| alert.id == widget_id))
        })
        .map(|device| device.id.clone())
        .collect()
}
