//! Device telemetry, read from the headers of a display poll.
//!
//! Two client families report the same data under different header names, so
//! every field accepts both spellings and lookup is case-insensitive. Parsing is
//! total: a header that is missing, non-UTF-8 or unparseable yields `None` for
//! that one field, because a device sending nonsense in one header must not cost
//! us the others.
//!
//! Know the ceiling before building on this. The KOReader client in service
//! sends battery as an integer percentage only — no voltage — hardcodes `rssi`
//! to `"0"` as an unfinished TODO, and sends no firmware version and no model.
//! Battery percentage is the only genuinely available statistic from the panel
//! in service, so nothing downstream may depend on richer data.

use axum::http::HeaderMap;
use serde::Serialize;

/// Below this reading, in millivolts, `battery-voltage` is interpreted as volts.
///
/// Some firmware reports integer millivolts and some reports decimal volts, with
/// nothing in the header to tell them apart. The heuristic is safe because the
/// ranges cannot overlap: a real lithium cell never reads below 100 mV, and
/// never above about 5 V.
const VOLTS_CEILING: f64 = 100.0;

/// The most recent reading from one device.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Telemetry {
    pub battery_percent: Option<f64>,
    pub battery_millivolts: Option<f64>,
    pub rssi: Option<i64>,
    pub firmware_version: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub mac: Option<String>,
    pub model: Option<String>,
}

impl Telemetry {
    /// Reads whatever this poll happens to report. Never fails.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            battery_percent: number(headers, &["percent-charged", "battery-percent"]),
            battery_millivolts: number(headers, &["battery-voltage"]).map(normalise_voltage),
            rssi: number(headers, &["rssi"]),
            // `user-agent` is a poor firmware version, but on clients that send
            // no `fw-version` it is the only build identifier available.
            firmware_version: text(headers, &["fw-version", "user-agent"]),
            width: number(headers, &["png-width", "width"]),
            height: number(headers, &["png-height", "height"]),
            mac: text(headers, &["id"]),
            model: text(headers, &["model"]),
        }
    }

    /// Folds a newer, possibly partial, reading into this one.
    ///
    /// A field absent from `incoming` keeps its previously known value: polls
    /// are header-light at times, and a wholesale replacement would blank the
    /// device's whole record on the first sparse one.
    pub fn merge_from(&mut self, incoming: Telemetry) {
        overwrite(&mut self.battery_percent, incoming.battery_percent);
        overwrite(&mut self.battery_millivolts, incoming.battery_millivolts);
        overwrite(&mut self.rssi, incoming.rssi);
        overwrite(&mut self.firmware_version, incoming.firmware_version);
        overwrite(&mut self.width, incoming.width);
        overwrite(&mut self.height, incoming.height);
        overwrite(&mut self.mac, incoming.mac);
        overwrite(&mut self.model, incoming.model);
    }
}

fn overwrite<T>(slot: &mut Option<T>, incoming: Option<T>) {
    if incoming.is_some() {
        *slot = incoming;
    }
}

/// Converts a raw `battery-voltage` reading to millivolts.
fn normalise_voltage(raw: f64) -> f64 {
    if raw < VOLTS_CEILING {
        return raw * 1000.0;
    }
    raw
}

/// The first of `names` present with a UTF-8 value, trimmed and non-empty.
///
/// `HeaderMap` lookup by `&str` is already case-insensitive, so the names here
/// are written lowercase and matched however the device spelled them.
fn text(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    for name in names {
        let Some(value) = headers.get(*name) else {
            continue;
        };
        let Ok(value) = value.to_str() else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

/// The first of `names` that both is present and parses as `T`.
///
/// A header that is present but unparseable is skipped rather than fatal, so a
/// device sending garbage in the preferred spelling can still be read from the
/// fallback one.
fn number<T: std::str::FromStr>(headers: &HeaderMap, names: &[&str]) -> Option<T> {
    for name in names {
        if let Some(parsed) = text(headers, &[name]).and_then(|value| value.parse().ok()) {
            return Some(parsed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn read(pairs: &[(&str, &str)]) -> Telemetry {
        Telemetry::from_headers(&headers(pairs))
    }

    #[test]
    fn reads_nothing_from_an_empty_poll() {
        assert_eq!(
            Telemetry::from_headers(&HeaderMap::new()),
            Telemetry::default()
        );
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let telemetry = read(&[
            ("Percent-Charged", "62"),
            ("FW-VERSION", "1.5.2"),
            ("PNG-Width", "800"),
            ("Id", "AA:BB:CC:DD:EE:FF"),
        ]);
        assert_eq!(telemetry.battery_percent, Some(62.0));
        assert_eq!(telemetry.firmware_version.as_deref(), Some("1.5.2"));
        assert_eq!(telemetry.width, Some(800));
        assert_eq!(telemetry.mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
    }

    #[test]
    fn accepts_either_battery_percent_spelling() {
        assert_eq!(
            read(&[("percent-charged", "41")]).battery_percent,
            Some(41.0)
        );
        assert_eq!(
            read(&[("battery-percent", "41")]).battery_percent,
            Some(41.0)
        );
    }

    #[test]
    fn percent_charged_beats_battery_percent() {
        let telemetry = read(&[("battery-percent", "10"), ("percent-charged", "90")]);
        assert_eq!(telemetry.battery_percent, Some(90.0));
    }

    #[test]
    fn accepts_either_dimension_spelling() {
        let png = read(&[("png-width", "800"), ("png-height", "480")]);
        assert_eq!((png.width, png.height), (Some(800), Some(480)));

        let plain = read(&[("width", "1024"), ("height", "758")]);
        assert_eq!((plain.width, plain.height), (Some(1024), Some(758)));
    }

    #[test]
    fn png_dimensions_beat_plain_ones() {
        let telemetry = read(&[
            ("width", "1024"),
            ("height", "758"),
            ("png-width", "800"),
            ("png-height", "480"),
        ]);
        assert_eq!((telemetry.width, telemetry.height), (Some(800), Some(480)));
    }

    #[test]
    fn fw_version_beats_user_agent() {
        let telemetry = read(&[("user-agent", "ESP32HTTPClient"), ("fw-version", "1.5.2")]);
        assert_eq!(telemetry.firmware_version.as_deref(), Some("1.5.2"));
    }

    #[test]
    fn user_agent_stands_in_when_fw_version_is_absent() {
        let telemetry = read(&[("user-agent", "KOReader/2024.10")]);
        assert_eq!(
            telemetry.firmware_version.as_deref(),
            Some("KOReader/2024.10")
        );
    }

    #[test]
    fn integer_voltage_is_already_millivolts() {
        assert_eq!(
            read(&[("battery-voltage", "3700")]).battery_millivolts,
            Some(3700.0)
        );
    }

    #[test]
    fn decimal_voltage_is_scaled_to_millivolts() {
        assert_eq!(
            read(&[("battery-voltage", "3.7")]).battery_millivolts,
            Some(3700.0)
        );
    }

    #[test]
    fn voltage_of_exactly_one_hundred_is_millivolts() {
        assert_eq!(
            read(&[("battery-voltage", "100")]).battery_millivolts,
            Some(100.0)
        );
        // Just under the boundary is still read as volts.
        assert_eq!(
            read(&[("battery-voltage", "99")]).battery_millivolts,
            Some(99_000.0)
        );
    }

    #[test]
    fn a_garbage_header_costs_only_its_own_field() {
        let telemetry = read(&[
            ("battery-voltage", "not-a-number"),
            ("rssi", "-58"),
            ("percent-charged", "77"),
            ("model", "og"),
        ]);
        assert_eq!(telemetry.battery_millivolts, None);
        assert_eq!(telemetry.rssi, Some(-58));
        assert_eq!(telemetry.battery_percent, Some(77.0));
        assert_eq!(telemetry.model.as_deref(), Some("og"));
    }

    #[test]
    fn a_non_utf8_header_yields_none_rather_than_an_error() {
        let mut map = HeaderMap::new();
        map.insert("id", HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());
        map.insert("model", HeaderValue::from_static("og_plus"));

        let telemetry = Telemetry::from_headers(&map);
        assert_eq!(telemetry.mac, None);
        assert_eq!(telemetry.model.as_deref(), Some("og_plus"));
    }

    #[test]
    fn an_unparseable_preferred_spelling_falls_through_to_the_fallback() {
        let telemetry = read(&[("percent-charged", "n/a"), ("battery-percent", "33")]);
        assert_eq!(telemetry.battery_percent, Some(33.0));
    }

    #[test]
    fn reads_the_koreader_shape() {
        // The client in service: integer percent, rssi hardcoded to "0", and no
        // voltage, firmware version or model at all.
        let telemetry = read(&[("battery-percent", "88"), ("rssi", "0")]);
        assert_eq!(telemetry.battery_percent, Some(88.0));
        assert_eq!(telemetry.rssi, Some(0));
        assert_eq!(telemetry.battery_millivolts, None);
        assert_eq!(telemetry.firmware_version, None);
        assert_eq!(telemetry.model, None);
    }

    #[test]
    fn an_empty_incoming_reading_preserves_everything() {
        let mut stored = read(&[
            ("percent-charged", "50"),
            ("battery-voltage", "3.9"),
            ("rssi", "-40"),
            ("fw-version", "1.5.2"),
            ("png-width", "800"),
            ("png-height", "480"),
            ("id", "aa:bb"),
            ("model", "og"),
        ]);
        let before = stored.clone();

        stored.merge_from(Telemetry::default());

        assert_eq!(stored, before);
    }

    #[test]
    fn incoming_updates_only_the_fields_it_carries() {
        let mut stored = read(&[("percent-charged", "50"), ("fw-version", "1.5.2")]);

        stored.merge_from(read(&[("percent-charged", "48")]));

        assert_eq!(stored.battery_percent, Some(48.0));
        assert_eq!(stored.firmware_version.as_deref(), Some("1.5.2"));
    }

    #[test]
    fn merging_fills_fields_that_were_previously_unknown() {
        let mut stored = read(&[("percent-charged", "50")]);

        stored.merge_from(read(&[("model", "og_plus"), ("rssi", "-61")]));

        assert_eq!(stored.battery_percent, Some(50.0));
        assert_eq!(stored.model.as_deref(), Some("og_plus"));
        assert_eq!(stored.rssi, Some(-61));
    }
}
