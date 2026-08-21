//! Device telemetry, read from the headers of a display poll.
//!
//! Two client families report the same data under different header names, so
//! every field accepts both spellings and lookup is case-insensitive. Parsing is
//! total: a header that is missing, non-UTF-8 or unparseable yields `None` for
//! that one field, because a device sending nonsense in one header must not cost
//! us the others.
//!
//! Know the ceiling before building on this. The KOReader client in service
//! sends battery as an integer percentage only — no voltage, no charging state —
//! hardcodes `rssi` to `"0"` as an unfinished TODO, and sends no firmware
//! version and no model. Battery percentage is the only genuinely available
//! statistic from that panel, so nothing downstream may depend on richer data.
//!
//! Charging state is the exception worth knowing about, because the two families
//! diverge on it rather than merely spelling it differently:
//!
//! - The TRMNL ESP32 firmware sends `battery-charging` as `1`/`0` (the charger
//!   IC's own view, omitted entirely on boards that cannot read it) and
//!   `usb-connected` as `true`/`false` (VBUS present). Both are omitted rather
//!   than sent as false when unknown, which is why both are `Option<bool>` here:
//!   "not reported" and "not charging" are different answers.
//! - No KOReader client sends either today. A fork can: KOReader exposes
//!   `powerd:isCharging()` and `powerd:isCharged()` on Kindle hardware, so
//!   `battery-charging: true` is one header away. That is the spelling to use.
//!
//! `battery-charging: 0` does not mean unplugged — the firmware's own enum reads
//! "charge complete, disabled, or no battery" — so only `usb-connected`
//! distinguishes plugged-and-full from running on the cell.

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
    /// Whether the charger reports it is actively filling the cell. `None` when
    /// the device did not say, which is every client but recent TRMNL firmware.
    pub charging: Option<bool>,
    /// Whether USB power is present. Plugged in and full reads as
    /// `usb_connected: true` with `charging: false`.
    pub usb_connected: Option<bool>,
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
            charging: flag(headers, &["battery-charging"]),
            usb_connected: flag(headers, &["usb-connected"]),
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
        overwrite(&mut self.charging, incoming.charging);
        overwrite(&mut self.usb_connected, incoming.usb_connected);
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

/// The first of `names` present with a value that reads as a boolean.
///
/// Both value families are accepted for every one of these headers because the
/// firmware itself is inconsistent: the same function that sends
/// `battery-charging` as `1`/`0` sends `usb-connected` as `true`/`false`. A
/// value in neither family reads as `None` — an unknown state, not a false one.
fn flag(headers: &HeaderMap, names: &[&str]) -> Option<bool> {
    let value = text(headers, names)?.to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
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
    fn reads_the_firmware_charging_headers_in_the_spellings_it_sends() {
        // The same firmware function sends one as 1/0 and the other as
        // true/false, so this pair is exactly what arrives on the wire.
        let telemetry = read(&[("battery-charging", "1"), ("usb-connected", "true")]);
        assert_eq!(telemetry.charging, Some(true));
        assert_eq!(telemetry.usb_connected, Some(true));

        let telemetry = read(&[("battery-charging", "0"), ("usb-connected", "false")]);
        assert_eq!(telemetry.charging, Some(false));
        assert_eq!(telemetry.usb_connected, Some(false));
    }

    #[test]
    fn either_value_family_reads_for_either_charging_header() {
        // A KOReader fork sends `tostring(powerd:isCharging())`, which is
        // "true"/"false" — under the header name the firmware spells 1/0.
        assert_eq!(read(&[("battery-charging", "TRUE")]).charging, Some(true));
        assert_eq!(read(&[("battery-charging", "yes")]).charging, Some(true));
        assert_eq!(read(&[("usb-connected", "1")]).usb_connected, Some(true));
        assert_eq!(read(&[("usb-connected", "No")]).usb_connected, Some(false));
    }

    #[test]
    fn an_absent_or_unreadable_charging_header_is_unknown_and_never_false() {
        // The firmware omits both headers on a board that cannot read the
        // charger. Reading that as "not charging" would report a panel on mains
        // as running down, and vice versa.
        assert_eq!(read(&[("percent-charged", "50")]).charging, None);
        assert_eq!(read(&[("battery-charging", "")]).charging, None);
        assert_eq!(read(&[("battery-charging", "maybe")]).charging, None);
        assert_eq!(read(&[("usb-connected", "2")]).usb_connected, None);
    }

    #[test]
    fn a_poll_that_omits_the_charging_headers_keeps_the_last_known_state() {
        let mut stored = read(&[("battery-charging", "1"), ("usb-connected", "true")]);

        stored.merge_from(read(&[("percent-charged", "91")]));

        assert_eq!(stored.charging, Some(true));
        assert_eq!(stored.usb_connected, Some(true));

        stored.merge_from(read(&[("battery-charging", "0")]));

        assert_eq!(stored.charging, Some(false), "a fresh reading wins");
        assert_eq!(stored.usb_connected, Some(true));
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
