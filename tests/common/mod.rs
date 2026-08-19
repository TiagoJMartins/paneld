//! Shared harness for the HTTP-boundary tests.
//!
//! Builds the router over a config fixture and drives it in-process, without
//! binding a port. The render loop is not spawned: [`Harness::tick`] runs one pass
//! of the real loop body, so a render is provoked deterministically instead of
//! waiting on an interval.

// Each test binary compiles this harness separately and uses only the part it
// needs, so helpers unused by one of them are expected rather than dead code.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use paneld::app::Runtime;
use paneld::config;
use paneld::renderer::{self, Schedule};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use tower::ServiceExt;

/// A Kindle-shaped fixture: one device, one pushed widget, one Home Assistant
/// widget left unconfigured so it renders as unavailable.
pub const ONE_DEVICE: &str = r#"
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
grid = { cols = 2, rows = 2 }

[[device.widget]]
id = "slack_unread"
kind = "beacon"
col = 0
row = 0
label = "Slack"

[[device.widget]]
id = "office_temp"
kind = "value"
col = 1
row = 0
label = "Office"
unit = "C"
"#;

/// Two devices sharing a widget id, for cadence and fan-out cases.
pub const TWO_DEVICES: &str = r#"
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

[[device.widget]]
id = "shared"
kind = "value"
col = 0
row = 0
label = "Shared"

[[device]]
id = "kitchen"
width = 240
height = 160
palette = "mono"
dither = "bayer"
refresh_rate = 900
render_interval = 900
grid = { cols = 1, rows = 1 }

[[device.widget]]
id = "shared"
kind = "text"
col = 0
row = 0
label = "Shared"
"#;

pub struct Harness {
    pub runtime: Arc<Runtime>,
    wake: Receiver<String>,
    schedule: Schedule,
    /// The loop's notion of the monotonic present, advanced only by [`Self::tick`]
    /// so that nothing here depends on wall-clock timing.
    at: Instant,
    content_path: PathBuf,
}

impl Harness {
    /// Builds the runtime from a fixture and renders every device once, exactly as
    /// startup does before the listener accepts.
    ///
    /// Starts from an empty content store: the path is per-test, and any file left
    /// by an earlier run of the same test is removed.
    pub async fn start(toml: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "paneld-test-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        Self::start_at(toml, path).await
    }

    /// Starts again against the same content file, without clearing it — what a
    /// redeploy looks like to the store.
    pub async fn restart(&self, toml: &str) -> Self {
        Self::start_at(toml, self.content_path.clone()).await
    }

    async fn start_at(toml: &str, content_path: PathBuf) -> Self {
        let mut parsed = config::parse(toml).expect("fixture config should be valid");
        parsed.server.content_path = content_path.to_string_lossy().into_owned();

        let (runtime, wake) =
            Runtime::with_home_assistant(parsed, None).expect("runtime should build");

        let at = Instant::now();
        runtime.render_all(OffsetDateTime::now_utc()).await;
        let mut schedule = Schedule::new();
        schedule.mark_all_rendered(&runtime, at);

        Self {
            runtime,
            wake,
            schedule,
            at,
            content_path,
        }
    }

    pub fn router(&self) -> Router {
        paneld::http::router(Arc::clone(&self.runtime))
    }

    /// Runs one pass of the render loop, `elapsed` after the previous one.
    ///
    /// Returns the device ids rendered.
    pub async fn tick(&mut self, elapsed: Duration) -> Vec<String> {
        self.at += elapsed;
        renderer::tick(
            &self.runtime,
            &mut self.wake,
            &mut self.schedule,
            None,
            self.at,
            OffsetDateTime::now_utc(),
        )
        .await
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Vec<u8>) {
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
            .to_bytes()
            .to_vec();
        (status, body)
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.get_with(uri, &[]).await
    }

    pub async fn get_with(&self, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
        let (status, body) = self.get_bytes_with(uri, headers).await;
        (status, parse_json(&body, uri))
    }

    pub async fn get_bytes(&self, uri: &str) -> (StatusCode, Vec<u8>) {
        self.get_bytes_with(uri, &[]).await
    }

    pub async fn get_bytes_with(
        &self,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder().method("GET").uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    pub async fn put(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let (status, response) = self
            .send(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await;
        (status, parse_json(&response, uri))
    }

    /// A `PUT` whose body is raw text, for malformed-body cases.
    pub async fn put_raw(&self, uri: &str, body: &'static str) -> StatusCode {
        self.send(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .0
    }

    pub async fn post_raw(&self, uri: &str, body: Vec<u8>) -> (StatusCode, Value) {
        let (status, response) = self
            .send(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
        (status, parse_json(&response, uri))
    }

    /// The display poll body for a device, asserted to be HTTP 200.
    pub async fn poll(&self, device: &str) -> Value {
        self.poll_with(device, &[]).await
    }

    pub async fn poll_with(&self, device: &str, headers: &[(&str, &str)]) -> Value {
        let (status, body) = self
            .get_with(&format!("/d/{device}/api/display"), headers)
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the display poll must always answer 200, even for errors"
        );
        body
    }

    pub async fn filename(&self, device: &str) -> String {
        self.poll(device).await["filename"]
            .as_str()
            .expect("filename should be a string")
            .to_owned()
    }

    pub async fn status(&self) -> Value {
        let (status, body) = self.get("/api/status").await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    pub async fn render_count(&self, device: &str) -> u64 {
        self.status().await[device]["render_count"]
            .as_u64()
            .unwrap_or_else(|| panic!("no render_count for device `{device}`"))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.content_path);
    }
}

/// Reads width and height straight out of the PNG IHDR chunk.
///
/// Done by hand because integration tests only see the crate's public API and its
/// dev-dependencies, and a decoder is not worth adding one for.
pub fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert!(is_png(bytes), "not a PNG");
    let read = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    (read(16), read(20))
}

fn parse_json(body: &[u8], uri: &str) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|error| {
        panic!(
            "{uri} should answer JSON, got {error}: {}",
            String::from_utf8_lossy(body)
        )
    })
}

/// Whether bytes begin with the PNG signature.
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}
