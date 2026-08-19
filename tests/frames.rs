//! Frame invalidation: the filename changes only when the rendered bytes change.
//!
//! The whole e-ink refresh story rests on this. The device treats `filename` as its
//! cache key, so a filename that fails to change when the content did leaves a
//! stale panel, and a filename that changes when nothing did burns a refresh and
//! battery on identical output.

mod common;

use axum::http::StatusCode;
use common::{Harness, ONE_DEVICE, TWO_DEVICES, is_png, png_dimensions};
use serde_json::json;
use std::time::Duration;

/// Long enough for a 300-second `render_interval` to have elapsed.
const PAST_THE_INTERVAL: Duration = Duration::from_secs(301);
/// Short enough that no interval has elapsed.
const NO_TIME: Duration = Duration::from_secs(0);

#[tokio::test]
async fn two_consecutive_polls_with_no_render_between_return_the_same_filename() {
    let harness = Harness::start(ONE_DEVICE).await;
    let first = harness.filename("kindle").await;
    let second = harness.filename("kindle").await;
    assert_eq!(first, second);
    assert!(first.ends_with(".png"), "{first}");
}

#[tokio::test]
async fn a_render_with_unchanged_content_leaves_the_filename_alone_but_counts() {
    let mut harness = Harness::start(ONE_DEVICE).await;
    let before = harness.filename("kindle").await;
    let counted_before = harness.render_count("kindle").await;

    assert_eq!(harness.tick(PAST_THE_INTERVAL).await, ["kindle"]);

    assert_eq!(
        harness.filename("kindle").await,
        before,
        "identical content must not move the device's cache key"
    );
    assert_eq!(
        harness.render_count("kindle").await,
        counted_before + 1,
        "a render that produced an unchanged hash still performed a render"
    );
}

#[tokio::test]
async fn a_render_after_a_content_push_returns_a_different_filename() {
    let mut harness = Harness::start(ONE_DEVICE).await;
    let before = harness.filename("kindle").await;

    let (status, _) = harness
        .put("/api/content/office_temp", json!({ "value": 21.4 }))
        .await;
    assert_eq!(status, StatusCode::OK);
    harness.tick(PAST_THE_INTERVAL).await;

    assert_ne!(
        harness.filename("kindle").await,
        before,
        "changed content must move the cache key or the panel never repaints"
    );
}

#[tokio::test]
async fn a_push_without_render_does_not_change_the_filename_until_a_render_happens() {
    let mut harness = Harness::start(ONE_DEVICE).await;
    let before = harness.filename("kindle").await;

    // Both the explicit `false` and the field being absent.
    for body in [
        json!({ "value": 11, "render": false }),
        json!({ "value": 12 }),
    ] {
        let (status, _) = harness.put("/api/content/office_temp", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            harness.filename("kindle").await,
            before,
            "storing content must not by itself change what is served"
        );
    }

    harness.tick(PAST_THE_INTERVAL).await;
    assert_ne!(harness.filename("kindle").await, before);
}

#[tokio::test]
async fn a_push_with_render_reaches_the_glass_without_any_interval_elapsing() {
    let mut harness = Harness::start(ONE_DEVICE).await;
    let before = harness.filename("kindle").await;

    let (status, _) = harness
        .put(
            "/api/content/office_temp",
            json!({ "value": 21.4, "render": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // No time passes: the render happens because the push asked for it.
    assert_eq!(harness.tick(NO_TIME).await, ["kindle"]);
    assert_ne!(harness.filename("kindle").await, before);
}

#[tokio::test]
async fn a_push_with_render_returns_before_the_render_is_observable() {
    // It must never block on rendering: the response arrives, and only afterwards
    // does the render happen.
    let mut harness = Harness::start(ONE_DEVICE).await;
    let counted_before = harness.render_count("kindle").await;

    let (status, body) = harness
        .put(
            "/api/content/office_temp",
            json!({ "value": 99, "render": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["value"], 99,
        "the response is the stored record, not a render report"
    );
    assert!(
        body.get("rendered").is_none() && body.get("render").is_none(),
        "the response must not claim anything about rendering: {body}"
    );
    assert_eq!(
        harness.render_count("kindle").await,
        counted_before,
        "the push returned before any render ran"
    );

    harness.tick(NO_TIME).await;
    assert_eq!(harness.render_count("kindle").await, counted_before + 1);
}

#[tokio::test]
async fn a_push_with_render_for_an_unused_widget_is_accepted_and_changes_nothing() {
    // Publishers are frequently wired up before their widget is laid out, so this
    // is explicitly not an error.
    let mut harness = Harness::start(ONE_DEVICE).await;
    let before = harness.filename("kindle").await;
    let counted_before = harness.render_count("kindle").await;

    let (status, body) = harness
        .put(
            "/api/content/not_on_any_dashboard",
            json!({ "value": "stored anyway", "render": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], "stored anyway");

    assert_eq!(harness.tick(NO_TIME).await, Vec::<String>::new());
    assert_eq!(harness.filename("kindle").await, before);
    assert_eq!(harness.render_count("kindle").await, counted_before);

    let (status, stored) = harness.get("/api/content/not_on_any_dashboard").await;
    assert_eq!(status, StatusCode::OK, "it must still have been stored");
    assert_eq!(stored["value"], "stored anyway");
}

#[tokio::test]
async fn a_burst_of_pushes_collapses_into_one_render_per_device() {
    // A chatty publisher must not be able to spin the renderer.
    let mut harness = Harness::start(TWO_DEVICES).await;
    let counted_before = harness.render_count("kindle").await;

    const PUSHES: u64 = 20;
    for value in 0..PUSHES {
        let (status, _) = harness
            .put(
                "/api/content/shared",
                json!({ "value": value, "render": true }),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    let rendered = harness.tick(NO_TIME).await;
    let renders = harness.render_count("kindle").await - counted_before;
    assert_eq!(
        renders, 1,
        "{PUSHES} pushes produced {renders} renders; the burst must collapse"
    );
    assert!(
        renders < PUSHES,
        "the whole point is fewer renders than pushes"
    );

    // The widget is on both dashboards, so both are rebuilt — once each.
    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert_eq!(harness.render_count("kitchen").await, 1 + 1);
}

#[tokio::test]
async fn a_poll_never_renders() {
    let harness = Harness::start(ONE_DEVICE).await;
    let counted_before = harness.render_count("kindle").await;
    for _ in 0..12 {
        harness.poll("kindle").await;
    }
    assert_eq!(
        harness.render_count("kindle").await,
        counted_before,
        "the poll path must be a pure read of the frame store"
    );
}

#[tokio::test]
async fn every_device_has_a_real_frame_before_the_first_poll() {
    let harness = Harness::start(TWO_DEVICES).await;

    for (device, expected) in [("kindle", (400, 300)), ("kitchen", (240, 160))] {
        assert!(
            harness.render_count(device).await >= 1,
            "{device} should have been rendered at startup"
        );

        let body = harness.poll(device).await;
        let (status, bytes) = harness
            .get_bytes(
                body["image_url"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("http://192.168.0.50:4444"),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            png_dimensions(&bytes),
            expected,
            "{device} must serve its own dashboard, not the unknown-device placeholder"
        );
    }
}

#[tokio::test]
async fn the_frame_url_is_under_the_device_prefix_and_serves_a_bounded_png() {
    let harness = Harness::start(ONE_DEVICE).await;
    let body = harness.poll("kindle").await;

    let image_url = body["image_url"].as_str().unwrap();
    assert!(
        image_url.starts_with("http://192.168.0.50:4444/d/kindle/"),
        "frames must stay under the device's own prefix: {image_url}"
    );
    assert!(
        image_url.ends_with(&format!("/{}", body["filename"].as_str().unwrap())),
        "the filename must match the image URL: {image_url}"
    );

    let path = image_url.trim_start_matches("http://192.168.0.50:4444");
    let (status, bytes) = harness.get_bytes(path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(is_png(&bytes), "the frame must be a PNG");
    assert!(
        bytes.len() < 90_000,
        "the frame is {} bytes, over the device's fetch ceiling",
        bytes.len()
    );
}

#[tokio::test]
async fn exactly_one_previous_generation_stays_fetchable() {
    // A device may be mid-download of the frame a new one replaced; anything older
    // is not worth the memory.
    let mut harness = Harness::start(ONE_DEVICE).await;

    let mut urls = Vec::new();
    for value in 1..=3 {
        harness
            .put(
                "/api/content/office_temp",
                json!({ "value": value * 1_000, "render": true }),
            )
            .await;
        harness.tick(NO_TIME).await;
        let body = harness.poll("kindle").await;
        let url = body["image_url"].as_str().unwrap().to_owned();
        assert!(!urls.contains(&url), "each push should produce a new frame");
        urls.push(url);
    }

    let path = |url: &str| {
        url.trim_start_matches("http://192.168.0.50:4444")
            .to_owned()
    };

    let (status, bytes) = harness.get_bytes(&path(&urls[2])).await;
    assert_eq!(status, StatusCode::OK, "the current frame");
    assert!(is_png(&bytes));

    let (status, bytes) = harness.get_bytes(&path(&urls[1])).await;
    assert_eq!(status, StatusCode::OK, "the previous generation");
    assert!(is_png(&bytes));

    let (status, _) = harness.get_bytes(&path(&urls[0])).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "two generations back must have been dropped"
    );
}

#[tokio::test]
async fn a_push_is_reflected_by_a_subsequent_get() {
    let harness = Harness::start(ONE_DEVICE).await;

    let (status, _) = harness
        .put(
            "/api/content/slack_unread",
            json!({ "value": "on", "state": "alert" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, stored) = harness.get("/api/content/slack_unread").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored["value"], "on");
    assert_eq!(stored["state"], "alert");
    assert!(
        stored["received_at"].is_string(),
        "the server stamps receipt: {stored}"
    );
}

#[tokio::test]
async fn a_push_to_an_unknown_widget_id_is_accepted_and_stored() {
    let harness = Harness::start(ONE_DEVICE).await;
    let (status, _) = harness
        .put("/api/content/wired_up_early", json!({ "value": 1 }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, stored) = harness.get("/api/content/wired_up_early").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored["value"], 1);
}

#[tokio::test]
async fn reading_a_widget_with_nothing_stored_is_a_404() {
    // Ordinary HTTP status codes are correct here: no firmware reads this.
    let harness = Harness::start(ONE_DEVICE).await;
    let (status, _) = harness.get("/api/content/never_pushed").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_second_of_two_pushes_wins_wholesale() {
    let harness = Harness::start(ONE_DEVICE).await;

    harness
        .put(
            "/api/content/office_temp",
            json!({ "value": 1, "state": "first", "unit": "C" }),
        )
        .await;
    let (status, second) = harness
        .put("/api/content/office_temp", json!({ "value": 2 }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["value"], 2);

    let (_, stored) = harness.get("/api/content/office_temp").await;
    assert_eq!(stored["value"], 2);
    assert!(
        stored["state"].is_null() && stored["unit"].is_null(),
        "last write wins unconditionally, with no merging: {stored}"
    );
}

#[tokio::test]
async fn a_malformed_push_is_rejected_without_disturbing_what_is_stored() {
    let harness = Harness::start(ONE_DEVICE).await;
    harness
        .put("/api/content/office_temp", json!({ "value": 7 }))
        .await;

    assert_eq!(
        harness
            .put_raw("/api/content/office_temp", "{not json")
            .await,
        StatusCode::BAD_REQUEST
    );
    // Neither `value` nor `rows`: there is nothing to store.
    let (status, _) = harness.put("/api/content/office_temp", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, stored) = harness.get("/api/content/office_temp").await;
    assert_eq!(stored["value"], 7, "the previous value must survive");
}

#[tokio::test]
async fn an_explicit_null_value_is_accepted() {
    // `value` is required but may legitimately be null.
    let harness = Harness::start(ONE_DEVICE).await;
    let (status, body) = harness
        .put("/api/content/office_temp", json!({ "value": null }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["value"].is_null());
}

#[tokio::test]
async fn a_push_of_several_named_rows_is_stored_and_rendered() {
    let mut harness = Harness::start(ONE_DEVICE).await;
    let before = harness.filename("kindle").await;

    let (status, body) = harness
        .put(
            "/api/content/office_temp",
            json!({
                "rows": [
                    { "id": "in", "label": "Inside", "value": 21.4, "unit": "C" },
                    { "id": "out", "label": "Outside", "value": 8.1, "unit": "C" }
                ],
                "render": true
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"].as_array().map(Vec::len), Some(2));

    harness.tick(NO_TIME).await;
    assert_ne!(harness.filename("kindle").await, before);
}

#[tokio::test]
async fn content_survives_a_restart() {
    // A redeploy must not blank the dashboard until every publisher happens to fire
    // again.
    let harness = Harness::start(ONE_DEVICE).await;
    harness
        .put("/api/content/office_temp", json!({ "value": 21.4 }))
        .await;

    let restarted = harness.restart(ONE_DEVICE).await;
    let (status, stored) = restarted.get("/api/content/office_temp").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the store is loaded from disk at startup"
    );
    assert_eq!(stored["value"], 21.4);
}

#[tokio::test]
async fn the_current_frame_url_is_stable_while_the_content_addressed_one_moves() {
    // A convenience for humans: one URL to refresh. It must track the latest frame
    // rather than a hash, and must not be cached, or a browser shows a stale panel.
    let mut harness = Harness::start(ONE_DEVICE).await;

    let (status, first) = harness.get_bytes("/d/kindle/current.png").await;
    assert_eq!(status, StatusCode::OK);
    assert!(is_png(&first));
    assert_eq!(png_dimensions(&first), (400, 300));

    harness
        .put(
            "/api/content/office_temp",
            json!({ "value": 21.4, "render": true }),
        )
        .await;
    harness.tick(NO_TIME).await;

    let (status, second) = harness.get_bytes("/d/kindle/current.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(
        first, second,
        "the same URL must now serve the newly rendered frame"
    );
}

#[tokio::test]
async fn the_current_frame_url_falls_back_to_the_placeholder_for_an_unknown_device() {
    let harness = Harness::start(ONE_DEVICE).await;
    let (status, bytes) = harness.get_bytes("/d/kindel/current.png").await;
    assert_eq!(status, StatusCode::OK);
    assert!(is_png(&bytes), "a mistyped id should still be diagnosable");
}
