//! The single background task that owns all rendering.
//!
//! Being the only thing in the process that renders removes every concurrency
//! question: two frames for one device can never be produced at once, and the poll
//! handler never contends with it.
//!
//! The loop's decision-making is [`tick`], which takes the instant it is acting at
//! rather than reading a clock. Tests drive `tick` directly, so coalescing,
//! startup rendering and the hash-unchanged path are all asserted without waiting
//! on a wall-clock interval.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;

use crate::app::Runtime;
use crate::schedule::due_devices;

/// Longest the loop will sleep before reconsidering, regardless of how distant the
/// next interval is.
///
/// Equal to the smallest legal `render_interval`, so a configuration reloaded to a
/// shorter interval takes effect immediately rather than after the old sleep
/// expires. Waking this often to compute a due-set costs nothing.
const MAX_SLEEP: Duration = Duration::from_secs(5);

/// When each device was last rendered.
///
/// Held by the loop rather than in shared state: nothing else needs it, and
/// keeping it local is what lets [`tick`] be a plain function of its inputs.
#[derive(Debug, Default)]
pub struct Schedule {
    last_render: HashMap<String, Instant>,
}

impl Schedule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that every configured device was rendered at `at`.
    ///
    /// Called after the startup render so the first interval is measured from then
    /// instead of leaving every device immediately due again.
    pub fn mark_all_rendered(&mut self, runtime: &Runtime, at: Instant) {
        for device in &runtime.config().devices {
            self.last_render.insert(device.id.clone(), at);
        }
    }
}

/// One pass of the render loop.
///
/// Drains the wake channel without blocking, deduplicates the device ids, unions
/// them with any device whose interval has elapsed, and renders each of those
/// exactly once. Draining-and-deduplicating is what stops a burst of pushes
/// causing a render per push.
///
/// `seed` is a device id already taken off the channel by the caller's `select!`,
/// so it is not lost.
///
/// Returns the device ids rendered, in the order they were rendered.
pub async fn tick(
    runtime: &Runtime,
    wake: &mut Receiver<String>,
    schedule: &mut Schedule,
    seed: Option<String>,
    at: Instant,
    now: OffsetDateTime,
) -> Vec<String> {
    let config = runtime.config();
    let mut wanted: Vec<String> = Vec::new();

    let request = |device_id: String, wanted: &mut Vec<String>| {
        if config.devices.iter().any(|device| device.id == device_id)
            && !wanted.contains(&device_id)
        {
            wanted.push(device_id);
        }
    };

    if let Some(device_id) = seed {
        request(device_id, &mut wanted);
    }
    // `try_recv` rather than `recv`: this must never wait for a message that is
    // not already queued.
    while let Ok(device_id) = wake.try_recv() {
        request(device_id, &mut wanted);
    }

    let intervals = config
        .devices
        .iter()
        .map(|device| (device.id.as_str(), device.render_interval));
    for due in due_devices(intervals, &schedule.last_render, at) {
        if !wanted.iter().any(|id| id == due) {
            wanted.push(due.to_owned());
        }
    }

    for device_id in &wanted {
        match runtime.render_device(device_id, now).await {
            Ok(changed) => tracing::info!(
                device = %device_id,
                changed,
                "rendered"
            ),
            Err(error) => tracing::error!(
                device = %device_id,
                error = format!("{error:#}"),
                "render failed; the device keeps serving its previous frame"
            ),
        }
        // Recorded even on failure. Otherwise a device that fails every render
        // stays permanently due and the loop retries it as fast as it can.
        schedule.last_render.insert(device_id.clone(), at);
    }

    wanted
}

/// Runs the render loop until the wake channel closes.
///
/// A single device's failure is logged and the loop continues. Letting a failure
/// end the task would leave the HTTP listener happily serving the last frame it
/// produced forever — stale but plausible content, which is worse than showing
/// nothing because nothing makes it visible. `last_render_at` and `render_count`
/// on the status endpoint exist for exactly that reason.
pub async fn run(runtime: Arc<Runtime>, mut wake: Receiver<String>, mut schedule: Schedule) {
    loop {
        let sleep_for = next_wakeup(&runtime, &schedule, Instant::now());

        let seed = tokio::select! {
            () = tokio::time::sleep(sleep_for) => None,
            received = wake.recv() => match received {
                Some(device_id) => Some(device_id),
                None => {
                    tracing::info!("wake channel closed; render loop stopping");
                    return;
                }
            },
        };

        tick(
            &runtime,
            &mut wake,
            &mut schedule,
            seed,
            Instant::now(),
            OffsetDateTime::now_utc(),
        )
        .await;
    }
}

/// How long until the nearest device is due, capped at [`MAX_SLEEP`].
fn next_wakeup(runtime: &Runtime, schedule: &Schedule, at: Instant) -> Duration {
    runtime
        .config()
        .devices
        .iter()
        .map(|device| match schedule.last_render.get(&device.id) {
            Some(last) => Duration::from_secs(u64::from(device.render_interval))
                .saturating_sub(at.saturating_duration_since(*last)),
            // Never rendered: due now.
            None => Duration::ZERO,
        })
        .min()
        .unwrap_or(MAX_SLEEP)
        .min(MAX_SLEEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    /// A path in the temp directory, unique per test, so no test writes a store
    /// into the working directory or reads another test's.
    fn temp_path(label: &str) -> String {
        std::env::temp_dir()
            .join(format!(
                "paneld-renderer-{label}-{}-{:?}.json",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned()
    }

    fn runtime(toml: &str) -> (Arc<Runtime>, Receiver<String>) {
        let mut parsed = config::parse(toml).expect("fixture config should be valid");
        parsed.server.state_path = temp_path("state");
        Runtime::with_home_assistant(parsed, None).expect("runtime should build")
    }

    const TWO_DEVICES: &str = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 200
height = 150
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

[[device]]
id = "kitchen"
width = 200
height = 150
palette = "mono"
dither = "bayer"
refresh_rate = 600
render_interval = 600
grid = { cols = 1, rows = 1 }

[[device.widget]]
id = "shared"
kind = "text"
col = 0
row = 0
"#;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[tokio::test]
    async fn a_tick_with_nothing_due_renders_nothing() {
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let at = Instant::now();
        schedule.mark_all_rendered(&runtime, at);

        let rendered = tick(&runtime, &mut wake, &mut schedule, None, at, now()).await;
        assert!(rendered.is_empty());
        assert!(runtime.status.snapshot().is_empty());
    }

    #[tokio::test]
    async fn a_device_never_rendered_is_rendered_on_the_first_tick() {
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();

        let rendered = tick(
            &runtime,
            &mut wake,
            &mut schedule,
            None,
            Instant::now(),
            now(),
        )
        .await;
        assert_eq!(rendered, ["kindle", "kitchen"]);
        for device in ["kindle", "kitchen"] {
            assert_eq!(runtime.status.snapshot()[device].render_count, 1);
        }
    }

    #[tokio::test]
    async fn devices_become_due_independently() {
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let start = Instant::now();
        schedule.mark_all_rendered(&runtime, start);

        // 300s in: only the 300s device.
        let rendered = tick(
            &runtime,
            &mut wake,
            &mut schedule,
            None,
            start + Duration::from_secs(300),
            now(),
        )
        .await;
        assert_eq!(rendered, ["kindle"]);

        // 600s in: the 600s device, and the first one again.
        let rendered = tick(
            &runtime,
            &mut wake,
            &mut schedule,
            None,
            start + Duration::from_secs(600),
            now(),
        )
        .await;
        assert_eq!(rendered, ["kindle", "kitchen"]);
    }

    #[tokio::test]
    async fn a_burst_of_wakes_collapses_into_one_render_per_device() {
        // The reason the channel is drained and deduplicated rather than handled
        // one message at a time: a chatty publisher must not be able to spin the
        // renderer.
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let at = Instant::now();
        schedule.mark_all_rendered(&runtime, at);

        for _ in 0..25 {
            runtime.request_render("kindle");
        }
        let rendered = tick(&runtime, &mut wake, &mut schedule, None, at, now()).await;
        assert_eq!(rendered, ["kindle"], "25 wakes must produce one render");
        assert_eq!(runtime.status.snapshot()["kindle"].render_count, 1);
    }

    #[tokio::test]
    async fn a_wake_for_an_unconfigured_device_renders_nothing() {
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let at = Instant::now();
        schedule.mark_all_rendered(&runtime, at);

        runtime.request_render("nonexistent");
        let rendered = tick(&runtime, &mut wake, &mut schedule, None, at, now()).await;
        assert!(rendered.is_empty());
    }

    #[tokio::test]
    async fn a_seed_taken_off_the_channel_is_not_lost() {
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let at = Instant::now();
        schedule.mark_all_rendered(&runtime, at);

        let rendered = tick(
            &runtime,
            &mut wake,
            &mut schedule,
            Some("kitchen".to_owned()),
            at,
            now(),
        )
        .await;
        assert_eq!(rendered, ["kitchen"]);
    }

    #[tokio::test]
    async fn a_render_that_changes_nothing_still_counts() {
        // `render_count` is the observable that makes the hash-unchanged path
        // assertable from outside.
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let at = Instant::now();

        tick(&runtime, &mut wake, &mut schedule, None, at, now()).await;
        let first = runtime.frames.current("kindle").unwrap().hash;

        runtime.request_render("kindle");
        tick(&runtime, &mut wake, &mut schedule, None, at, now()).await;

        assert_eq!(runtime.status.snapshot()["kindle"].render_count, 2);
        assert_eq!(
            runtime.frames.current("kindle").unwrap().hash,
            first,
            "identical content must leave the served frame alone"
        );
    }

    #[tokio::test]
    async fn a_failing_device_does_not_stay_permanently_due() {
        // A device whose render fails must have its attempt recorded, or the loop
        // retries it as fast as it can forever.
        let (runtime, mut wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let at = Instant::now();
        schedule.mark_all_rendered(&runtime, at);

        // A width of zero cannot be encoded, so the render fails inside the
        // pipeline rather than being rejected earlier.
        let mut broken = (*runtime.config()).clone();
        broken.devices[0].width = 0;
        runtime.replace_config(broken);

        runtime.request_render("kindle");
        let rendered = tick(&runtime, &mut wake, &mut schedule, None, at, now()).await;
        assert_eq!(rendered, ["kindle"], "a failing device is still attempted");
        assert!(
            runtime.frames.current("kindle").is_none(),
            "a failed render must not publish a frame"
        );

        // Immediately afterwards it is no longer due.
        let rendered = tick(&runtime, &mut wake, &mut schedule, None, at, now()).await;
        assert!(rendered.is_empty());
    }

    #[test]
    fn the_loop_never_sleeps_past_the_smallest_legal_interval() {
        let (runtime, _wake) = runtime(TWO_DEVICES);
        let mut schedule = Schedule::new();
        let at = Instant::now();

        // Nothing rendered yet: due now.
        assert_eq!(next_wakeup(&runtime, &schedule, at), Duration::ZERO);

        // Freshly rendered, with intervals of 300s and 600s: still capped, so a
        // reload that shortens an interval is picked up promptly.
        schedule.mark_all_rendered(&runtime, at);
        assert_eq!(next_wakeup(&runtime, &schedule, at), MAX_SLEEP);
    }

    #[test]
    fn with_no_devices_the_loop_still_sleeps_a_bounded_time() {
        let (runtime, _wake) = runtime(
            r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"
"#,
        );
        assert_eq!(
            next_wakeup(&runtime, &Schedule::new(), Instant::now()),
            MAX_SLEEP
        );
    }
}
