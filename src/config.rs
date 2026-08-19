//! Dashboard configuration: a single TOML file parsed into a validated [`Config`].
//!
//! [`parse`] is a pure function from TOML text to a validated configuration. It
//! is the whole config seam: every rule in this module is asserted against it
//! directly, and the rest of the program only ever sees an already-valid
//! [`Config`].
//!
//! A validation failure is always an error naming the offending widget id or
//! field, because the caller's job on failure is to keep the previous
//! configuration in effect and log something the author can act on.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// Inclusive bounds on a device's `refresh_rate`, in seconds.
///
/// The lower bound is a battery guard: the device turns `refresh_rate` straight
/// into a deep-sleep timer, so a small value wakes it constantly.
pub const REFRESH_RATE_BOUNDS: std::ops::RangeInclusive<u32> = 30..=86_400;

/// Inclusive bounds on a device's `render_interval`, in seconds.
pub const RENDER_INTERVAL_BOUNDS: std::ops::RangeInclusive<u32> = 5..=86_400;

/// Upper bound on a device's frame dimensions, in pixels.
///
/// Not a protocol rule — a guard so that a typo cannot ask for a buffer that
/// exhausts memory.
pub const MAX_DIMENSION: u32 = 4_096;

/// Smallest grid cell that can hold anything legible, in pixels.
///
/// Also a hard safety floor rather than a taste judgement: below roughly this
/// size a cell's content box stops being able to fit a single glyph, and the text
/// layout engine panics rather than returning an error.
pub const MIN_CELL: u32 = 40;

/// A validated configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub server: Server,
    pub home_assistant: Option<HomeAssistant>,
    pub devices: Vec<Device>,
}

/// Server-wide settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Address to bind the HTTP listener to.
    pub listen: SocketAddr,
    /// Base URL that frame URLs are built from. Must be reachable *from the
    /// device*, so never a loopback or container-internal address.
    ///
    /// Stored without a trailing slash: the panel clients build request URLs by
    /// plain string concatenation, and a trailing slash there produces a doubled
    /// path separator.
    pub public_base_url: String,
    /// Where the content store is persisted. Relative paths resolve against the
    /// process working directory.
    #[serde(default = "default_content_path")]
    pub content_path: String,
}

fn default_content_path() -> String {
    "paneld-content.json".to_owned()
}

/// Home Assistant connection details, required by any `ha_entity` widget.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistant {
    /// e.g. `http://homeassistant.local:8123`. Stored without a trailing slash.
    pub base_url: String,
    /// Long-lived access token.
    pub token: String,
}

/// One panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    /// Selects the `/d/<id>/` route prefix.
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub palette: Palette,
    pub dither: Dither,
    /// Advice sent to the device telling it when to poll again, in seconds.
    pub refresh_rate: u32,
    /// How often this server rebuilds the device's frame, in seconds. Defaults
    /// to `refresh_rate`.
    ///
    /// A separate clock from `refresh_rate` and never conflated with it:
    /// rendering more often than the device looks is wasted work, and rendering
    /// less often means it sometimes fetches a frame it already has.
    pub render_interval: u32,
    pub grid: Grid,
    pub widgets: Vec<Widget>,
}

/// The dashboard grid a device's widgets are placed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grid {
    pub cols: u32,
    pub rows: u32,
}

/// One widget, placed explicitly on the grid rather than inferred from document
/// order.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Widget {
    /// Also the content push address: `PUT /api/content/<id>`.
    pub id: String,
    pub kind: WidgetKind,
    pub col: u32,
    pub row: u32,
    #[serde(default = "one")]
    pub col_span: u32,
    #[serde(default = "one")]
    pub row_span: u32,
    pub label: Option<String>,
    pub unit: Option<String>,
    /// How long pushed content stays fresh, in seconds. `0` disables the
    /// staleness timer, which is the default: a widget should not start
    /// reporting itself stale just because its author never thought about it.
    #[serde(default)]
    pub stale_after: u64,
    /// Home Assistant entity id, for `kind = "ha_entity"`.
    pub entity: Option<String>,
    /// Values that put a `beacon` in its "on" state.
    #[serde(default = "default_on_values")]
    pub on_values: Vec<String>,
}

fn one() -> u32 {
    1
}

fn default_on_values() -> Vec<String> {
    vec!["on".to_owned(), "true".to_owned(), "alert".to_owned()]
}

/// What a widget renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum WidgetKind {
    /// One pushed number or string as a large figure.
    #[serde(rename = "value")]
    Value,
    /// A two-state indicator driven by `on_values`.
    #[serde(rename = "beacon")]
    Beacon,
    /// A pushed string, wrapped to the cell.
    #[serde(rename = "text")]
    Text,
    /// An entity's state read from Home Assistant rather than from a push.
    #[serde(rename = "ha_entity")]
    HaEntity,
}

/// A panel's colour capability. Configuration, not code, so that a mono,
/// 4-colour or 6-colour panel is a config change rather than a new render path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Palette {
    /// 16-level greyscale, e.g. a Kindle Paperwhite.
    #[serde(rename = "gray16")]
    Gray16,
    /// 4-level greyscale.
    #[serde(rename = "gray4")]
    Gray4,
    /// Pure black and white.
    #[serde(rename = "mono")]
    Mono,
    /// Black, white, red, yellow.
    #[serde(rename = "bwry")]
    Bwry,
    /// Six-colour Spectra.
    #[serde(rename = "spectra6")]
    Spectra6,
}

/// How the rasterised frame is reduced to the panel's palette.
///
/// A real operational decision, not cosmetic: error diffusion gives better tone
/// but makes pixels in unchanged regions differ between consecutive frames,
/// which defeats frame-hash comparison whenever any part of the image changes.
/// Ordered dithering is stateless per pixel and therefore stable frame to frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Dither {
    #[serde(rename = "atkinson")]
    Atkinson,
    #[serde(rename = "floyd-steinberg")]
    FloydSteinberg,
    #[serde(rename = "bayer")]
    Bayer,
    #[serde(rename = "none")]
    None,
}

/// Reads and validates the configuration file at `path`.
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    parse(&text).with_context(|| format!("in config file {}", path.display()))
}

/// Parses and validates configuration text.
///
/// This is the config seam: a pure function from TOML text to a validated
/// [`Config`].
pub fn parse(text: &str) -> Result<Config> {
    let file: File = toml::from_str(text).context("parsing TOML")?;

    let server = Server {
        public_base_url: validate_base_url(&file.server.public_base_url, "server.public_base_url")?,
        ..file.server
    };

    let home_assistant = match file.home_assistant {
        Some(ha) => Some(HomeAssistant {
            base_url: validate_base_url(&ha.base_url, "home_assistant.base_url")?,
            token: ha.token,
        }),
        None => None,
    };

    let mut devices = Vec::with_capacity(file.devices.len());
    for device in file.devices {
        devices.push(validate_device(device, home_assistant.is_some())?);
    }

    let mut seen = HashMap::new();
    for device in &devices {
        if seen.insert(device.id.as_str(), ()).is_some() {
            bail!("duplicate device id `{}`", device.id);
        }
    }

    Ok(Config {
        server,
        home_assistant,
        devices,
    })
}

/// Rejects a base URL the device could not reach, and strips any trailing slash.
///
/// A trailing slash is the single most likely cause of a silently blank panel:
/// both client families concatenate the base URL with the endpoint path without
/// normalising, so `http://host:4444/` yields `http://host:4444//api/display`.
fn validate_base_url(raw: &str, field: &str) -> Result<String> {
    let trimmed = raw.trim_end_matches('/');
    ensure!(!trimmed.is_empty(), "{field} must not be empty");

    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .with_context(|| format!("{field} must start with http:// or https://"))?;

    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = match authority.rsplit_once(':') {
        // Reject only a genuine `host:port` split, not the colons of an IPv6
        // literal.
        Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => authority,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    ensure!(!host.is_empty(), "{field} must include a host");

    if is_unreachable_host(host) {
        bail!(
            "{field} is `{raw}`, which the device cannot reach; \
             it must be a LAN or tailnet address, never localhost or a container-internal name"
        );
    }
    Ok(trimmed.to_owned())
}

/// Whether a host is one the panel could never resolve to this server.
fn is_unreachable_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    lower == "localhost"
        || lower.ends_with(".localhost")
        || lower == "::1"
        || lower == "0.0.0.0"
        || lower == "::"
        || lower.starts_with("127.")
}

fn validate_device(device: RawDevice, has_home_assistant: bool) -> Result<Device> {
    let RawDevice {
        id,
        width,
        height,
        palette,
        dither,
        refresh_rate,
        render_interval,
        grid,
        widgets,
    } = device;

    ensure!(!id.is_empty(), "device id must not be empty");
    ensure!(
        id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "device id `{id}` must contain only ASCII letters, digits, `-` or `_`, \
         because it is a URL path segment"
    );

    ensure!(
        (1..=MAX_DIMENSION).contains(&width) && (1..=MAX_DIMENSION).contains(&height),
        "device `{id}` has dimensions {width}x{height}, outside 1..={MAX_DIMENSION}"
    );

    ensure!(
        REFRESH_RATE_BOUNDS.contains(&refresh_rate),
        "device `{id}` has refresh_rate {refresh_rate}, outside {}..={}",
        REFRESH_RATE_BOUNDS.start(),
        REFRESH_RATE_BOUNDS.end()
    );

    // Rendering more often than the device polls is wasted work, so the default
    // is to rebuild exactly as often as it looks.
    let render_interval = render_interval.unwrap_or(refresh_rate);
    ensure!(
        RENDER_INTERVAL_BOUNDS.contains(&render_interval),
        "device `{id}` has render_interval {render_interval}, outside {}..={}",
        RENDER_INTERVAL_BOUNDS.start(),
        RENDER_INTERVAL_BOUNDS.end()
    );

    ensure!(
        grid.cols >= 1 && grid.rows >= 1,
        "device `{id}` has grid {}x{}; cols and rows must both be at least 1",
        grid.cols,
        grid.rows
    );

    let (cell_w, cell_h) = (width / grid.cols, height / grid.rows);
    ensure!(
        cell_w >= MIN_CELL && cell_h >= MIN_CELL,
        "device `{id}` is {width}x{height} over a {}x{} grid, giving {cell_w}x{cell_h} cells; \
         a cell must be at least {MIN_CELL}x{MIN_CELL} to render anything",
        grid.cols,
        grid.rows
    );

    validate_placement(&id, grid, &widgets)?;

    for widget in &widgets {
        if widget.kind == WidgetKind::HaEntity {
            ensure!(
                widget.entity.is_some(),
                "widget `{}` on device `{id}` has kind ha_entity but no `entity`",
                widget.id
            );
            ensure!(
                has_home_assistant,
                "widget `{}` on device `{id}` has kind ha_entity, \
                 which requires a [home_assistant] section with base_url and token",
                widget.id
            );
        }
    }

    Ok(Device {
        id,
        width,
        height,
        palette,
        dither,
        refresh_rate,
        render_interval,
        grid,
        widgets,
    })
}

/// Rejects a widget that leaves the grid, and any two widgets sharing a cell.
fn validate_placement(device_id: &str, grid: Grid, widgets: &[Widget]) -> Result<()> {
    let mut occupant: Vec<Option<&str>> = vec![None; (grid.cols * grid.rows) as usize];

    for widget in widgets {
        ensure!(
            widget.col_span >= 1 && widget.row_span >= 1,
            "widget `{}` on device `{device_id}` has a zero span; \
             col_span and row_span must both be at least 1",
            widget.id
        );

        let col_end = widget.col.saturating_add(widget.col_span);
        let row_end = widget.row.saturating_add(widget.row_span);
        ensure!(
            col_end <= grid.cols && row_end <= grid.rows,
            "widget `{}` on device `{device_id}` spans to column {col_end} row {row_end}, \
             outside its {}x{} grid",
            widget.id,
            grid.cols,
            grid.rows
        );

        for row in widget.row..row_end {
            for col in widget.col..col_end {
                let cell = &mut occupant[(row * grid.cols + col) as usize];
                if let Some(other) = cell {
                    bail!(
                        "widgets `{other}` and `{}` on device `{device_id}` \
                         both occupy grid cell col {col} row {row}",
                        widget.id
                    );
                }
                *cell = Some(&widget.id);
            }
        }
    }
    Ok(())
}

/// The TOML document's shape.
///
/// Distinct from [`Config`] only where the file is allowed to omit something the
/// rest of the program should not have to think about: `render_interval` is
/// optional here and resolved there. Using a sentinel instead would make an
/// explicit `render_interval = 0` silently mean "default" rather than the config
/// error it is.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    server: Server,
    home_assistant: Option<HomeAssistant>,
    #[serde(default, rename = "device")]
    devices: Vec<RawDevice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    id: String,
    width: u32,
    height: u32,
    palette: Palette,
    dither: Dither,
    refresh_rate: u32,
    render_interval: Option<u32>,
    grid: Grid,
    #[serde(default, rename = "widget")]
    widgets: Vec<Widget>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid document. Tests append to or rewrite parts of it so that
    /// each case differs from a known-good baseline by exactly one thing.
    const BASE: &str = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 1024
height = 758
palette = "gray16"
dither = "atkinson"
refresh_rate = 300
grid = { cols = 4, rows = 3 }
"#;

    fn err(text: &str) -> String {
        let error = parse(text).expect_err("expected this config to be rejected");
        // Validation context is attached with `with_context`, so the offending
        // detail can be on any link of the chain.
        error
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join(": ")
    }

    #[test]
    fn parses_a_minimal_document() {
        let config = parse(BASE).unwrap();
        assert_eq!(config.server.listen, "0.0.0.0:4444".parse().unwrap());
        assert_eq!(config.devices.len(), 1);
        let device = &config.devices[0];
        assert_eq!(device.id, "kindle");
        assert_eq!(device.palette, Palette::Gray16);
        assert_eq!(device.dither, Dither::Atkinson);
        assert_eq!(device.grid, Grid { cols: 4, rows: 3 });
    }

    #[test]
    fn parses_the_documented_widget_block() {
        let config = parse(&format!(
            r#"{BASE}
[[device.widget]]
id = "slack_unread"
kind = "beacon"
col = 0
row = 0
label = "Slack"
stale_after = 3600

[[device.widget]]
id = "office_temp"
kind = "value"
col = 1
row = 0
col_span = 2
label = "Office"
unit = "°C"
"#
        ))
        .unwrap();

        let widgets = &config.devices[0].widgets;
        assert_eq!(widgets.len(), 2);
        assert_eq!(widgets[0].kind, WidgetKind::Beacon);
        assert_eq!(widgets[0].stale_after, 3600);
        assert_eq!(widgets[0].col_span, 1, "col_span defaults to 1");
        assert_eq!(widgets[0].row_span, 1, "row_span defaults to 1");
        assert_eq!(
            widgets[0].on_values,
            ["on", "true", "alert"],
            "on_values has a documented default"
        );
        assert_eq!(widgets[1].col_span, 2);
        assert_eq!(widgets[1].unit.as_deref(), Some("°C"));
        assert_eq!(
            widgets[1].stale_after, 0,
            "an unspecified stale_after disables the staleness timer"
        );
    }

    #[test]
    fn render_interval_defaults_to_refresh_rate() {
        let config = parse(BASE).unwrap();
        let device = &config.devices[0];
        assert_eq!(device.refresh_rate, 300);
        assert_eq!(device.render_interval, 300);
    }

    #[test]
    fn render_interval_is_independent_when_given() {
        let config = parse(&BASE.replace(
            "refresh_rate = 300",
            "refresh_rate = 300\nrender_interval = 60",
        ))
        .unwrap();
        assert_eq!(config.devices[0].refresh_rate, 300);
        assert_eq!(config.devices[0].render_interval, 60);
    }

    #[test]
    fn rejects_refresh_rate_below_the_battery_guard() {
        let message = err(&BASE.replace("refresh_rate = 300", "refresh_rate = 29"));
        assert!(message.contains("refresh_rate 29"), "{message}");
        assert!(message.contains("kindle"), "{message}");
    }

    #[test]
    fn rejects_zero_refresh_rate() {
        let message = err(&BASE.replace("refresh_rate = 300", "refresh_rate = 0"));
        assert!(message.contains("refresh_rate 0"), "{message}");
    }

    #[test]
    fn rejects_refresh_rate_above_a_day() {
        let message = err(&BASE.replace("refresh_rate = 300", "refresh_rate = 86401"));
        assert!(message.contains("refresh_rate 86401"), "{message}");
    }

    #[test]
    fn accepts_refresh_rate_at_both_bounds() {
        for rate in [30, 86_400] {
            let text = BASE.replace("refresh_rate = 300", &format!("refresh_rate = {rate}"));
            assert_eq!(parse(&text).unwrap().devices[0].refresh_rate, rate);
        }
    }

    #[test]
    fn rejects_render_interval_out_of_range() {
        for interval in [0, 4, 86_401] {
            let text = BASE.replace(
                "refresh_rate = 300",
                &format!("refresh_rate = 300\nrender_interval = {interval}"),
            );
            let message = err(&text);
            assert!(
                message.contains(&format!("render_interval {interval}")),
                "{interval} should be rejected, got: {message}"
            );
        }
    }

    #[test]
    fn accepts_render_interval_at_both_bounds() {
        for interval in [5, 86_400] {
            let text = BASE.replace(
                "refresh_rate = 300",
                &format!("refresh_rate = 300\nrender_interval = {interval}"),
            );
            assert_eq!(parse(&text).unwrap().devices[0].render_interval, interval);
        }
    }

    #[test]
    fn rejects_a_localhost_public_base_url() {
        for host in [
            "http://localhost:4444",
            "http://127.0.0.1:4444",
            "http://[::1]:4444",
            "http://0.0.0.0:4444",
            "http://LocalHost:4444",
            "http://127.1.2.3",
        ] {
            let text = BASE.replace("http://192.168.0.50:4444", host);
            let message = err(&text);
            assert!(
                message.contains("public_base_url"),
                "{host} should be rejected, got: {message}"
            );
        }
    }

    #[test]
    fn accepts_a_lan_or_tailnet_public_base_url() {
        for host in [
            "http://192.168.0.50:4444",
            "http://10.1.2.3:4444",
            "http://panel.taile1234.ts.net",
            "https://panel.example.com",
        ] {
            let text = BASE.replace("http://192.168.0.50:4444", host);
            assert_eq!(parse(&text).unwrap().server.public_base_url, host);
        }
    }

    #[test]
    fn strips_a_trailing_slash_from_the_public_base_url() {
        let text = BASE.replace("http://192.168.0.50:4444", "http://192.168.0.50:4444/");
        assert_eq!(
            parse(&text).unwrap().server.public_base_url,
            "http://192.168.0.50:4444",
            "a trailing slash would produce a doubled path separator on the device"
        );
    }

    #[test]
    fn rejects_a_public_base_url_without_a_scheme() {
        let message = err(&BASE.replace("http://192.168.0.50:4444", "192.168.0.50:4444"));
        assert!(message.contains("http://"), "{message}");
    }

    #[test]
    fn rejects_overlapping_widgets_naming_both() {
        let message = err(&format!(
            r#"{BASE}
[[device.widget]]
id = "first"
kind = "text"
col = 0
row = 0
col_span = 2

[[device.widget]]
id = "second"
kind = "text"
col = 1
row = 0
"#
        ));
        assert!(message.contains("first"), "{message}");
        assert!(message.contains("second"), "{message}");
    }

    #[test]
    fn rejects_a_span_exceeding_the_grid() {
        let message = err(&format!(
            r#"{BASE}
[[device.widget]]
id = "too_wide"
kind = "text"
col = 2
row = 0
col_span = 3
"#
        ));
        assert!(message.contains("too_wide"), "{message}");
        assert!(message.contains("grid"), "{message}");
    }

    #[test]
    fn rejects_a_row_span_exceeding_the_grid() {
        let message = err(&format!(
            r#"{BASE}
[[device.widget]]
id = "too_tall"
kind = "text"
col = 0
row = 1
row_span = 3
"#
        ));
        assert!(message.contains("too_tall"), "{message}");
    }

    #[test]
    fn rejects_a_zero_span() {
        let message = err(&format!(
            r#"{BASE}
[[device.widget]]
id = "flat"
kind = "text"
col = 0
row = 0
col_span = 0
"#
        ));
        assert!(message.contains("flat"), "{message}");
    }

    #[test]
    fn accepts_widgets_that_tile_the_grid_exactly() {
        let mut text = BASE.to_owned();
        for row in 0..3 {
            for col in 0..4 {
                text.push_str(&format!(
                    "\n[[device.widget]]\nid = \"w{row}{col}\"\nkind = \"text\"\ncol = {col}\nrow = {row}\n"
                ));
            }
        }
        assert_eq!(parse(&text).unwrap().devices[0].widgets.len(), 12);
    }

    #[test]
    fn rejects_an_ha_entity_widget_without_home_assistant_config() {
        let message = err(&format!(
            r#"{BASE}
[[device.widget]]
id = "office_temp"
kind = "ha_entity"
col = 0
row = 0
entity = "sensor.office_temperature"
"#
        ));
        assert!(message.contains("office_temp"), "{message}");
        assert!(message.contains("home_assistant"), "{message}");
    }

    #[test]
    fn rejects_an_ha_entity_widget_without_an_entity() {
        let message = err(&format!(
            r#"{BASE}
[home_assistant]
base_url = "http://homeassistant.local:8123"
token = "tok"

[[device.widget]]
id = "office_temp"
kind = "ha_entity"
col = 0
row = 0
"#
        ));
        assert!(message.contains("office_temp"), "{message}");
        assert!(message.contains("entity"), "{message}");
    }

    #[test]
    fn accepts_an_ha_entity_widget_with_home_assistant_config() {
        let config = parse(&format!(
            r#"{BASE}
[home_assistant]
base_url = "http://homeassistant.local:8123/"
token = "tok"

[[device.widget]]
id = "office_temp"
kind = "ha_entity"
col = 0
row = 0
entity = "sensor.office_temperature"
"#
        ))
        .unwrap();
        let ha = config.home_assistant.unwrap();
        assert_eq!(ha.base_url, "http://homeassistant.local:8123");
        assert_eq!(
            config.devices[0].widgets[0].entity.as_deref(),
            Some("sensor.office_temperature")
        );
    }

    #[test]
    fn rejects_duplicate_device_ids() {
        let text = format!("{BASE}{}", &BASE[BASE.find("[[device]]").unwrap()..]);
        let message = err(&text);
        assert!(message.contains("duplicate device id"), "{message}");
        assert!(message.contains("kindle"), "{message}");
    }

    #[test]
    fn rejects_a_device_id_that_is_not_url_safe() {
        let message = err(&BASE.replace(r#"id = "kindle""#, r#"id = "kin/dle""#));
        assert!(message.contains("kin/dle"), "{message}");
    }

    #[test]
    fn rejects_an_unknown_key_so_a_typo_is_not_silently_ignored() {
        let message = err(&BASE.replace("dither = \"atkinson\"", "dithur = \"atkinson\""));
        assert!(message.contains("dithur"), "{message}");
    }

    #[test]
    fn rejects_an_unknown_palette() {
        let message = err(&BASE.replace(r#"palette = "gray16""#, r#"palette = "gray9""#));
        assert!(message.contains("gray9"), "{message}");
    }

    #[test]
    fn parses_every_documented_palette_and_dither() {
        for palette in ["gray16", "gray4", "mono", "bwry", "spectra6"] {
            for dither in ["atkinson", "floyd-steinberg", "bayer", "none"] {
                let text = BASE
                    .replace(
                        r#"palette = "gray16""#,
                        &format!(r#"palette = "{palette}""#),
                    )
                    .replace(r#"dither = "atkinson""#, &format!(r#"dither = "{dither}""#));
                parse(&text).unwrap_or_else(|e| panic!("{palette}/{dither} should parse: {e:#}"));
            }
        }
    }

    #[test]
    fn parses_every_documented_widget_kind() {
        for kind in ["value", "beacon", "text"] {
            let text = format!(
                "{BASE}\n[[device.widget]]\nid = \"w\"\nkind = \"{kind}\"\ncol = 0\nrow = 0\n"
            );
            assert_eq!(parse(&text).unwrap().devices[0].widgets.len(), 1);
        }
    }

    #[test]
    fn rejects_a_grid_too_dense_for_its_panel() {
        // Not cosmetic: cells this small make the text layout engine panic.
        let message = err(&BASE.replace(
            "grid = { cols = 4, rows = 3 }",
            "grid = { cols = 40, rows = 3 }",
        ));
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("cells"), "{message}");
    }

    #[test]
    fn accepts_a_grid_whose_cells_are_exactly_at_the_floor() {
        let text = BASE
            .replace("width = 1024", &format!("width = {}", MIN_CELL * 4))
            .replace("height = 758", &format!("height = {}", MIN_CELL * 3));
        assert_eq!(parse(&text).unwrap().devices[0].width, MIN_CELL * 4);
    }

    #[test]
    fn rejects_a_panel_smaller_than_one_cell() {
        let text = BASE
            .replace("width = 1024", "width = 1")
            .replace("height = 758", "height = 1")
            .replace(
                "grid = { cols = 4, rows = 3 }",
                "grid = { cols = 1, rows = 1 }",
            );
        let message = err(&text);
        assert!(message.contains("cells"), "{message}");
    }

    #[test]
    fn rejects_absurd_dimensions() {
        let message = err(&BASE.replace("width = 1024", "width = 100000"));
        assert!(message.contains("kindle"), "{message}");
    }

    #[test]
    fn rejects_a_zero_dimension() {
        let message = err(&BASE.replace("height = 758", "height = 0"));
        assert!(message.contains("kindle"), "{message}");
    }

    #[test]
    fn a_document_with_no_devices_is_valid() {
        let text = &BASE[..BASE.find("[[device]]").unwrap()];
        assert!(parse(text).unwrap().devices.is_empty());
    }

    #[test]
    fn content_path_has_a_default_and_is_overridable() {
        assert_eq!(
            parse(BASE).unwrap().server.content_path,
            "paneld-content.json"
        );
        let text = BASE.replace(
            "public_base_url =",
            "content_path = \"/var/lib/paneld/content.json\"\npublic_base_url =",
        );
        assert_eq!(
            parse(&text).unwrap().server.content_path,
            "/var/lib/paneld/content.json"
        );
    }
}
