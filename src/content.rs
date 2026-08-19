//! Values pushed by any device on the network, addressed by widget id.
//!
//! The store's contract is deliberately blunt: **last write wins,
//! unconditionally**. No timestamp ordering, no conflict detection, no merging
//! with whatever was there before. Ordering semantics were the thing that made
//! the previous approach hard to reason about, so a `put` replaces a record
//! wholesale and that is the entire concurrency story.
//!
//! Two properties the rest of the program depends on:
//!
//! - `received_at` is stamped server-side from the `now` handed to [`ContentStore::put`].
//!   The publishers that matter cannot be trusted to have a correct clock, and
//!   no decision here depends on theirs. Staleness is computed at render time
//!   against this stamp.
//! - A push to an unknown widget id is accepted and stored. Publishers are
//!   routinely wired up before their widget is laid out, and rejecting them
//!   makes that ordering painful.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

/// Maximum number of distinct widget ids the store will hold.
///
/// A bound rather than a policy: an errant publisher looping over generated ids
/// must not be able to exhaust memory or grow the persisted file without limit.
pub const MAX_WIDGETS: usize = 1_024;

/// Maximum byte length of any single string field, including the widget id.
pub const MAX_STRING_BYTES: usize = 4_096;

/// Maximum number of rows in one record.
pub const MAX_ROWS: usize = 64;

/// The JSON body of `PUT /api/content/{widget_id}`.
///
/// Unknown fields are tolerated: publishers are ad-hoc scripts, and rejecting a
/// body for carrying an extra key buys nothing.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ContentBody {
    /// The pushed value.
    ///
    /// Double-`Option` because an absent key and an explicit JSON `null` are
    /// genuinely different here: the field is required, but `null` is one of its
    /// legal values. `None` is absent, `Some(None)` is `null`.
    #[serde(default, deserialize_with = "double_option")]
    pub value: Option<Option<Value>>,
    /// Optional state label, interpreted against the widget's `on_values`.
    pub state: Option<String>,
    /// Optional unit, overriding the widget's configured one.
    pub unit: Option<String>,
    /// A small group of related readings. When present, [`ContentBody::value`]
    /// is ignored.
    pub rows: Option<Vec<Row>>,
    /// Whether this push should provoke a render immediately instead of waiting
    /// for the next scheduled one. Defaults to `false`, which keeps a chatty
    /// publisher cheap.
    #[serde(default)]
    pub render: bool,
}

/// Distinguishes an absent key from an explicit `null`.
fn double_option<'de, D, T>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

/// One reading within a multi-row record.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Row {
    /// Stable identifier for the row, if the publisher supplies one.
    pub id: Option<String>,
    /// Human-readable label.
    pub label: Option<String>,
    /// The row's value.
    pub value: Option<Value>,
    /// Unit for this row's value.
    pub unit: Option<String>,
    /// State label for this row.
    pub state: Option<String>,
}

/// What is stored for a widget id, and what `GET /api/content/{widget_id}`
/// returns.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ContentRecord {
    /// The stored value. `Value::Null` whenever [`ContentRecord::rows`] is
    /// present, because rows supersede a scalar value.
    pub value: Value,
    /// State label, interpreted against the widget's `on_values`.
    pub state: Option<String>,
    /// Unit, overriding the widget's configured one.
    pub unit: Option<String>,
    /// The record's rows, if it is a multi-row record.
    pub rows: Option<Vec<Row>>,
    /// When the server received this push. Stamped server-side; client
    /// timestamps are never accepted.
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
}

/// Why a push was rejected.
///
/// Every variant names the offending widget id or field, because the operator's
/// next move is to fix the publisher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutError {
    /// The body carried neither `value` nor `rows`.
    MissingContent {
        /// The widget id that was pushed to.
        widget_id: String,
    },
    /// A string field exceeded [`MAX_STRING_BYTES`].
    StringTooLong {
        /// Dotted path of the offending field, e.g. `rows[3].label`.
        field: String,
        /// The offending length, in bytes.
        len: usize,
    },
    /// The body carried more than [`MAX_ROWS`] rows.
    TooManyRows {
        /// The widget id that was pushed to.
        widget_id: String,
        /// The offending row count.
        rows: usize,
    },
    /// The store already holds [`MAX_WIDGETS`] distinct ids and this is a new
    /// one. An overwrite of an already-stored id is never rejected for this
    /// reason: at the cap, existing publishers must keep working.
    TooManyWidgets {
        /// The new widget id that could not be admitted.
        widget_id: String,
    },
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingContent { widget_id } => write!(
                f,
                "content for widget `{widget_id}` must carry `value` or `rows`"
            ),
            Self::StringTooLong { field, len } => write!(
                f,
                "`{field}` is {len} bytes, over the {MAX_STRING_BYTES} byte limit"
            ),
            Self::TooManyRows { widget_id, rows } => write!(
                f,
                "content for widget `{widget_id}` has {rows} rows, over the {MAX_ROWS} row limit"
            ),
            Self::TooManyWidgets { widget_id } => write!(
                f,
                "cannot store widget `{widget_id}`: the store already holds {MAX_WIDGETS} distinct widget ids"
            ),
        }
    }
}

impl std::error::Error for PutError {}

/// The content store: an in-memory map plus the file it is persisted to.
///
/// Interior mutability so that `&self` methods work from `axum` handlers holding
/// a shared reference.
#[derive(Debug)]
pub struct ContentStore {
    path: PathBuf,
    records: Mutex<HashMap<String, ContentRecord>>,
}

impl ContentStore {
    /// Opens the store persisted at `path`.
    ///
    /// Never fails: a missing file starts empty silently, and an unreadable or
    /// corrupt one logs a warning and starts empty. Refusing to boot because a
    /// cache of last-seen values is damaged would be the wrong trade — the panel
    /// would show nothing at all.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let records = read_records(&path);
        Self {
            path,
            records: Mutex::new(records),
        }
    }

    /// Stores content for `widget_id`, replacing any previous record wholesale.
    ///
    /// Bounds are checked before anything is inserted, so a rejected push leaves
    /// the store exactly as it was.
    pub fn put(
        &self,
        widget_id: &str,
        body: ContentBody,
        now: OffsetDateTime,
    ) -> std::result::Result<ContentRecord, PutError> {
        check_string("widget_id", widget_id)?;
        let record = build_record(widget_id, body, now)?;

        let mut records = self.lock();
        if records.len() >= MAX_WIDGETS && !records.contains_key(widget_id) {
            return Err(PutError::TooManyWidgets {
                widget_id: widget_id.to_owned(),
            });
        }
        records.insert(widget_id.to_owned(), record.clone());
        Ok(record)
    }

    /// The record stored for `widget_id`, if any.
    pub fn get(&self, widget_id: &str) -> Option<ContentRecord> {
        self.lock().get(widget_id).cloned()
    }

    /// Every stored record, for rendering a whole dashboard from one consistent
    /// view of the store.
    pub fn snapshot(&self) -> HashMap<String, ContentRecord> {
        self.lock().clone()
    }

    /// Writes the whole store to its configured path, atomically.
    ///
    /// The temporary file is created in the *same* directory as the target: a
    /// temp file elsewhere would make the rename a cross-device copy and lose
    /// the atomicity this exists for.
    pub fn persist(&self) -> Result<()> {
        let records = self.snapshot();
        let json = serde_json::to_vec_pretty(&records).context("serialising content store")?;

        let tmp = self.temp_path();
        std::fs::write(&tmp, &json)
            .with_context(|| format!("writing content store temp file {}", tmp.display()))?;

        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).with_context(|| {
                format!("replacing content store {}", self.path.display())
            });
        }
        Ok(())
    }

    /// A per-call temporary path beside the target file, unique so that two
    /// concurrent persists cannot write to the same scratch file.
    fn temp_path(&self) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);

        let dir = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let stem = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "content.json".to_owned());
        dir.join(format!(".{stem}.tmp-{}-{nonce}", std::process::id()))
    }

    /// A panicking handler must not wedge content updates for the rest of the
    /// process: the map is structurally intact either way, so recover the guard.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ContentRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Validates a body and turns it into the record to store.
fn build_record(
    widget_id: &str,
    body: ContentBody,
    now: OffsetDateTime,
) -> std::result::Result<ContentRecord, PutError> {
    let ContentBody {
        value,
        state,
        unit,
        rows,
        render: _,
    } = body;

    if let Some(state) = &state {
        check_string("state", state)?;
    }
    if let Some(unit) = &unit {
        check_string("unit", unit)?;
    }

    // `rows` present means rows *are* the content, so the scalar value is
    // dropped rather than kept alongside them for a renderer to have to choose
    // between.
    let value = match &rows {
        Some(rows) => {
            check_rows(widget_id, rows)?;
            Value::Null
        }
        None => match value {
            Some(value) => {
                let value = value.unwrap_or(Value::Null);
                check_value_strings("value", &value)?;
                value
            }
            None => {
                return Err(PutError::MissingContent {
                    widget_id: widget_id.to_owned(),
                });
            }
        },
    };

    Ok(ContentRecord {
        value,
        state,
        unit,
        rows,
        received_at: now,
    })
}

fn check_rows(widget_id: &str, rows: &[Row]) -> std::result::Result<(), PutError> {
    if rows.len() > MAX_ROWS {
        return Err(PutError::TooManyRows {
            widget_id: widget_id.to_owned(),
            rows: rows.len(),
        });
    }
    for (index, row) in rows.iter().enumerate() {
        for (name, field) in [
            ("id", &row.id),
            ("label", &row.label),
            ("unit", &row.unit),
            ("state", &row.state),
        ] {
            if let Some(text) = field {
                check_string(&format!("rows[{index}].{name}"), text)?;
            }
        }
        if let Some(value) = &row.value {
            check_value_strings(&format!("rows[{index}].value"), value)?;
        }
    }
    Ok(())
}

fn check_string(field: &str, text: &str) -> std::result::Result<(), PutError> {
    if text.len() > MAX_STRING_BYTES {
        return Err(PutError::StringTooLong {
            field: field.to_owned(),
            len: text.len(),
        });
    }
    Ok(())
}

/// Bounds every string reachable inside a JSON value.
///
/// A pushed value is documented as a scalar, but nothing stops a publisher
/// sending a nested structure, and each of its strings costs memory and disk
/// just the same. Recursion depth is bounded by `serde_json`'s own nesting
/// limit, reached while parsing, long before this runs.
fn check_value_strings(field: &str, value: &Value) -> std::result::Result<(), PutError> {
    match value {
        Value::String(text) => check_string(field, text),
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_value_strings(field, item)),
        Value::Object(entries) => entries.iter().try_for_each(|(key, item)| {
            check_string(field, key)?;
            check_value_strings(field, item)
        }),
        _ => Ok(()),
    }
}

fn read_records(path: &Path) -> HashMap<String, ContentRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "content store is unreadable; starting with an empty store"
            );
            return HashMap::new();
        }
    };

    match serde_json::from_str(&text) {
        Ok(records) => records,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "content store is corrupt; starting with an empty store"
            );
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed stamp, so `received_at` never depends on wall-clock time. The
    /// subsecond component is deliberate: real stamps carry nanoseconds, and a
    /// persisted store must reload them unchanged.
    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
            + time::Duration::nanoseconds(123_456_789)
    }

    fn body(json: &str) -> ContentBody {
        serde_json::from_str(json).expect("body fixture should deserialise")
    }

    /// A unique directory per test, so file tests cannot see each other's state.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "paneld-content-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("content.json")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store() -> ContentStore {
        ContentStore::load("unused-in-memory-only.json")
    }

    #[test]
    fn parses_the_documented_body() {
        let parsed = body(r#"{"value":"on","state":"alert","unit":null,"rows":null,"render":false}"#);
        assert_eq!(parsed.value, Some(Some(Value::from("on"))));
        assert_eq!(parsed.state.as_deref(), Some("alert"));
        assert_eq!(parsed.unit, None);
        assert_eq!(parsed.rows, None);
        assert!(!parsed.render);
    }

    #[test]
    fn render_defaults_to_false_when_absent() {
        assert!(!body(r#"{"value":1}"#).render);
        assert!(body(r#"{"value":1,"render":true}"#).render);
    }

    #[test]
    fn an_explicit_null_value_is_stored() {
        let store = store();
        let record = store
            .put("sensor", body(r#"{"value":null}"#), at(0))
            .expect("an explicit null value is a legal push");

        assert_eq!(record.value, Value::Null);
        assert_eq!(store.get("sensor").unwrap().value, Value::Null);
    }

    #[test]
    fn a_body_without_value_or_rows_is_rejected() {
        let store = store();
        let error = store
            .put("sensor", body(r#"{"state":"alert"}"#), at(0))
            .expect_err("an absent value is not the same as a null one");

        assert_eq!(
            error,
            PutError::MissingContent {
                widget_id: "sensor".to_owned()
            }
        );
        assert!(store.get("sensor").is_none(), "a rejected push stores nothing");
        assert!(error.to_string().contains("sensor"), "{error}");
    }

    #[test]
    fn a_null_rows_key_does_not_count_as_content() {
        let store = store();
        assert!(store.put("sensor", body(r#"{"rows":null}"#), at(0)).is_err());
    }

    #[test]
    fn accepts_every_scalar_value_kind() {
        let store = store();
        for (id, json, expected) in [
            ("s", r#"{"value":"on"}"#, Value::from("on")),
            ("n", r#"{"value":21.5}"#, Value::from(21.5)),
            ("b", r#"{"value":true}"#, Value::from(true)),
        ] {
            let record = store.put(id, body(json), at(0)).unwrap();
            assert_eq!(record.value, expected);
        }
    }

    #[test]
    fn the_second_put_wins_wholesale() {
        let store = store();
        store
            .put(
                "sensor",
                body(r#"{"value":"on","state":"alert","unit":"°C"}"#),
                at(0),
            )
            .unwrap();
        let second = store
            .put("sensor", body(r#"{"value":"off"}"#), at(60))
            .unwrap();

        assert_eq!(second.value, Value::from("off"));
        assert_eq!(second.state, None, "a field absent from the second put does not survive it");
        assert_eq!(second.unit, None, "a field absent from the second put does not survive it");
        assert_eq!(second.received_at, at(60));
        assert_eq!(store.get("sensor").unwrap(), second);
    }

    #[test]
    fn an_older_stamp_still_wins_because_last_write_wins_unconditionally() {
        let store = store();
        store.put("sensor", body(r#"{"value":"new"}"#), at(600)).unwrap();
        store.put("sensor", body(r#"{"value":"old"}"#), at(0)).unwrap();

        let stored = store.get("sensor").unwrap();
        assert_eq!(stored.value, Value::from("old"));
        assert_eq!(stored.received_at, at(0));
    }

    #[test]
    fn rows_supersede_value() {
        let store = store();
        let record = store
            .put(
                "group",
                body(r#"{"value":"ignored","rows":[{"id":"a","label":"A","value":1,"unit":"C","state":"ok"}]}"#),
                at(0),
            )
            .unwrap();

        assert_eq!(record.value, Value::Null, "rows present means value is ignored");
        let rows = record.rows.expect("rows are stored");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_deref(), Some("a"));
        assert_eq!(rows[0].value, Some(Value::from(1)));
    }

    #[test]
    fn rows_alone_are_content_enough() {
        let store = store();
        let record = store
            .put("group", body(r#"{"rows":[{"label":"A","value":1}]}"#), at(0))
            .expect("rows satisfy the requirement that value would otherwise");
        assert_eq!(record.value, Value::Null);
    }

    #[test]
    fn get_and_snapshot_report_what_is_stored() {
        let store = store();
        assert!(store.get("nothing").is_none());
        assert!(store.snapshot().is_empty());

        store.put("a", body(r#"{"value":1}"#), at(0)).unwrap();
        store.put("b", body(r#"{"value":2}"#), at(1)).unwrap();

        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot["a"].value, Value::from(1));
        assert_eq!(snapshot["b"].received_at, at(1));
    }

    #[test]
    fn rejects_an_oversize_widget_id() {
        let store = store();
        let id = "w".repeat(MAX_STRING_BYTES + 1);
        let error = store.put(&id, body(r#"{"value":1}"#), at(0)).unwrap_err();

        assert_eq!(
            error,
            PutError::StringTooLong {
                field: "widget_id".to_owned(),
                len: MAX_STRING_BYTES + 1,
            }
        );
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn accepts_a_string_field_exactly_at_the_byte_bound() {
        let store = store();
        let text = "u".repeat(MAX_STRING_BYTES);
        store
            .put("sensor", body(&format!(r#"{{"value":1,"unit":"{text}"}}"#)), at(0))
            .expect("the bound is inclusive");
    }

    #[test]
    fn rejects_oversize_strings_wherever_they_appear() {
        let store = store();
        let text = "x".repeat(MAX_STRING_BYTES + 1);

        for (json, field) in [
            (format!(r#"{{"value":"{text}"}}"#), "value"),
            (format!(r#"{{"value":1,"state":"{text}"}}"#), "state"),
            (format!(r#"{{"value":1,"unit":"{text}"}}"#), "unit"),
            (
                format!(r#"{{"rows":[{{"label":"ok"}},{{"label":"{text}"}}]}}"#),
                "rows[1].label",
            ),
            (
                format!(r#"{{"rows":[{{"value":"{text}"}}]}}"#),
                "rows[0].value",
            ),
        ] {
            let error = store.put("sensor", body(&json), at(0)).unwrap_err();
            assert_eq!(
                error,
                PutError::StringTooLong {
                    field: field.to_owned(),
                    len: MAX_STRING_BYTES + 1,
                },
                "{field} should be bounded"
            );
        }
        assert!(store.snapshot().is_empty(), "no rejected push was stored");
    }

    #[test]
    fn rejects_more_rows_than_the_limit() {
        let store = store();
        let row = r#"{"label":"a","value":1}"#;

        let at_limit = format!(r#"{{"rows":[{}]}}"#, vec![row; MAX_ROWS].join(","));
        store
            .put("group", body(&at_limit), at(0))
            .expect("the row limit is inclusive");

        let over = format!(r#"{{"rows":[{}]}}"#, vec![row; MAX_ROWS + 1].join(","));
        assert_eq!(
            store.put("group", body(&over), at(1)).unwrap_err(),
            PutError::TooManyRows {
                widget_id: "group".to_owned(),
                rows: MAX_ROWS + 1,
            }
        );
        assert_eq!(
            store.get("group").unwrap().received_at,
            at(0),
            "the rejected push left the previous record intact"
        );
    }

    #[test]
    fn at_the_widget_cap_new_ids_are_rejected_but_overwrites_still_work() {
        let store = store();
        for index in 0..MAX_WIDGETS {
            store
                .put(&format!("w{index}"), body(r#"{"value":1}"#), at(0))
                .unwrap();
        }

        assert_eq!(
            store.put("new", body(r#"{"value":1}"#), at(1)).unwrap_err(),
            PutError::TooManyWidgets {
                widget_id: "new".to_owned()
            }
        );

        let overwritten = store
            .put("w0", body(r#"{"value":2}"#), at(1))
            .expect("an existing publisher must keep working at the cap");
        assert_eq!(overwritten.value, Value::from(2));
        assert_eq!(store.snapshot().len(), MAX_WIDGETS);
    }

    #[test]
    fn a_missing_file_starts_empty() {
        let dir = Dir::new("missing");
        let store = ContentStore::load(dir.file());
        assert!(store.snapshot().is_empty());
        assert!(!dir.file().exists(), "load does not create the file");
    }

    #[test]
    fn persist_then_load_round_trips_records() {
        let dir = Dir::new("roundtrip");
        let store = ContentStore::load(dir.file());
        store
            .put(
                "sensor",
                body(r#"{"value":"on","state":"alert","unit":"°C"}"#),
                at(0),
            )
            .unwrap();
        store
            .put(
                "group",
                body(r#"{"rows":[{"id":"a","label":"A","value":1,"unit":"C","state":"ok"}]}"#),
                at(90),
            )
            .unwrap();
        store.persist().unwrap();

        let reloaded = ContentStore::load(dir.file()).snapshot();
        assert_eq!(reloaded, store.snapshot());
        assert_eq!(reloaded["sensor"].received_at, at(0));
        assert_eq!(reloaded["group"].received_at, at(90));
    }

    #[test]
    fn persist_leaves_no_temporary_file_behind() {
        let dir = Dir::new("temp");
        let store = ContentStore::load(dir.file());
        store.put("sensor", body(r#"{"value":1}"#), at(0)).unwrap();
        store.persist().unwrap();
        store.persist().unwrap();

        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["content.json"]);
    }

    #[test]
    fn persist_overwrites_a_previous_store() {
        let dir = Dir::new("overwrite");
        let first = ContentStore::load(dir.file());
        first.put("gone", body(r#"{"value":1}"#), at(0)).unwrap();
        first.persist().unwrap();

        let second = ContentStore::load(dir.file());
        second.put("kept", body(r#"{"value":2}"#), at(1)).unwrap();
        second.persist().unwrap();

        // The second store loaded the first's contents, so both ids survive; the
        // point is that persisting replaces the file rather than appending.
        let reloaded = ContentStore::load(dir.file()).snapshot();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded["kept"].value, Value::from(2));
    }

    #[test]
    fn a_corrupt_file_yields_an_empty_store() {
        let dir = Dir::new("corrupt");
        std::fs::write(dir.file(), "{not json at all").unwrap();

        let store = ContentStore::load(dir.file());
        assert!(store.snapshot().is_empty(), "a damaged store must not stop the boot");

        // And the store is usable afterwards, replacing the bad file on persist.
        store.put("sensor", body(r#"{"value":1}"#), at(0)).unwrap();
        store.persist().unwrap();
        assert_eq!(ContentStore::load(dir.file()).snapshot().len(), 1);
    }

    #[test]
    fn a_file_of_the_wrong_shape_yields_an_empty_store() {
        let dir = Dir::new("wrong-shape");
        std::fs::write(dir.file(), r#"["not","a","map"]"#).unwrap();
        assert!(ContentStore::load(dir.file()).snapshot().is_empty());
    }
}
