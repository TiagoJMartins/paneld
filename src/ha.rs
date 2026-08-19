//! Home Assistant entity states, for `ha_entity` widgets.
//!
//! [`HaClient`] is the seam. Everything above this module talks to the trait, so
//! a test supplies canned answers without a network and without a mocking
//! framework. It is deliberately narrow — one entity id in, one state string out
//! — because config already supplies a widget's label and unit, so we never need
//! an entity's attributes.
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
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};

/// How long a single Home Assistant request may take.
///
/// A bound, not a tuning knob: the render loop is a single task, so a hang here
/// wedges every device's frame, not just the widget that is waiting.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest response body echoed back inside an error message, in characters.
const BODY_SNIPPET_CHARS: usize = 200;

/// Reads one entity's state from Home Assistant.
#[async_trait::async_trait]
pub trait HaClient: Send + Sync {
    /// Returns the entity's current state as Home Assistant reports it, e.g.
    /// `"21.4"`, `"on"` or `"unavailable"`.
    async fn state(&self, entity_id: &str) -> Result<String>;
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
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.token))
            .context("home_assistant.token is not usable as an HTTP header value")?;
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
    async fn fetch_state(&self, entity_id: &str) -> Result<String> {
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
        let Some(state) = json.get("state").and_then(serde_json::Value::as_str) else {
            bail!(
                "response JSON has no string `state` field: {}",
                snippet(&body)
            );
        };

        Ok(state.to_owned())
    }
}

#[async_trait::async_trait]
impl HaClient for HttpHaClient {
    async fn state(&self, entity_id: &str) -> Result<String> {
        self.fetch_state(entity_id)
            .await
            .with_context(|| format!("Home Assistant entity `{entity_id}`"))
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
pub async fn fetch_states(
    client: &dyn HaClient,
    entities: &[String],
) -> HashMap<String, Result<String, String>> {
    let distinct = distinct_ids(entities);
    let results = join_all(distinct.iter().map(|id| client.state(id)).collect()).await;

    distinct
        .into_iter()
        .zip(results)
        .map(|(entity_id, result)| {
            let value = result.map_err(|err| {
                let message = format!("{err:#}");
                tracing::warn!(entity_id, error = %message, "Home Assistant fetch failed");
                message
            });
            (entity_id.to_owned(), value)
        })
        .collect()
}

/// Input order, one entry per distinct id.
///
/// Linear membership scan rather than a `HashSet`: an entity list is one id per
/// grid cell, so this is a handful of pointer comparisons against an
/// allocation.
fn distinct_ids(entities: &[String]) -> Vec<&str> {
    let mut distinct: Vec<&str> = Vec::with_capacity(entities.len());
    for entity in entities {
        if !distinct.contains(&entity.as_str()) {
            distinct.push(entity);
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

    if let Some(bad) = entity_id
        .chars()
        .find(|c| matches!(c, '/' | '?' | '#') || c.is_whitespace())
    {
        bail!(
            "entity id `{entity_id}` contains {bad:?}, which would change the \
             request path; expected a plain `domain.object_id`"
        );
    }

    Ok(())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::HomeAssistant;

    /// One canned answer plus how many times that entity has been asked for.
    struct Canned {
        answer: Result<String, String>,
        calls: AtomicUsize,
    }

    /// Canned answers and call counts, so a test can assert what was fetched and
    /// how often without a network.
    struct StubHaClient {
        answers: HashMap<String, Canned>,
        total_calls: AtomicUsize,
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
    }

    #[async_trait::async_trait]
    impl HaClient for StubHaClient {
        async fn state(&self, entity_id: &str) -> Result<String> {
            self.total_calls.fetch_add(1, Ordering::Relaxed);

            let Some(canned) = self.answers.get(entity_id) else {
                bail!("stub has no answer for `{entity_id}`");
            };
            canned.calls.fetch_add(1, Ordering::Relaxed);

            match &canned.answer {
                Ok(state) => Ok(state.clone()),
                Err(message) => bail!("{message}"),
            }
        }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn config() -> HomeAssistant {
        HomeAssistant {
            base_url: "http://homeassistant.local:8123".to_owned(),
            token: "tok".to_owned(),
        }
    }

    #[tokio::test]
    async fn returns_one_entry_per_distinct_entity_fetching_a_repeat_once() {
        let client = StubHaClient::new(&[
            ("sensor.office_temp", Ok("21.4")),
            ("binary_sensor.door", Ok("on")),
        ]);

        let states = fetch_states(
            &client,
            &ids(&[
                "sensor.office_temp",
                "binary_sensor.door",
                "sensor.office_temp",
            ]),
        )
        .await;

        assert_eq!(states.len(), 2);
        assert_eq!(states["sensor.office_temp"], Ok("21.4".to_owned()));
        assert_eq!(states["binary_sensor.door"], Ok("on".to_owned()));
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

        let states = fetch_states(
            &client,
            &ids(&["sensor.office_temp", "sensor.broken", "binary_sensor.door"]),
        )
        .await;

        assert_eq!(states.len(), 3);
        assert_eq!(states["sensor.office_temp"], Ok("21.4".to_owned()));
        assert_eq!(states["binary_sensor.door"], Ok("on".to_owned()));
        let message = states["sensor.broken"]
            .as_ref()
            .expect_err("sensor.broken answers with an error");
        assert!(message.contains("connection refused"), "{message}");
    }

    #[tokio::test]
    async fn an_unknown_entity_is_an_error_not_a_missing_entry() {
        let client = StubHaClient::new(&[("sensor.office_temp", Ok("21.4"))]);

        let states = fetch_states(&client, &ids(&["sensor.nope"])).await;

        assert_eq!(states.len(), 1);
        assert!(states["sensor.nope"].is_err(), "{states:?}");
    }

    #[tokio::test]
    async fn an_empty_entity_list_yields_an_empty_map() {
        let client = StubHaClient::new(&[("sensor.office_temp", Ok("21.4"))]);

        let states = fetch_states(&client, &[]).await;

        assert!(states.is_empty(), "{states:?}");
        assert_eq!(client.total_calls(), 0);
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
            async fn state(&self, entity_id: &str) -> Result<String> {
                self.barrier.wait().await;
                Ok(entity_id.to_owned())
            }
        }

        let client = BarrierHaClient {
            barrier: tokio::sync::Barrier::new(2),
        };

        let states = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_states(&client, &ids(&["sensor.a", "sensor.b"])),
        )
        .await
        .expect("both fetches are in flight together");

        assert_eq!(states["sensor.a"], Ok("sensor.a".to_owned()));
        assert_eq!(states["sensor.b"], Ok("sensor.b".to_owned()));
    }

    #[test]
    fn new_accepts_a_valid_config() {
        HttpHaClient::new(&config()).expect("a valid config builds a client");
    }

    #[test]
    fn new_rejects_a_token_that_cannot_be_a_header_value() {
        let message = HttpHaClient::new(&HomeAssistant {
            base_url: "http://homeassistant.local:8123".to_owned(),
            token: "tok\nInjected: yes".to_owned(),
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
            .state("sensor.office/../../config")
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
            .state("sensor.office temp")
            .await
            .expect_err("an entity id with a space is rejected");

        assert!(format!("{err:#}").contains("sensor.office temp"), "{err:#}");
    }
}
