//! Home Assistant: entity states for `ha_entity` widgets, and service calls for
//! taps.
//!
//! [`HaClient`] is the seam. Everything above this module talks to the trait, so
//! a test supplies canned answers without a network and without a mocking
//! framework. It is deliberately narrow — one reading in, one string out; one
//! resolved call in, nothing out — because every decision about *what* to read or
//! call is config's, resolved long before it gets here.
//!
//! [`fetch_states`] is the only entry point the renderer uses. It cannot fail as
//! a whole: a per-entity failure comes back as `Err(message)` so one unreachable
//! integration degrades a single cell instead of blanking the dashboard.

use std::collections::HashMap;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};

use crate::config::ServiceCall;

/// How long a single Home Assistant request may take.
///
/// A bound, not a tuning knob: the render loop is a single task, so a hang here
/// wedges every device's frame, not just the widget that is waiting.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest response body echoed back inside an error message, in characters.
const BODY_SNIPPET_CHARS: usize = 200;

/// Reads one entity's state from Home Assistant.
/// What to read from Home Assistant.
///
/// An attribute rather than the entity's own state is a common need, not an edge
/// case: a `weather.*` entity's state is a condition like `partlycloudy`, and the
/// temperature worth putting on a panel is an attribute of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reading {
    pub entity_id: String,
    pub attribute: Option<String>,
}

impl Reading {
    pub fn state(entity_id: impl Into<String>) -> Self {
        Self {
            entity_id: entity_id.into(),
            attribute: None,
        }
    }

    pub fn attribute(entity_id: impl Into<String>, attribute: impl Into<String>) -> Self {
        Self {
            entity_id: entity_id.into(),
            attribute: Some(attribute.into()),
        }
    }
}

impl std::fmt::Display for Reading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.attribute {
            Some(attribute) => write!(f, "{}#{attribute}", self.entity_id),
            None => f.write_str(&self.entity_id),
        }
    }
}

/// What a reading currently shows, and how much the panel should trust it.
///
/// The distinction exists because "the request failed" and "there is no value"
/// are different facts that used to render the same. A temperature that read
/// 21.4 five minutes ago is still the best answer this panel has; replacing it
/// with the word `unavailable` throws away information a reader wants and makes
/// a momentary timeout look like a dead sensor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reported {
    /// Read successfully just now.
    Fresh(String),
    /// The last value read successfully, kept because the newest request did not
    /// confirm it. Rendered muted, and its cell carries a mark saying so.
    Held(String),
    /// No value has ever been read, and the newest request did not produce one.
    Lost,
}

/// The last value successfully read for each reading.
///
/// Lives across renders, which is the whole point: a frame is built from a
/// single round of fetches, so without somewhere to remember yesterday's answer
/// a failed request has nothing to fall back on.
///
/// Bounded by the configuration rather than by a cap. Every fold prunes to the
/// readings that round asked about, so a dashboard that stops referencing an
/// entity stops paying for it, and a reload cannot make this grow without limit.
#[derive(Debug, Default)]
pub struct LastGood {
    values: HashMap<Reading, String>,
}

impl LastGood {
    /// Folds one round of results against what was last known good.
    ///
    /// A sentinel state counts as a failure, not as a value: Home Assistant
    /// reports a dropped-out entity as the literal string `unavailable`, and that
    /// is a successful HTTP request carrying no reading. Treating it as a value
    /// would put a word where a number goes; treating it as a failure keeps the
    /// last real number on the glass, which is what a dropped-out sensor calls
    /// for.
    pub fn fold(
        &mut self,
        results: HashMap<Reading, Result<String, String>>,
    ) -> HashMap<Reading, Reported> {
        let mut reported = HashMap::with_capacity(results.len());
        for (reading, result) in results {
            let value = match result {
                Ok(value) if !is_sentinel(&value) => {
                    self.values.insert(reading.clone(), value.clone());
                    Reported::Fresh(value)
                }
                _ => match self.values.get(&reading) {
                    Some(held) => Reported::Held(held.clone()),
                    None => Reported::Lost,
                },
            };
            reported.insert(reading, value);
        }
        self.values
            .retain(|reading, _| reported.contains_key(reading));
        reported
    }
}

/// Whether a Home Assistant state means "no reading" rather than a value.
///
/// These are the strings Home Assistant itself uses for an entity that is
/// missing, has dropped off its radio, or has not reported since a restart.
pub fn is_sentinel(state: &str) -> bool {
    let state = state.trim();
    state.is_empty()
        || state.eq_ignore_ascii_case("unavailable")
        || state.eq_ignore_ascii_case("unknown")
        || state.eq_ignore_ascii_case("none")
}

#[async_trait::async_trait]
pub trait HaClient: Send + Sync {
    /// Returns the entity's current state as Home Assistant reports it, e.g.
    /// `"21.4"`, `"on"` or `"unavailable"`.
    async fn read(&self, reading: &Reading) -> Result<String>;

    /// Calls a Home Assistant service, e.g. `POST /api/services/light/toggle`.
    ///
    /// The whole body is the caller's `data`, already carrying `entity_id` when
    /// there is a target, so nothing is decided here.
    async fn call(&self, call: &ServiceCall) -> Result<()>;
}

/// A [`HaClient`] speaking to a real Home Assistant over HTTP.
///
/// `Debug` prints the token as `Sensitive` rather than its value, because the
/// renderer logs the structs that hold this client.
#[derive(Debug)]
pub struct HttpHaClient {
    /// Built once and reused: a client per request would open a fresh
    /// connection pool each time and leak sockets under a fast render interval.
    http: reqwest::Client,
    /// Stored without a trailing slash, so request paths are plain
    /// concatenation.
    base_url: String,
}

impl HttpHaClient {
    /// Builds the shared HTTP client, carrying the token as a default header.
    ///
    /// Fails only if the configured token cannot be a header value.
    pub fn new(config: &crate::config::HomeAssistant) -> Result<Self> {
        let token = resolve_token(config)?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}")).context(
            "the home_assistant.token / token_env value is not usable as an HTTP header value",
        )?;
        auth.set_sensitive(true);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .default_headers(headers)
            .build()
            .context("building the Home Assistant HTTP client")?;

        Ok(Self {
            http,
            base_url: config.base_url.clone(),
        })
    }

    /// The request, with every failure reported as its own message. `state`
    /// wraps this in the context naming the entity.
    async fn fetch_reading(&self, reading: &Reading) -> Result<String> {
        let entity_id = reading.entity_id.as_str();
        validate_entity_id(entity_id)?;

        let url = format!("{}/api/states/{entity_id}", self.base_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        let status = response.status();
        // Read the body before asserting the status: Home Assistant explains a
        // 401 or a 404 in the body, and that explanation is what the operator
        // needs in the log.
        let body = response
            .text()
            .await
            .with_context(|| format!("reading the response body of GET {url}"))?;

        ensure!(
            status.is_success(),
            "Home Assistant returned HTTP {status}: {}",
            snippet(&body)
        );

        let json: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| format!("response body is not JSON: {}", snippet(&body)))?;
        let Some(attribute) = &reading.attribute else {
            let Some(state) = json.get("state").and_then(serde_json::Value::as_str) else {
                bail!(
                    "response JSON has no string `state` field: {}",
                    snippet(&body)
                );
            };
            return Ok(state.to_owned());
        };

        let Some(value) = json.get("attributes").and_then(|a| a.get(attribute)) else {
            bail!(
                "entity has no attribute `{attribute}`; it has {}",
                available_attributes(&json)
            );
        };
        // Numbers and booleans are as legitimate on a panel as strings, and a
        // temperature arrives as a number.
        Ok(match value {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Null => bail!("attribute `{attribute}` is null"),
            other => other.to_string(),
        })
    }

    /// The request, with every failure reported as its own message. `call` wraps
    /// this in the context naming the service.
    async fn post_service(&self, call: &ServiceCall) -> Result<()> {
        // Config validated both of these, but this is the boundary that builds a
        // URL out of them, and a boundary that trusts its caller is one bug away
        // from posting to a path nobody wrote.
        validate_segment(&call.domain, "service domain")?;
        validate_segment(&call.service, "service name")?;

        let url = format!(
            "{}/api/services/{}/{}",
            self.base_url, call.domain, call.service
        );
        // Serialised here rather than through reqwest's `json` feature, matching how
        // `fetch_reading` decodes: this crate owns its JSON, and the transport only
        // carries bytes.
        let payload = serde_json::to_vec(&call.data)
            .with_context(|| format!("serialising the service data for POST {url}"))?;

        // No `return_response`: Home Assistant answers 400 to an actuator asked to
        // return a payload, and nothing here reads one.
        let response = self
            .http
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = response.status();
        // Read the body before asserting the status: Home Assistant explains why it
        // refused a call in the body, and that explanation is the whole value of the
        // log line an operator will read.
        let body = response
            .text()
            .await
            .with_context(|| format!("reading the response body of POST {url}"))?;

        ensure!(
            status.is_success(),
            "Home Assistant returned HTTP {status}: {}",
            snippet(&body)
        );

        Ok(())
    }
}

#[async_trait::async_trait]
impl HaClient for HttpHaClient {
    async fn read(&self, reading: &Reading) -> Result<String> {
        self.fetch_reading(reading)
            .await
            .with_context(|| format!("Home Assistant reading `{reading}`"))
    }

    async fn call(&self, call: &ServiceCall) -> Result<()> {
        self.post_service(call)
            .await
            .with_context(|| format!("Home Assistant service call `{call}`"))
    }
}

/// The token, either written in the config or read from the environment.
///
/// Config validation guarantees exactly one source is configured. Reading the
/// environment happens here rather than at parse time so that parsing stays a
/// pure function of the text, and so a token never has to be written into a
/// ConfigMap to be mounted.
fn resolve_token(config: &crate::config::HomeAssistant) -> Result<String> {
    if let Some(token) = &config.token {
        return Ok(token.clone());
    }
    let name = config
        .token_env
        .as_deref()
        .context("home_assistant has neither `token` nor `token_env`")?;
    let token = std::env::var(name).with_context(|| {
        format!("home_assistant.token_env names environment variable `{name}`, which is not set")
    })?;
    ensure!(
        !token.trim().is_empty(),
        "home_assistant.token_env names environment variable `{name}`, which is set but empty"
    );
    Ok(token)
}

/// The attribute names an entity does have, for an error message that tells the
/// author what to write instead of only what was wrong.
fn available_attributes(json: &serde_json::Value) -> String {
    match json
        .get("attributes")
        .and_then(serde_json::Value::as_object)
    {
        Some(attributes) if !attributes.is_empty() => {
            let mut names: Vec<&str> = attributes.keys().map(String::as_str).collect();
            names.sort_unstable();
            names.join(", ")
        }
        _ => "none".to_owned(),
    }
}

/// Fetches every referenced entity, one result per distinct entity.
///
/// Never fails as a whole: a per-entity failure is captured as `Err(message)`,
/// the formatted error chain, so the renderer can draw that one cell as
/// unavailable and still produce a frame.
///
/// Fetches concurrently. The render loop is a single task, so N sequential
/// requests against an unreachable Home Assistant would stall it for
/// N * [`REQUEST_TIMEOUT`].
pub async fn fetch_readings(
    client: &dyn HaClient,
    readings: &[Reading],
) -> HashMap<Reading, Result<String, String>> {
    let distinct = distinct_readings(readings);
    let results = join_all(distinct.iter().map(|r| client.read(r)).collect()).await;

    distinct
        .into_iter()
        .zip(results)
        .map(|(reading, result)| {
            let value = result.map_err(|err| {
                let message = format!("{err:#}");
                tracing::warn!(reading = %reading, error = %message, "Home Assistant fetch failed");
                message
            });
            (reading.clone(), value)
        })
        .collect()
}

/// Input order, one entry per distinct reading.
///
/// Linear membership scan rather than a `HashSet`: a reading list is one entry
/// per grid cell, so this is a handful of comparisons against an allocation.
fn distinct_readings(readings: &[Reading]) -> Vec<&Reading> {
    let mut distinct: Vec<&Reading> = Vec::with_capacity(readings.len());
    for reading in readings {
        if !distinct.contains(&reading) {
            distinct.push(reading);
        }
    }
    distinct
}

/// Rejects an entity id that could forge a different request path.
///
/// Nothing is percent-encoded: an entity id is `domain.object_id`, which is
/// URL-safe. The characters that are not are refused outright rather than
/// escaped, because a `/` in an id is a config mistake, not a value to carry.
fn validate_entity_id(entity_id: &str) -> Result<()> {
    ensure!(!entity_id.is_empty(), "entity id is empty");

    if let Some(bad) = entity_id.chars().find(path_forging) {
        bail!(
            "entity id `{entity_id}` contains {bad:?}, which would change the \
             request path; expected a plain `domain.object_id`"
        );
    }

    Ok(())
}

/// Rejects a service domain or name that could forge a different request path.
///
/// Config already refused anything but lower-case letters, digits and
/// underscores, so this can only fire on a call built in code. It exists anyway
/// because this is the function that concatenates a URL, and the day someone adds
/// a second way to build a [`ServiceCall`] is the day that stops being true.
fn validate_segment(value: &str, what: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{what} is empty");

    if let Some(bad) = value.chars().find(path_forging) {
        bail!(
            "{what} `{value}` contains {bad:?}, which would change the request \
             path; expected a plain Home Assistant identifier"
        );
    }

    Ok(())
}

/// Characters that would change a request path if they reached a URL unescaped.
///
/// Refused outright rather than percent-encoded: an entity id and a service name
/// are both already URL-safe, so one of these is a mistake to report, never a
/// value to carry.
fn path_forging(c: &char) -> bool {
    matches!(c, '/' | '?' | '#') || c.is_whitespace()
}

/// A body prefix short enough to log, at a character boundary.
fn snippet(body: &str) -> &str {
    let trimmed = body.trim();
    match trimmed.char_indices().nth(BODY_SNIPPET_CHARS) {
        Some((end, _)) => &trimmed[..end],
        None => trimmed,
    }
}

/// Runs every future to completion concurrently, results in input order.
///
/// Hand-rolled for two reasons: `futures` is not a dependency, and
/// `tokio::task::JoinSet` needs `'static` futures, which a request borrowing a
/// `&dyn HaClient` can never be. Requiring `Unpin` keeps this allocation-free —
/// the futures handed in are already the boxed futures `#[async_trait]`
/// produces.
///
/// Every unfinished future is polled on every wake. That is quadratic in the
/// number of futures, which is fine at one entity per grid cell and is why this
/// is private rather than a general-purpose combinator.
async fn join_all<F>(futures: Vec<F>) -> Vec<F::Output>
where
    F: Future + Unpin,
{
    let mut pending: Vec<Option<F>> = futures.into_iter().map(Some).collect();
    let mut results: Vec<Option<F::Output>> = (0..pending.len()).map(|_| None).collect();
    let mut remaining = pending.len();

    poll_fn(move |cx| {
        for (slot, result) in pending.iter_mut().zip(results.iter_mut()) {
            let Some(future) = slot.as_mut() else {
                continue;
            };
            if let Poll::Ready(value) = Pin::new(future).poll(cx) {
                *result = Some(value);
                // Dropped so it is never polled after completion.
                *slot = None;
                remaining -= 1;
            }
        }

        if remaining > 0 {
            return Poll::Pending;
        }

        Poll::Ready(
            results
                .iter_mut()
                .map(|result| result.take().expect("every future has completed"))
                .collect(),
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::HomeAssistant;

    /// One canned answer plus how many times that entity has been asked for.
    struct Canned {
        answer: Result<String, String>,
        calls: AtomicUsize,
    }

    /// Canned answers, read counts and every service call posted, so a test can
    /// assert what was fetched, how often, and what was actuated, without a
    /// network.
    struct StubHaClient {
        answers: HashMap<String, Canned>,
        total_calls: AtomicUsize,
        /// Every call, in the order it was made. A `Mutex` rather than a counter
        /// because a tap's whole payload is the thing worth asserting on.
        services: Mutex<Vec<ServiceCall>>,
    }

    impl StubHaClient {
        fn new(answers: &[(&str, Result<&str, &str>)]) -> Self {
            let answers = answers
                .iter()
                .map(|(id, answer)| {
                    let answer = match answer {
                        Ok(state) => Ok((*state).to_owned()),
                        Err(message) => Err((*message).to_owned()),
                    };
                    (
                        (*id).to_owned(),
                        Canned {
                            answer,
                            calls: AtomicUsize::new(0),
                        },
                    )
                })
                .collect();
            Self {
                answers,
                total_calls: AtomicUsize::new(0),
                services: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self, entity_id: &str) -> usize {
            self.answers
                .get(entity_id)
                .map_or(0, |canned| canned.calls.load(Ordering::Relaxed))
        }

        fn total_calls(&self) -> usize {
            self.total_calls.load(Ordering::Relaxed)
        }

        fn services(&self) -> Vec<ServiceCall> {
            self.services.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HaClient for StubHaClient {
        async fn read(&self, reading: &Reading) -> Result<String> {
            self.total_calls.fetch_add(1, Ordering::Relaxed);
            let entity_id = reading.to_string();
            let entity_id = entity_id.as_str();

            let Some(canned) = self.answers.get(entity_id) else {
                bail!("stub has no answer for `{entity_id}`");
            };
            canned.calls.fetch_add(1, Ordering::Relaxed);

            match &canned.answer {
                Ok(state) => Ok(state.clone()),
                Err(message) => bail!("{message}"),
            }
        }

        async fn call(&self, call: &ServiceCall) -> Result<()> {
            self.services.lock().unwrap().push(call.clone());
            Ok(())
        }
    }

    fn readings(ids: &[&str]) -> Vec<Reading> {
        ids.iter().map(|id| Reading::state(*id)).collect()
    }

    fn config() -> HomeAssistant {
        HomeAssistant {
            base_url: "http://homeassistant.local:8123".to_owned(),
            token: Some("tok".to_owned()),
            token_env: None,
        }
    }

    #[tokio::test]
    async fn returns_one_entry_per_distinct_entity_fetching_a_repeat_once() {
        let client = StubHaClient::new(&[
            ("sensor.office_temp", Ok("21.4")),
            ("binary_sensor.door", Ok("on")),
        ]);

        let states = fetch_readings(
            &client,
            &readings(&[
                "sensor.office_temp",
                "binary_sensor.door",
                "sensor.office_temp",
            ]),
        )
        .await;

        assert_eq!(states.len(), 2);
        assert_eq!(
            states[&Reading::state("sensor.office_temp")],
            Ok("21.4".to_owned())
        );
        assert_eq!(
            states[&Reading::state("binary_sensor.door")],
            Ok("on".to_owned())
        );
        assert_eq!(client.call_count("sensor.office_temp"), 1);
        assert_eq!(client.total_calls(), 2);
    }

    #[tokio::test]
    async fn a_failing_entity_degrades_only_itself() {
        let client = StubHaClient::new(&[
            ("sensor.office_temp", Ok("21.4")),
            ("sensor.broken", Err("connection refused")),
            ("binary_sensor.door", Ok("on")),
        ]);

        let states = fetch_readings(
            &client,
            &readings(&["sensor.office_temp", "sensor.broken", "binary_sensor.door"]),
        )
        .await;

        assert_eq!(states.len(), 3);
        assert_eq!(
            states[&Reading::state("sensor.office_temp")],
            Ok("21.4".to_owned())
        );
        assert_eq!(
            states[&Reading::state("binary_sensor.door")],
            Ok("on".to_owned())
        );
        let message = states[&Reading::state("sensor.broken")]
            .as_ref()
            .expect_err("sensor.broken answers with an error");
        assert!(message.contains("connection refused"), "{message}");
    }

    #[tokio::test]
    async fn an_unknown_entity_is_an_error_not_a_missing_entry() {
        let client = StubHaClient::new(&[("sensor.office_temp", Ok("21.4"))]);

        let states = fetch_readings(&client, &readings(&["sensor.nope"])).await;

        assert_eq!(states.len(), 1);
        assert!(
            states[&Reading::state("sensor.nope")].is_err(),
            "{states:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_entity_list_yields_an_empty_map() {
        let client = StubHaClient::new(&[("sensor.office_temp", Ok("21.4"))]);

        let states = fetch_readings(&client, &[]).await;

        assert!(states.is_empty(), "{states:?}");
        assert_eq!(client.total_calls(), 0);
    }

    /// The render path is a read, and must stay one. A loop that could actuate
    /// something would make repainting a panel a side effect, which is the kind of
    /// surprise nobody debugs quickly.
    #[tokio::test]
    async fn fetching_readings_never_calls_a_service() {
        let client = StubHaClient::new(&[("sensor.office_temp", Ok("21.4"))]);

        fetch_readings(&client, &readings(&["sensor.office_temp"])).await;

        assert!(
            client.services().is_empty(),
            "reading state must never post to /api/services"
        );
    }

    /// Both entities wait on a two-party barrier, so neither can finish until
    /// the other has started. A sequential implementation deadlocks; the
    /// timeout turns that into a failure rather than a hung test.
    #[tokio::test]
    async fn entities_are_fetched_concurrently() {
        struct BarrierHaClient {
            barrier: tokio::sync::Barrier,
        }

        #[async_trait::async_trait]
        impl HaClient for BarrierHaClient {
            async fn read(&self, reading: &Reading) -> Result<String> {
                self.barrier.wait().await;
                Ok(reading.entity_id.clone())
            }

            async fn call(&self, call: &ServiceCall) -> Result<()> {
                bail!("this case actuates nothing, yet was asked for `{call}`")
            }
        }

        let client = BarrierHaClient {
            barrier: tokio::sync::Barrier::new(2),
        };

        let states = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_readings(&client, &readings(&["sensor.a", "sensor.b"])),
        )
        .await
        .expect("both fetches are in flight together");

        assert_eq!(
            states[&Reading::state("sensor.a")],
            Ok("sensor.a".to_owned())
        );
        assert_eq!(
            states[&Reading::state("sensor.b")],
            Ok("sensor.b".to_owned())
        );
    }

    #[test]
    fn new_accepts_a_valid_config() {
        HttpHaClient::new(&config()).expect("a valid config builds a client");
    }

    #[test]
    fn new_rejects_a_token_that_cannot_be_a_header_value() {
        let message = HttpHaClient::new(&HomeAssistant {
            base_url: "http://homeassistant.local:8123".to_owned(),
            token: Some("tok\nInjected: yes".to_owned()),
            token_env: None,
        })
        .expect_err("a token with a newline cannot be a header value")
        .to_string();

        assert!(message.contains("home_assistant.token"), "{message}");
    }

    #[tokio::test]
    async fn state_rejects_an_entity_id_containing_a_slash() {
        let client = HttpHaClient::new(&config()).unwrap();

        // Validation happens before any request, so this never touches the
        // network despite the base URL being unreachable from a test.
        let err = client
            .read(&Reading::state("sensor.office/../../config"))
            .await
            .expect_err("a path-forging entity id is rejected");
        let message = format!("{err:#}");

        assert!(message.contains("sensor.office/../../config"), "{message}");
        assert!(message.contains("request path"), "{message}");
    }

    #[tokio::test]
    async fn state_rejects_an_entity_id_containing_whitespace() {
        let client = HttpHaClient::new(&config()).unwrap();

        let err = client
            .read(&Reading::state("sensor.office temp"))
            .await
            .expect_err("an entity id with a space is rejected");

        assert!(format!("{err:#}").contains("sensor.office temp"), "{err:#}");
    }
    #[tokio::test]
    async fn a_reading_can_target_an_attribute_rather_than_the_state() {
        // The case that motivated this: a weather entity's state is a condition
        // like "partlycloudy", and the temperature is an attribute of it.
        let client = StubHaClient::new(&[
            ("weather.braga", Ok("partlycloudy")),
            ("weather.braga#temperature", Ok("27.1")),
        ]);
        let readings = vec![
            Reading::state("weather.braga"),
            Reading::attribute("weather.braga", "temperature"),
        ];

        let out = fetch_readings(&client, &readings).await;
        assert_eq!(out.len(), 2, "state and attribute are distinct readings");
        assert_eq!(
            out[&Reading::state("weather.braga")],
            Ok("partlycloudy".to_owned())
        );
        assert_eq!(
            out[&Reading::attribute("weather.braga", "temperature")],
            Ok("27.1".to_owned())
        );
    }

    #[test]
    fn a_reading_renders_for_logs_and_keys_distinctly() {
        assert_eq!(Reading::state("weather.braga").to_string(), "weather.braga");
        assert_eq!(
            Reading::attribute("weather.braga", "temperature").to_string(),
            "weather.braga#temperature"
        );
        assert_ne!(
            Reading::state("weather.braga"),
            Reading::attribute("weather.braga", "temperature")
        );
    }

    #[test]
    fn a_token_can_come_from_the_environment_so_it_need_not_be_in_the_config() {
        // Set on this process only; the point is that a ConfigMap never has to
        // carry a credential.
        let name = "PANELD_TEST_HA_TOKEN";
        unsafe { std::env::set_var(name, "from-the-env") };

        let client = HttpHaClient::new(&HomeAssistant {
            base_url: "http://homeassistant.local:8123".to_owned(),
            token: None,
            token_env: Some(name.to_owned()),
        });
        assert!(client.is_ok(), "{:?}", client.err());

        unsafe { std::env::remove_var(name) };
        let message = HttpHaClient::new(&HomeAssistant {
            base_url: "http://homeassistant.local:8123".to_owned(),
            token: None,
            token_env: Some(name.to_owned()),
        })
        .expect_err("an unset variable must be an error, not an empty token")
        .to_string();
        assert!(message.contains(name), "{message}");
    }

    fn toggle() -> ServiceCall {
        let mut data = serde_json::Map::new();
        data.insert(
            "entity_id".to_owned(),
            serde_json::Value::String("light.desk".to_owned()),
        );
        ServiceCall {
            domain: "light".to_owned(),
            service: "toggle".to_owned(),
            data,
        }
    }

    /// Serves exactly one request on loopback and hands back what was received.
    ///
    /// Hand-rolled over a socket rather than mocked: the value of this fixture is
    /// that it observes the bytes the real client puts on the wire, which is the one
    /// thing a stub implementation of the trait can never tell us.
    async fn one_shot(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback is bindable");
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let served = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("one connection arrives");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            // Read until the head and its whole declared body have arrived, rather
            // than until EOF: the client keeps the connection alive, so waiting for
            // a close here would hang the test instead of failing it.
            while !framed(&request) {
                let read = socket.read(&mut buffer).await.expect("the socket reads");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }

            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("the socket writes");
            socket.flush().await.expect("the response is flushed");
            String::from_utf8_lossy(&request).into_owned()
        });

        (base_url, served)
    }

    /// Whether a complete request has arrived: a head, then as many body bytes as
    /// its `content-length` promised.
    fn framed(request: &[u8]) -> bool {
        let text = String::from_utf8_lossy(request);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let declared: usize = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0);
        body.len() >= declared
    }

    #[tokio::test]
    async fn a_service_call_posts_its_data_as_the_body_of_the_services_path() {
        let (base_url, served) = one_shot("200 OK", "[]").await;
        let client = HttpHaClient::new(&HomeAssistant {
            base_url,
            token: Some("tok".to_owned()),
            token_env: None,
        })
        .unwrap();

        client.call(&toggle()).await.expect("a 200 is a success");

        let request = served.await.expect("the fixture served one request");
        assert!(
            request.starts_with("POST /api/services/light/toggle HTTP/1.1\r\n"),
            "the domain and service are the last two path segments: {request}"
        );
        assert!(
            request.ends_with(r#"{"entity_id":"light.desk"}"#),
            "the body is the caller's data verbatim: {request}"
        );
        assert!(
            !request.contains("return_response"),
            "Home Assistant 400s an actuator asked to return a payload: {request}"
        );
        assert!(
            request.to_lowercase().contains("authorization: bearer tok"),
            "the token travels as a default header: {request}"
        );
    }

    #[tokio::test]
    async fn a_refused_service_call_carries_home_assistants_own_explanation() {
        let (base_url, served) = one_shot(
            "400 Bad Request",
            r#"{"message":"extra keys not allowed @ data['brightness']"}"#,
        )
        .await;
        let client = HttpHaClient::new(&HomeAssistant {
            base_url,
            token: Some("tok".to_owned()),
            token_env: None,
        })
        .unwrap();

        let error = client
            .call(&toggle())
            .await
            .expect_err("a 400 is not a success");
        let message = format!("{error:#}");

        served.await.expect("the fixture served one request");
        assert!(message.contains("400"), "{message}");
        assert!(
            message.contains("extra keys not allowed"),
            "an operator needs Home Assistant's reason, not just the status: {message}"
        );
        assert!(
            message.contains("light.toggle"),
            "and needs to know which call it was: {message}"
        );
    }

    /// Config already refuses these, so this is the boundary asserting for itself
    /// rather than trusting the layer above. Validation runs before any request, so
    /// nothing here touches the network despite the base URL being unreachable.
    #[tokio::test]
    async fn call_rejects_a_domain_or_service_that_could_forge_a_request_path() {
        let client = HttpHaClient::new(&config()).unwrap();
        let cases: &[(&str, &str, &str)] = &[
            ("a slash in the domain", "light/../../config", "toggle"),
            ("a slash in the service", "light", "toggle/../states"),
            ("a query in the service", "light", "toggle?return_response"),
            ("a space in the domain", "light switch", "toggle"),
            ("an empty domain", "", "toggle"),
            ("an empty service", "light", ""),
        ];

        for (what, domain, service) in cases {
            let call = ServiceCall {
                domain: (*domain).to_owned(),
                service: (*service).to_owned(),
                data: serde_json::Map::new(),
            };
            let error = client
                .call(&call)
                .await
                .expect_err(&format!("{what} must be rejected before any request"));
            let message = format!("{error:#}");
            assert!(
                message.contains("request path") || message.contains("is empty"),
                "{what}: the error must say why it was refused: {message}"
            );
        }
    }
}
