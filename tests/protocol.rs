//! The BYOS wire contract, asserted through the HTTP boundary.
//!
//! Every rule here breaks a real device when violated, and none of them are
//! visible from inside the process: they are properties of the bytes and JSON a
//! panel receives. So this suite only ever looks at a response — status line,
//! body fields, frame bytes — and never at a struct.
//!
//! The recurring hazard is silence. A device that dislikes a response does not
//! report anything; it backs off, or repaints from flash, or deep-sleeps for
//! zero seconds and flattens its battery overnight. There is no error path to
//! observe on the panel, which is why these assertions are exact.

mod common;

use axum::http::StatusCode;
use common::{Harness, ONE_DEVICE, TWO_DEVICES, is_png, png_dimensions};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The `public_base_url` of every fixture below.
///
/// Frame URLs are absolute because the device fetches them directly, so the
/// prefix has to come off before the in-process harness can serve the path.
const BASE_URL: &str = "http://192.168.0.50:4444";

/// The lower end of the protocol's `refresh_rate` range.
const MIN_REFRESH: &str = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "bayer"
refresh_rate = 30
render_interval = 300
grid = { cols = 1, rows = 1 }

[[device.widget]]
id = "office_temp"
kind = "value"
col = 0
row = 0
label = "Office"
"#;

/// The upper end of the protocol's `refresh_rate` range: a day.
const MAX_REFRESH: &str = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "bayer"
refresh_rate = 86400
render_interval = 300
grid = { cols = 1, rows = 1 }

[[device.widget]]
id = "office_temp"
kind = "value"
col = 0
row = 0
label = "Office"
"#;

/// A device id that appears in no fixture, so a poll for it is the
/// mistyped-base-URL case.
const UNKNOWN: &str = "kindl";

/// The path a device would fetch, from the absolute URL a poll handed out.
fn frame_path(body: &Value) -> String {
    let url = body["image_url"]
        .as_str()
        .unwrap_or_else(|| panic!("image_url should be a string: {body}"));
    assert!(
        url.starts_with(BASE_URL),
        "the frame URL must be absolute and on the configured base URL: {url}"
    );
    url[BASE_URL.len()..].to_owned()
}

/// The two invariants that must hold on *every* display response, whichever
/// path produced it.
fn assert_wakes_and_does_nothing_dangerous(body: &Value) {
    let refresh_rate = body["refresh_rate"]
        .as_u64()
        .unwrap_or_else(|| panic!("refresh_rate must serialise as a number: {body}"));
    assert!(
        refresh_rate > 0,
        "a zero refresh_rate is a deep-sleep timer of zero: the device wakes \
         instantly and the battery is flat within hours: {body}"
    );
    assert_eq!(
        body["reset_firmware"],
        json!(false),
        "reset_firmware wipes the device's credentials and WiFi configuration: {body}"
    );
}

// ---------------------------------------------------------------------------
// Rule 1: always answer HTTP 200, even for errors.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_device_endpoint_answers_http_two_hundred() {
    // The firmware accepts only 200, 301 and 429. A well-formed 404 or 500 reads
    // as a transport failure and the device backs off, so there is no HTTP status
    // available to signal anything with.
    let harness = Harness::start(ONE_DEVICE).await;

    for uri in [
        "/d/kindle/api/display",
        &format!("/d/{UNKNOWN}/api/display"),
        "/d/kindle/api/setup",
        &format!("/d/{UNKNOWN}/api/setup"),
    ] {
        let (status, _) = harness.get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri} answered {status}");
    }

    for uri in ["/d/kindle/api/log", &format!("/d/{UNKNOWN}/api/log")] {
        let (status, _) = harness.post_raw(uri, b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::OK, "{uri} answered {status}");
    }
}

#[tokio::test]
async fn a_poll_for_an_unconfigured_id_still_parses_as_json_carrying_status_zero() {
    // An unknown id is a configuration mistake, not a protocol error: the device
    // must still be told to carry on polling, or the mistake is undiagnosable.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll(UNKNOWN).await;

    assert_eq!(body["status"], json!(0), "{body}");
    assert!(
        body["status"].is_number(),
        "an unknown id must not turn the status into a string: {body}"
    );
    assert_wakes_and_does_nothing_dangerous(&body);
}

// ---------------------------------------------------------------------------
// Rule 2: `status: 0` means success. Not 200.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_display_status_is_the_integer_zero_never_two_hundred() {
    // A body status of 200 falls through the firmware's switch and the device does
    // nothing at all — no fetch, no repaint, no error.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll("kindle").await;

    assert_eq!(body["status"], json!(0), "{body}");
    assert!(
        body["status"].is_number(),
        "a quoted \"0\" is not the integer 0 to the firmware's parser: {body}"
    );
    assert_ne!(body["status"], json!(200), "{body}");
    assert_ne!(body["status"], json!("0"), "{body}");
}

// ---------------------------------------------------------------------------
// Rule 3: `refresh_rate` is always a positive integer, in seconds.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_configured_refresh_rate_comes_back_exactly_at_both_ends_of_the_valid_range() {
    // In-range values pass through untouched: an operator who asks for a 30-second
    // cadence and silently gets 300 cannot tell from the panel.
    for (fixture, expected) in [(MIN_REFRESH, 30), (MAX_REFRESH, 86_400)] {
        let harness = Harness::start(fixture).await;
        let body = harness.poll("kindle").await;
        assert_eq!(body["refresh_rate"], json!(expected), "{body}");
    }
}

#[tokio::test]
async fn every_display_response_carries_a_positive_numeric_refresh_rate() {
    // Including the placeholder path, which is reached precisely when something is
    // already misconfigured and is therefore the easiest place to send a zero.
    let harness = Harness::start(TWO_DEVICES).await;

    for device in ["kindle", "kitchen", UNKNOWN] {
        let body = harness.poll(device).await;
        assert_wakes_and_does_nothing_dangerous(&body);
    }

    // And with device-reported dimensions in play, which changes which frame the
    // placeholder branch produces.
    let body = harness
        .poll_with(UNKNOWN, &[("png-width", "600"), ("png-height", "448")])
        .await;
    assert_wakes_and_does_nothing_dangerous(&body);
}

// ---------------------------------------------------------------------------
// Rule 4: `filename` is the device's cache key.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_filename_is_a_content_addressed_png_name_that_ends_the_image_url() {
    // The device caches on the filename alone, so it must be derived from the
    // frame bytes and it must agree with the URL it is served from.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll("kindle").await;

    let filename = body["filename"]
        .as_str()
        .unwrap_or_else(|| panic!("filename should be a string: {body}"));
    assert!(!filename.is_empty(), "{body}");
    assert!(filename.ends_with(".png"), "{filename}");

    let image_url = body["image_url"].as_str().unwrap();
    assert!(
        image_url.ends_with(&format!("/{filename}")),
        "the URL the device fetches must end in the key it caches under: {image_url}"
    );

    let stem = filename.trim_end_matches(".png");
    assert_eq!(
        stem.len(),
        32,
        "the stem is a SHA-256 truncated to 16 bytes; the client folds long names \
         to the first 7 plus last 17 characters, so a longer stem risks collision: {stem}"
    );
    assert!(
        stem.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "the stem must be lowercase hex: {stem}"
    );
}

// ---------------------------------------------------------------------------
// Rule 5: the tail fields are safe constants forever.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_response_tail_is_exactly_the_safe_constants() {
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll("kindle").await;

    assert_eq!(body["update_firmware"], json!(false), "{body}");
    assert!(
        body["firmware_url"].is_null(),
        "firmware_url must be null, not an empty string: {body}"
    );
    assert_eq!(body["reset_firmware"], json!(false), "{body}");
    assert_eq!(body["special_function"], json!("none"), "{body}");
}

#[tokio::test]
async fn reset_firmware_is_never_true_on_any_response_the_suite_can_provoke() {
    // A factory reset wipes the device's credentials and WiFi configuration: it is
    // the one field in this protocol that cannot be undone from the server.
    for fixture in [ONE_DEVICE, TWO_DEVICES, MIN_REFRESH, MAX_REFRESH] {
        let harness = Harness::start(fixture).await;
        for device in ["kindle", UNKNOWN] {
            let body = harness.poll(device).await;
            assert_eq!(body["reset_firmware"], json!(false), "{device}: {body}");
            assert_ne!(body["reset_firmware"], json!(true), "{device}: {body}");
        }
    }
}

// ---------------------------------------------------------------------------
// Device identity is the URL path, not a token.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_doubled_slash_in_the_display_path_returns_an_identical_body() {
    // Both client families concatenate a configured base URL with the endpoint
    // path without normalising, so a base URL with a trailing slash produces this
    // exact request. It is the most likely cause of a silently blank panel.
    let harness = Harness::start(ONE_DEVICE).await;

    let (plain_status, plain) = harness.get("/d/kindle/api/display").await;
    let (doubled_status, doubled) = harness.get("/d/kindle//api/display").await;

    assert_eq!(plain_status, StatusCode::OK);
    assert_eq!(doubled_status, StatusCode::OK);
    assert_eq!(
        doubled, plain,
        "a trailing slash in the base URL must not change a single field"
    );
}

#[tokio::test]
async fn a_doubled_slash_in_the_frames_path_serves_identical_bytes() {
    // The frame URL is built by the same concatenation, so the doubled segment
    // reaches the fetch as well as the poll.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll("kindle").await;
    let path = frame_path(&body);

    let (plain_status, plain) = harness.get_bytes(&path).await;
    let doubled_path = path.replacen("/frames/", "//frames/", 1);
    let (doubled_status, doubled) = harness.get_bytes(&doubled_path).await;

    assert_eq!(plain_status, StatusCode::OK);
    assert_eq!(doubled_status, StatusCode::OK, "{doubled_path}");
    assert!(is_png(&plain));
    assert_eq!(doubled, plain, "{doubled_path} served different bytes");
}

#[tokio::test]
async fn a_doubled_slash_in_the_status_path_returns_an_identical_body() {
    // Same normalisation, on the operator surface, so a script built from a
    // trailing-slash base URL behaves too.
    let harness = Harness::start(ONE_DEVICE).await;

    let (plain_status, plain) = harness.get("/api/status").await;
    let (doubled_status, doubled) = harness.get("/api//status").await;

    assert_eq!(plain_status, StatusCode::OK);
    assert_eq!(doubled_status, StatusCode::OK);
    assert_eq!(doubled, plain);
}

#[tokio::test]
async fn the_frame_url_stays_under_the_polled_device_prefix() {
    // One client family attaches its headers only when the image URL
    // string-prefixes its configured base URL, which is the device's own prefix.
    let harness = Harness::start(TWO_DEVICES).await;

    for device in ["kindle", "kitchen"] {
        let body = harness.poll(device).await;
        let image_url = body["image_url"].as_str().unwrap();
        assert!(
            image_url.starts_with(&format!("{BASE_URL}/d/{device}/")),
            "{device} was handed a frame outside its own prefix: {image_url}"
        );
    }
}

#[tokio::test]
async fn a_nonsense_access_token_and_no_token_at_all_succeed_identically() {
    // There is no token store and no token comparison anywhere in this server:
    // identity is the path. A device whose token was never provisioned must work.
    let harness = Harness::start(ONE_DEVICE).await;

    let with_nonsense = harness
        .poll_with("kindle", &[("Access-Token", "not-a-real-token-at-all")])
        .await;
    let without = harness.poll("kindle").await;

    assert_eq!(with_nonsense["status"], json!(0));
    assert_eq!(
        with_nonsense, without,
        "an unrecognised token must not change the response in any way"
    );
}

// ---------------------------------------------------------------------------
// Setup.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn setup_answers_the_onboarding_constants_with_a_fetchable_png() {
    let harness = Harness::start(ONE_DEVICE).await;
    let (status, body) = harness.get("/d/kindle/api/setup").await;
    assert_eq!(status, StatusCode::OK);

    // Note the in-body 200 here, unlike the display poll's 0.
    assert_eq!(body["status"], json!(200), "{body}");
    assert_eq!(body["friendly_id"], json!("kindle"), "{body}");
    assert_eq!(body["message"], json!("ok"), "{body}");
    assert!(
        !body["api_key"]
            .as_str()
            .expect("api_key should be a string")
            .is_empty(),
        "an empty api_key is stored by the client and never questioned again: {body}"
    );

    // Onboarding fails outright if these bytes are not a real PNG.
    let (frame_status, bytes) = harness.get_bytes(&frame_path(&body)).await;
    assert_eq!(frame_status, StatusCode::OK);
    assert!(is_png(&bytes), "setup's image_url must serve a real PNG");
}

#[tokio::test]
async fn the_setup_api_key_is_stable_across_calls() {
    // The client stores the key and sends it back forever. There is no token store
    // here, so the value has to be derived rather than generated.
    let harness = Harness::start(ONE_DEVICE).await;

    let (_, first) = harness.get("/d/kindle/api/setup").await;
    let (_, second) = harness.get("/d/kindle/api/setup").await;

    assert_eq!(first["api_key"], second["api_key"]);
    assert_ne!(
        first["api_key"],
        harness.get("/d/kitchen/api/setup").await.1["api_key"],
        "the key is derived from the device id, so two ids must not share one"
    );
}

// ---------------------------------------------------------------------------
// Log.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_log_endpoint_answers_status_two_hundred_for_any_body() {
    // It exists only because one client family stops polling if it 404s. The body
    // is never parsed, so nothing about its shape may affect the answer.
    let harness = Harness::start(ONE_DEVICE).await;

    let bodies: Vec<(&str, Vec<u8>)> = vec![
        (
            "well-formed",
            json!({ "log": { "logs_array": [{ "log_message": "boot" }] } })
                .to_string()
                .into_bytes(),
        ),
        ("malformed", b"{\"log\": not json at all".to_vec()),
        ("empty", Vec::new()),
        // Well past the stored line cap, to prove the cap truncates rather than
        // rejects.
        ("64 KiB", vec![b'x'; 64 * 1024]),
    ];

    for (label, body) in bodies {
        let (status, response) = harness.post_raw("/d/kindle/api/log", body).await;
        assert_eq!(status, StatusCode::OK, "{label} body answered {status}");
        assert_eq!(response["status"], json!(200), "{label} body: {response}");
    }
}

// ---------------------------------------------------------------------------
// Telemetry through the HTTP boundary.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telemetry_header_lookup_is_case_insensitive() {
    // The two client families disagree on capitalisation as well as on spelling.
    let harness = Harness::start(ONE_DEVICE).await;
    harness
        .poll_with("kindle", &[("PERCENT-CHARGED", "64")])
        .await;

    let status = harness.status().await;
    assert_eq!(
        status["kindle"]["telemetry"]["battery_percent"],
        json!(64.0),
        "{status}"
    );
}

#[tokio::test]
async fn both_battery_percent_spellings_are_accepted() {
    for (header, value) in [("percent-charged", "41"), ("battery-percent", "88")] {
        let harness = Harness::start(ONE_DEVICE).await;
        harness.poll_with("kindle", &[(header, value)]).await;

        let status = harness.status().await;
        assert_eq!(
            status["kindle"]["telemetry"]["battery_percent"],
            json!(value.parse::<f64>().unwrap()),
            "{header} was not read: {status}"
        );
    }
}

#[tokio::test]
async fn both_dimension_spellings_are_accepted() {
    for (width_header, height_header) in [("png-width", "png-height"), ("width", "height")] {
        let harness = Harness::start(ONE_DEVICE).await;
        harness
            .poll_with("kindle", &[(width_header, "600"), (height_header, "448")])
            .await;

        let status = harness.status().await;
        let telemetry = &status["kindle"]["telemetry"];
        assert_eq!(
            telemetry["width"],
            json!(600),
            "{width_header} was not read: {status}"
        );
        assert_eq!(
            telemetry["height"],
            json!(448),
            "{height_header} was not read: {status}"
        );
    }
}

#[tokio::test]
async fn an_integer_battery_voltage_stays_millivolts_and_a_decimal_one_is_scaled() {
    // Some firmware reports integer millivolts and some decimal volts, with
    // nothing in the header to tell them apart. A real lithium cell never reads
    // below 100 mV nor above about 5 V, which is what makes the heuristic safe.
    for reported in ["3700", "3.7"] {
        let harness = Harness::start(ONE_DEVICE).await;
        harness
            .poll_with("kindle", &[("battery-voltage", reported)])
            .await;

        let status = harness.status().await;
        assert_eq!(
            status["kindle"]["telemetry"]["battery_millivolts"],
            json!(3700.0),
            "battery-voltage: {reported} landed wrong: {status}"
        );
    }
}

#[tokio::test]
async fn a_poll_missing_a_header_does_not_erase_the_previously_reported_value() {
    // The client in service is header-light: it sends an integer battery percent
    // and nothing else. One such poll must not blank the device's whole record.
    let harness = Harness::start(ONE_DEVICE).await;
    harness
        .poll_with(
            "kindle",
            &[("percent-charged", "72"), ("battery-voltage", "3900")],
        )
        .await;
    harness
        .poll_with("kindle", &[("percent-charged", "70")])
        .await;

    let status = harness.status().await;
    let telemetry = &status["kindle"]["telemetry"];
    assert_eq!(
        telemetry["battery_percent"],
        json!(70.0),
        "the newer reading must win where it is present: {status}"
    );
    assert_eq!(
        telemetry["battery_millivolts"],
        json!(3900.0),
        "the voltage was reported once and must survive a poll that omits it: {status}"
    );
}

#[tokio::test]
async fn the_battery_endpoint_reports_the_history_behind_the_reading() {
    // Every level the device reported, not just the last one: this is the
    // surface a wrong rate is diagnosed from, so the samples have to be here.
    let harness = Harness::start(ONE_DEVICE).await;
    for percent in ["74", "74", "73"] {
        harness
            .poll_with("kindle", &[("percent-charged", percent)])
            .await;
    }

    let (status, battery) = harness.get("/api/battery").await;
    assert_eq!(status, StatusCode::OK);

    let kindle = &battery["kindle"];
    assert_eq!(kindle["percent"], json!(73.0), "{battery}");
    assert!(kindle["reported_at"].is_string(), "{battery}");

    let readings = kindle["readings"].as_array().expect("readings is an array");
    assert_eq!(
        readings.len(),
        2,
        "the repeated 74 extends one reading rather than adding another: {battery}"
    );
    assert_eq!(readings[0]["percent"], json!(74.0));
    assert_eq!(readings[0]["polls"], json!(2));
    assert_eq!(readings[1]["percent"], json!(73.0));
}

#[tokio::test]
async fn the_battery_endpoint_withholds_a_rate_it_cannot_measure_yet() {
    // One crossing gives a direction and nothing more. A number here instead of
    // a null would be quantisation noise dressed up as an estimate.
    let harness = Harness::start(ONE_DEVICE).await;
    for percent in ["74", "73"] {
        harness
            .poll_with("kindle", &[("percent-charged", percent)])
            .await;
    }

    let trend = &harness.get("/api/battery").await.1["kindle"]["trend"];
    assert_eq!(trend["direction"], json!("discharging"), "{trend}");
    assert_eq!(trend["steps"], json!(1), "{trend}");
    assert!(trend["percent_per_hour"].is_null(), "{trend}");
    assert!(trend["eta_at"].is_null(), "{trend}");
    assert!(trend["eta_seconds"].is_null(), "{trend}");
}

#[tokio::test]
async fn a_reported_charging_state_reaches_the_battery_endpoint() {
    let harness = Harness::start(ONE_DEVICE).await;
    harness
        .poll_with(
            "kindle",
            &[
                ("percent-charged", "80"),
                ("battery-charging", "1"),
                ("usb-connected", "true"),
            ],
        )
        .await;

    let battery = harness.get("/api/battery").await.1;
    let kindle = &battery["kindle"];
    assert_eq!(kindle["power"]["charging"], json!(true), "{battery}");
    assert_eq!(kindle["power"]["usb_connected"], json!(true), "{battery}");
    assert_eq!(
        kindle["trend"]["direction"],
        json!("charging"),
        "the device said so, before any level has moved: {battery}"
    );
}

#[tokio::test]
async fn a_device_that_reported_no_battery_level_is_absent_rather_than_empty() {
    // The history is persisted, so a device that never reports a level must not
    // earn an entry in the file. `/api/status` is where a poll from it shows up.
    let harness = Harness::start(ONE_DEVICE).await;
    harness.poll("kindle").await;

    let battery = harness.get("/api/battery").await.1;
    assert_eq!(battery, json!({}), "{battery}");

    let status = harness.status().await;
    assert!(status["kindle"]["last_poll_at"].is_string(), "{status}");
}

#[tokio::test]
async fn the_battery_history_survives_a_restart() {
    // The whole reason it is on disk: a redeploy must not cost the samples the
    // rate is measured from, or every deploy blinds the estimate for hours.
    let harness = Harness::start(ONE_DEVICE).await;
    for percent in ["74", "73"] {
        harness
            .poll_with("kindle", &[("percent-charged", percent)])
            .await;
    }

    let restarted = harness.restart(ONE_DEVICE).await;
    let battery = restarted.get("/api/battery").await.1;
    let kindle = &battery["kindle"];
    assert_eq!(kindle["percent"], json!(73.0), "{battery}");
    assert_eq!(
        kindle["readings"].as_array().map(Vec::len),
        Some(2),
        "both levels came back: {battery}"
    );
    assert_eq!(
        kindle["trend"]["direction"],
        json!("discharging"),
        "{battery}"
    );
}

#[tokio::test]
async fn the_status_endpoint_does_not_carry_the_battery_history() {
    // `/api/status` is a small response an operator polls; the history grows with
    // every level change and lives on its own endpoint.
    let harness = Harness::start(ONE_DEVICE).await;
    harness
        .poll_with("kindle", &[("percent-charged", "74")])
        .await;

    let status = harness.status().await;
    assert!(status["kindle"]["battery"].is_null(), "{status}");
    assert_eq!(
        status["kindle"]["telemetry"]["battery_percent"],
        json!(74.0),
        "the reading itself is still there: {status}"
    );
}

#[tokio::test]
async fn a_poll_records_last_poll_at_as_an_rfc_3339_timestamp() {
    // The only way an operator can see a panel that has stopped polling.
    let harness = Harness::start(ONE_DEVICE).await;
    assert!(
        harness.status().await["kindle"]["last_poll_at"].is_null(),
        "a device that has not polled must not claim a poll time"
    );

    harness.poll("kindle").await;

    let status = harness.status().await;
    let stamp = status["kindle"]["last_poll_at"]
        .as_str()
        .unwrap_or_else(|| panic!("last_poll_at should be a string: {status}"));
    OffsetDateTime::parse(stamp, &Rfc3339)
        .unwrap_or_else(|error| panic!("last_poll_at `{stamp}` is not RFC 3339: {error}"));
}

// ---------------------------------------------------------------------------
// The unknown-device placeholder.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_poll_for_an_unconfigured_id_serves_a_fetchable_placeholder_png() {
    // A mistyped base URL is otherwise invisible: the panel just keeps showing
    // whatever it last downloaded. The placeholder makes it diagnosable on glass.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll(UNKNOWN).await;

    assert_eq!(body["status"], json!(0), "{body}");
    let filename = body["filename"].as_str().unwrap();
    assert!(filename.ends_with(".png"), "{filename}");

    let (status, bytes) = harness.get_bytes(&frame_path(&body)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(is_png(&bytes), "the placeholder must be a real PNG");
}

#[tokio::test]
async fn the_placeholder_takes_its_size_from_the_reported_dimension_headers() {
    // The device's own panel size is the only size that renders legibly on it.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness
        .poll_with(UNKNOWN, &[("png-width", "600"), ("png-height", "448")])
        .await;

    let (status, bytes) = harness.get_bytes(&frame_path(&body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(png_dimensions(&bytes), (600, 448));
}

#[tokio::test]
async fn the_placeholder_falls_back_to_a_default_size_when_no_dimensions_are_reported() {
    // The client in service reports no dimensions at all, so the fallback is the
    // common case rather than the exception.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll(UNKNOWN).await;

    let (status, bytes) = harness.get_bytes(&frame_path(&body)).await;
    assert_eq!(status, StatusCode::OK);
    let (width, height) = png_dimensions(&bytes);
    assert_eq!(
        (width, height),
        (800, 480),
        "a device that reports nothing must still get a panel-shaped frame"
    );
}

#[tokio::test]
async fn absurdly_reported_placeholder_dimensions_are_clamped() {
    // The dimensions come from device-supplied headers, so a malformed poll must
    // not be able to ask the server to allocate an enormous buffer.
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness
        .poll_with(
            UNKNOWN,
            &[("png-width", "999999"), ("png-height", "999999")],
        )
        .await;

    let (status, bytes) = harness.get_bytes(&frame_path(&body)).await;
    assert_eq!(status, StatusCode::OK);
    let (width, height) = png_dimensions(&bytes);
    assert!(
        width <= 4096 && height <= 4096,
        "a poll asked for 999999x999999 and was served {width}x{height}"
    );
}

#[tokio::test]
async fn two_unconfigured_ids_produce_different_placeholder_frames() {
    // The frame names the id that was requested. Without that it is decoration,
    // and the owner learns nothing from looking at the panel.
    let harness = Harness::start(ONE_DEVICE).await;

    let first = harness.poll("kindl").await;
    let second = harness.poll("kindle-2").await;

    let (_, first_bytes) = harness.get_bytes(&frame_path(&first)).await;
    let (_, second_bytes) = harness.get_bytes(&frame_path(&second)).await;

    assert_ne!(
        first["filename"], second["filename"],
        "two different unknown ids shared a cache key"
    );
    assert_ne!(
        first_bytes, second_bytes,
        "the placeholder must name the requested id to be diagnostic"
    );
}

#[tokio::test]
async fn an_unconfigured_id_never_enters_the_status_map() {
    // The key is client-supplied, so recording it would let a typo storm grow the
    // map without bound.
    let harness = Harness::start(ONE_DEVICE).await;
    for id in ["kindl", "kindle-2", "KINDLE", "x"] {
        harness.poll_with(id, &[("percent-charged", "50")]).await;
    }

    let status = harness.status().await;
    let keys: Vec<&str> = status
        .as_object()
        .expect("status should be a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["kindle"], "{status}");
}
