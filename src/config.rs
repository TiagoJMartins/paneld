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
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};
use tz::TimeZoneRef;

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

/// Default ceiling on the encoded frame, in bytes.
///
/// A constraint of the original TRMNL ESP32 boards, which buffer the whole PNG
/// in RAM before decoding: without PSRAM, a frame much larger than this fails to
/// fetch. It is *not* a constraint of every BYOS client — a Kindle running
/// KOReader decodes arbitrary PNGs — which is why it is a per-device setting
/// rather than a constant, and why exceeding it warns rather than fails.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 90_000;

/// Smallest grid cell that can hold anything legible, in pixels.
///
/// Also a hard safety floor rather than a taste judgement: below roughly this
/// size a cell's content box stops being able to fit a single glyph, and the text
/// layout engine panics rather than returning an error.
pub const MIN_CELL: u32 = 40;

/// Smallest content box a cell may be left with, in pixels.
///
/// A hard safety floor like [`MIN_CELL`], and for the same reason: the text
/// layout engine is handed a cell's content box directly and panics on a
/// degenerate one. Configurable padding and border widths make that box something
/// an author can shrink, so the combination has to be rejected here rather than
/// discovered on the panel.
pub const MIN_CONTENT: u32 = 16;

/// Most decimal places a reading may be rounded to.
///
/// A wall panel is read from across a room, where the seventh decimal of a
/// temperature is noise competing with the number for space.
pub const MAX_PRECISION: u8 = 6;

/// Ceiling on any one of a device's `chrome` measurements, in pixels.
///
/// A guard rather than taste: spacing is subtracted from every cell, so a typo
/// with an extra digit is a dashboard with no cells rather than a wide one.
pub const MAX_CHROME: u32 = 64;

/// Ceiling on a grid's `cols` and `rows`.
///
/// Placement allocates one slot per cell, so an unbounded track count is an
/// allocation an author can ask for by typo. Far above any grid a panel can
/// actually show, which the [`MIN_CELL`] floor bounds much more tightly.
pub const MAX_TRACKS: u32 = 64;

/// Inclusive bounds on a status bar's thickness, in pixels.
pub const STATUS_BAR_THICKNESS_BOUNDS: std::ops::RangeInclusive<u32> = 12..=256;

/// A cell's rule, in pixels: the width the dashboard has always drawn.
const HAIRLINE: f32 = 1.0;

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
    /// Where fetched widget icons are cached. Relative paths resolve against the
    /// process working directory.
    ///
    /// A cache rather than a store: every entry is re-fetchable, so losing the
    /// directory costs one round trip per icon and nothing else. It exists so
    /// that rendering never reaches the network, which is what keeps a frame
    /// reproducible and keeps a dashboard drawable while the internet is down.
    #[serde(default = "default_icon_cache_path")]
    pub icon_cache_path: String,
}

fn default_content_path() -> String {
    "paneld-content.json".to_owned()
}

fn default_icon_cache_path() -> String {
    "paneld-icons".to_owned()
}

/// Home Assistant connection details, required by any `ha_entity` widget.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistant {
    /// e.g. `http://homeassistant.local:8123`. Stored without a trailing slash.
    pub base_url: String,
    /// Long-lived access token, written literally. Convenient for local use, but
    /// it puts a credential in the config file — prefer [`Self::token_env`]
    /// anywhere the config is version-controlled or mounted from a ConfigMap.
    pub token: Option<String>,
    /// Name of an environment variable to read the token from instead.
    ///
    /// Exactly one of `token` and `token_env` must be set. Resolved when the
    /// client is built rather than at parse time, so that parsing stays a pure
    /// function of the text.
    pub token_env: Option<String>,
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
    /// Warn when an encoded frame reaches this many bytes. `0` disables the
    /// check, which is right for a client that has real memory.
    pub max_frame_bytes: usize,
    pub grid: Grid,
    /// The dashboard's spacing and rules, resolved to pixels.
    pub chrome: Chrome,
    /// A strip along one edge, outside the widget grid. `None` gives the grid the
    /// whole frame.
    pub status_bar: Option<StatusBar>,
    pub widgets: Vec<Widget>,
}

impl Device {
    /// Every widget on this device, the children of every group included.
    ///
    /// Render prep walks this to collect the Home Assistant readings and the icons
    /// a dashboard needs. Walking `widgets` alone would resolve neither for a
    /// nested cell, which shows up as a group of cells all reading `no data`.
    pub fn all_widgets(&self) -> impl Iterator<Item = &Widget> {
        self.widgets.iter().flat_map(Widget::iter)
    }

    /// The rect the widget grid occupies, as `(x, y, width, height)` in frame
    /// pixels: the whole frame, less the strip a status bar took.
    ///
    /// Defined here rather than in the renderer because three things must agree
    /// about it — the layout, the tap hit test, and the validation that rejects a
    /// bar leaving cells too small to render. Two of those live in other modules,
    /// so the arithmetic is written once and read from there.
    pub fn grid_area(&self) -> (u32, u32, u32, u32) {
        grid_area(self.width, self.height, self.status_bar.as_ref())
    }

    /// The rect a status bar occupies, as `(x, y, width, height)` in frame pixels,
    /// or `None` on a device that has no bar.
    pub fn status_bar_area(&self) -> Option<(u32, u32, u32, u32)> {
        let bar = self.status_bar.as_ref()?;
        // Clamped, though validation already rejects a bar this thick: a rect that
        // wrapped around would be drawn somewhere unpredictable rather than not at
        // all.
        let thickness = bar.thickness.min(match bar.edge {
            Edge::Top | Edge::Bottom => self.height,
            Edge::Left | Edge::Right => self.width,
        });
        Some(match bar.edge {
            Edge::Top => (0, 0, self.width, thickness),
            Edge::Bottom => (0, self.height - thickness, self.width, thickness),
            Edge::Left => (0, 0, thickness, self.height),
            Edge::Right => (self.width - thickness, 0, thickness, self.height),
        })
    }
}

/// The widget grid's rect within a frame, as `(x, y, width, height)`.
///
/// A free function as well as [`Device::grid_area`] because validation needs the
/// answer while the [`Device`] is still being assembled.
fn grid_area(width: u32, height: u32, bar: Option<&StatusBar>) -> (u32, u32, u32, u32) {
    let Some(bar) = bar else {
        return (0, 0, width, height);
    };
    let thickness = bar.thickness;
    match bar.edge {
        Edge::Top => (0, thickness, width, height.saturating_sub(thickness)),
        Edge::Bottom => (0, 0, width, height.saturating_sub(thickness)),
        Edge::Left => (thickness, 0, width.saturating_sub(thickness), height),
        Edge::Right => (0, 0, width.saturating_sub(thickness), height),
    }
}

/// One grid cell's usable size, in pixels: what a `1fr` track resolves to once
/// the grid area's own margin and the gaps between tracks are taken out.
///
/// Shared by the renderer, the tap hit test and this module's own validation, so
/// that a cell's box, the box a finger is resolved against, and the box validation
/// held to a floor are all one box.
pub fn cell_size(area_w: u32, area_h: u32, grid: Grid, chrome: Chrome) -> (f32, f32) {
    let cols = grid.cols.max(1) as f32;
    let rows = grid.rows.max(1) as f32;
    (
        (area_w as f32 - chrome.gap * (cols + 1.0)).max(1.0) / cols,
        (area_h as f32 - chrome.gap * (rows + 1.0)).max(1.0) / rows,
    )
}

/// One sub-cell of a group's grid, in pixels, given the group's content box.
///
/// The children fill that box with one [`Chrome::gap`] between them and no margin
/// of their own — the group's padding already is that margin — so `n` tracks want
/// `n - 1` gaps where the outer grid wants `n + 1`.
pub fn sub_cell_size(box_w: f32, box_h: f32, grid: Grid, chrome: Chrome) -> (f32, f32) {
    let cols = grid.cols.max(1) as f32;
    let rows = grid.rows.max(1) as f32;
    (
        (box_w - chrome.gap * (cols - 1.0)).max(1.0) / cols,
        (box_h - chrome.gap * (rows - 1.0)).max(1.0) / rows,
    )
}

/// A dashboard's spacing and rules, in pixels.
///
/// Resolved to concrete pixel counts here rather than derived in the renderer, so
/// that the layout, the hit test and validation read the same three numbers.
/// Validation is why that matters: the layout engine panics on a degenerate
/// content box, so a padding-and-border combination that would produce one has to
/// be rejected before it reaches the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chrome {
    /// Between cells, and around the grid area.
    pub gap: f32,
    /// Inside a cell, between its rule and its content.
    pub padding: f32,
    /// A cell's rule. `0.0` draws no frame at all, which is what a dashboard of
    /// bare readings wants.
    pub border: f32,
}

impl Chrome {
    /// The spacing a `grid` over a `width` x `height` area gets when the author
    /// says nothing.
    ///
    /// Scaled to the cell rather than fixed. Fixed spacing is a trap on a small
    /// panel or a dense grid: once padding exceeds the cell it is inside, the text
    /// layout engine is handed a negative content box and panics.
    pub fn derived(width: u32, height: u32, grid: Grid) -> Self {
        let smallest_side =
            (width as f32 / grid.cols.max(1) as f32).min(height as f32 / grid.rows.max(1) as f32);
        Self {
            gap: (smallest_side * 0.06).clamp(1.0, 10.0),
            // A tighter ceiling than the gap's, because padding is charged twice —
            // once on each side — and it comes straight off the width a figure has
            // to fit in. The rule and the gap already separate two cells; a wider
            // inset only shrinks the number inside one.
            padding: (smallest_side * 0.10).clamp(2.0, 8.0),
            border: HAIRLINE,
        }
    }

    /// What a cell's content box loses to its own chrome, on each axis.
    pub fn inset(&self) -> f32 {
        (self.padding + self.border) * 2.0
    }
}

/// A strip along one edge of the frame, outside the widget grid.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusBar {
    pub edge: Edge,
    /// Height on a horizontal edge, width on a vertical one, in pixels.
    pub thickness: u32,
    /// What the bar says, in the order written.
    pub fields: Vec<StatusField>,
    /// The zone [`StatusField::Date`] and [`StatusField::Time`] are rendered in.
    pub timezone: Timezone,
}

/// A time zone, resolved from its IANA name when the configuration loaded.
///
/// A real zone and not an offset, because an offset is a fact about a moment and
/// a wall panel outlives the moment: a panel told `+01:00` in August is an hour
/// wrong from the last Sunday in October until spring, and wrong quietly, which is
/// the worst way for a clock to be wrong. The database is compiled into the binary
/// (see `Cargo.toml`), so this needs nothing installed and reads nothing at render
/// time.
#[derive(Debug, Clone, PartialEq)]
pub struct Timezone {
    /// The name the file wrote.
    ///
    /// Kept alongside the handle because the handle cannot answer it: `Portugal`
    /// and `Europe/Lisbon` resolve to the same transition data and so compare
    /// equal, and lookup is case-insensitive, so the only record of what an author
    /// actually asked for is this string.
    name: String,
    zone: TimeZoneRef<'static>,
}

impl Timezone {
    /// UTC, which is what a bar gets when its author names no zone.
    pub fn utc() -> Self {
        Self {
            name: "UTC".to_owned(),
            zone: TimeZoneRef::utc(),
        }
    }

    /// Resolves an IANA name — `Europe/Lisbon`, `America/New_York`, `UTC` —
    /// against the compiled-in database.
    ///
    /// An unknown name is an error naming the database version, because the two
    /// ways to get here are a typo and a zone that only exists in a release newer
    /// than the one this binary was built against, and those want different fixes.
    pub fn parse(name: &str) -> Result<Self> {
        let name = name.trim();
        ensure!(
            !name.is_empty(),
            "a time zone must be named, as an IANA zone like `Europe/Lisbon`"
        );
        let zone = tzdb::tz_by_name(name).with_context(|| {
            format!(
                "`{name}` is not a zone in the IANA {} database compiled into this binary",
                tzdb::VERSION
            )
        })?;
        Ok(Self {
            name: name.to_owned(),
            zone,
        })
    }

    /// The IANA name this was resolved from.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `instant`, as the wall time this zone was reading at that moment.
    ///
    /// Degrades to UTC rather than failing, because this runs inside a render and a
    /// clock an hour wrong beats no frame at all. Both ways it can fail are
    /// exotic: an instant past the last transition of a zone that carries no POSIX
    /// rule, and an offset beyond ±24 hours, which no real zone has.
    pub fn at(&self, instant: OffsetDateTime) -> OffsetDateTime {
        let offset = self
            .zone
            .find_local_time_type(instant.unix_timestamp())
            .ok()
            .and_then(|local| UtcOffset::from_whole_seconds(local.ut_offset()).ok())
            .unwrap_or(UtcOffset::UTC);
        instant.to_offset(offset)
    }
}

/// The IANA release the time zone database compiled into this binary came from.
///
/// Surfaced because the database is frozen at build time and nothing announces
/// that: a zone whose rules changed after this release is rendered by the old
/// rules, silently, until someone rebuilds.
pub const TZDATA_VERSION: &str = tzdb::VERSION;

/// Which edge of the frame a status bar takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Edge {
    #[serde(rename = "top")]
    Top,
    #[serde(rename = "bottom")]
    Bottom,
    #[serde(rename = "left")]
    Left,
    #[serde(rename = "right")]
    Right,
}

impl std::fmt::Display for Edge {
    /// The spelling the config file uses, so an error names what the author wrote.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Right => "right",
        })
    }
}

/// One thing a status bar can show.
///
/// A closed set rather than a format string. Every field here is something this
/// process already knows for certain; a template language would invite naming
/// things it does not have, and a wall panel is a poor place to discover that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum StatusField {
    /// The date, in the bar's own offset.
    #[serde(rename = "date")]
    Date,
    /// The time of day, in the bar's own offset.
    ///
    /// Know what a clock costs before configuring one: it makes every frame differ
    /// from the last, so the panel repaints on every render interval rather than
    /// only when a reading changed.
    #[serde(rename = "time")]
    Time,
    /// The device's last reported battery percentage.
    #[serde(rename = "battery")]
    Battery,
    /// How often the device is told to poll, as a period.
    #[serde(rename = "refresh")]
    Refresh,
    /// The device id, for a household running more than one panel.
    #[serde(rename = "device")]
    Device,
    /// Signal strength as the device reported it. Worth knowing before
    /// configuring it: the KOReader client in service hardcodes `rssi` to 0.
    #[serde(rename = "signal")]
    Signal,
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
///
/// A *validated* widget: [`RawWidget`] is the file's shape, and everything the
/// rest of the program should not have to re-decide — chiefly a `tap`, which the
/// file may spell three ways — is resolved on the way here.
#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    /// Also the content push address: `PUT /api/content/<id>`. Unique across the
    /// whole device, group children included, precisely because of that.
    pub id: String,
    pub kind: WidgetKind,
    pub col: u32,
    pub row: u32,
    pub col_span: u32,
    pub row_span: u32,
    pub label: Option<String>,
    pub unit: Option<String>,
    /// Decimal places a numeric reading is rounded to. `None` renders whatever the
    /// source said, digit for digit.
    ///
    /// Already folded with the device's default, so the renderer reads this and
    /// nothing else.
    pub precision: Option<u8>,
    /// Whether a graphic reading is captioned in words: a beacon's `ON`/`OFF`, a
    /// weather cell's condition.
    ///
    /// The graphic is the reading and the word only confirms it, so on a dense
    /// dashboard the word is the part worth dropping — which is why it is a switch
    /// rather than a fixed part of those two bodies.
    pub state_text: bool,
    /// How long pushed content stays fresh, in seconds. `0` disables the
    /// staleness timer, which is the default: a widget should not start
    /// reporting itself stale just because its author never thought about it.
    pub stale_after: u64,
    /// Home Assistant entity id, for `kind = "ha_entity"` and `kind = "weather"`,
    /// and the fallback source for any [`Reading`] that names none.
    pub entity: Option<String>,
    /// Read this attribute of the entity instead of its own state.
    ///
    /// Needed more often than it sounds: a `weather.*` entity's state is a
    /// condition like `partlycloudy`, and the temperature you actually want to
    /// show is an attribute.
    pub attribute: Option<String>,
    /// Values that put a `beacon` in its "on" state.
    pub on_values: Vec<String>,
    /// An icon drawn beside the cell's label, spelt the way
    /// [gethomepage](https://gethomepage.dev) spells one. See [`crate::icon`].
    pub icon: Option<String>,
    /// A `beacon`'s indicator while it reads on, drawn in place of the dot.
    pub icon_on: Option<String>,
    /// A `beacon`'s indicator while it reads off, drawn in place of the dot.
    pub icon_off: Option<String>,
    /// What a `list` cell is made of, and what a `weather` cell hangs off its
    /// condition. Empty for every other kind.
    pub readings: Vec<Reading>,
    /// The sub-grid and children of a `group`, and `None` for every other kind.
    pub group: Option<Group>,
    /// What tapping this cell does. See [`crate::tap`].
    pub tap: Option<Tap>,
}

impl Widget {
    /// This widget, and every child when it is a group.
    ///
    /// Flat rather than recursive because a group is one level deep by
    /// construction: [`validate_widget`] rejects a group inside a group.
    pub fn iter(&self) -> impl Iterator<Item = &Widget> {
        std::iter::once(self).chain(self.group.iter().flat_map(|group| group.widgets.iter()))
    }
}

/// One labelled value inside a multi-reading cell.
///
/// Shared by `list`, whose whole body is these, and by `weather`, which hangs them
/// off the condition it already reads. One type because they are one thing: a
/// Home Assistant value with a label, a unit and a precision of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// What the row is called. A row without one is just its value.
    pub label: Option<String>,
    /// Resolved: the reading's own `entity`, or the widget's when it named none.
    pub entity: String,
    /// Read this attribute instead of the entity's state. How a weather cell gets
    /// at the `temperature` and `humidity` that its condition does not carry.
    pub attribute: Option<String>,
    pub unit: Option<String>,
    /// Decimal places, already folded with the widget's and the device's.
    pub precision: Option<u8>,
}

/// A widget that is itself a grid of widgets.
///
/// One level deep, deliberately: a group inside a group is rejected. Arbitrary
/// nesting would make placement, geometry and tap resolution recursive to serve a
/// composition nobody asked for, when two levels is the whole ask — several small
/// readings sharing one slot of the outer grid.
///
/// The children are laid out inside the group's *content box*, with one
/// [`Chrome::gap`] between them and no outer margin of their own, because the
/// group's own padding already is that margin. Pinned here because the renderer,
/// the hit test and the validation that rejects an unreadably dense group all have
/// to agree on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The sub-grid the children are placed on, local to the group's own rect.
    pub grid: Grid,
    pub widgets: Vec<Widget>,
}

/// What tapping a widget's cell does.
///
/// Deliberately two verbs rather than a general grammar. paneld has no pages and
/// no rotations, so the navigation verbs a browser-rendered dashboard needs have
/// no meaning here, and a webhook verb would be an outbound-request surface
/// nobody asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Tap {
    /// Rebuild this device's frame now. Useful on its own cell as a manual
    /// "refresh the panel", and the only action that reaches nothing outside
    /// this process.
    Refresh,
    /// Call a Home Assistant service.
    Service(ServiceCall),
}

/// A Home Assistant service call, resolved from a widget's `tap`.
///
/// `data` already carries `entity_id` when there is a target, so dispatching is
/// a `POST` of this struct's `data` to `/api/services/{domain}/{service}` with
/// nothing left to decide.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCall {
    /// e.g. `light`.
    pub domain: String,
    /// e.g. `toggle`.
    pub service: String,
    /// The service data, as posted.
    pub data: serde_json::Map<String, serde_json::Value>,
}

impl std::fmt::Display for ServiceCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.domain, self.service)
    }
}

fn one() -> u32 {
    1
}

fn yes() -> bool {
    true
}

fn default_max_frame_bytes() -> usize {
    DEFAULT_MAX_FRAME_BYTES
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
    /// A `weather.*` entity's condition, as an icon and a named condition,
    /// optionally with readings from the same entity beside it.
    ///
    /// Its own kind rather than an `ha_entity` that sniffs the entity id,
    /// because the two render nothing alike: a condition is a word from a closed
    /// set that wants a picture, not a figure in tabular numerals.
    #[serde(rename = "weather")]
    Weather,
    /// Several readings as labelled rows in one cell.
    ///
    /// Distinct from a `group` and not a special case of one: a list is one
    /// presentation of `n` values, sized and aligned together, where a group is
    /// `n` independent widgets that happen to share a slot.
    #[serde(rename = "list")]
    List,
    /// A sub-grid of widgets sharing one slot of the outer grid.
    #[serde(rename = "group")]
    Group,
}

impl std::fmt::Display for WidgetKind {
    /// The spelling the config file uses, so an error names what the author wrote.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Value => "value",
            Self::Beacon => "beacon",
            Self::Text => "text",
            Self::HaEntity => "ha_entity",
            Self::Weather => "weather",
            Self::List => "list",
            Self::Group => "group",
        })
    }
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

/// One configuration document: its text, and the name an error should blame.
#[derive(Debug, Clone, Copy)]
pub struct Document<'a> {
    /// How the document is named in an error — a path, or a test's label.
    pub name: &'a str,
    pub text: &'a str,
}

/// Reads and validates the configuration at `path`, together with every fragment
/// in its drop-in directory.
pub fn load(path: &Path) -> Result<Config> {
    let sources = sources(path);
    let mut texts = Vec::with_capacity(sources.len());
    for source in &sources {
        texts.push(
            std::fs::read_to_string(source)
                .with_context(|| format!("reading config file {}", source.display()))?,
        );
    }

    let names: Vec<String> = sources
        .iter()
        .map(|source| source.display().to_string())
        .collect();
    let documents: Vec<Document<'_>> = names
        .iter()
        .zip(&texts)
        .map(|(name, text)| Document { name, text })
        .collect();
    parse_documents(&documents)
}

/// Every file [`load`] reads, in load order: the main file, then its fragments.
pub fn sources(path: &Path) -> Vec<PathBuf> {
    let mut sources = vec![path.to_path_buf()];
    sources.extend(fragments(path));
    sources
}

/// The drop-in directory for `path`.
///
/// `<file name>.d` beside the main file, after the convention systemd made
/// familiar — `paneld.toml` reads `paneld.toml.d/`. Derived from the path rather
/// than configured, so pointing `--config` somewhere else moves the fragments
/// with it instead of leaving them behind.
pub fn fragment_dir(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".d");
    path.with_file_name(name)
}

/// The fragments in `path`'s drop-in directory, in load order.
///
/// Sorted by file name, so a merge conflict is reported the same way twice
/// running rather than in whatever order the filesystem happened to answer in.
/// Top-level `*.toml` only: a subdirectory, a dotfile and an editor's
/// `paneld.toml.swp` all live in a config directory without being configuration.
fn fragments(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(fragment_dir(path)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().and_then(|name| name.to_str());
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
                && name.is_some_and(|name| !name.starts_with('.'))
        })
        .collect();
    files.sort();
    files
}

/// The most recent modification time across everything [`load`] reads.
///
/// The drop-in directory's own timestamp is included, and that is the point of
/// this function rather than an incidental extra: adding or deleting a fragment
/// modifies no file, so a check that only stats files sees nothing and the panel
/// keeps rendering a configuration that no longer exists on disk.
pub fn modified_at(path: &Path) -> Option<SystemTime> {
    std::iter::once(path.to_path_buf())
        .chain(std::iter::once(fragment_dir(path)))
        .chain(fragments(path))
        .filter_map(|path| std::fs::metadata(path).ok()?.modified().ok())
        .max()
}

/// Parses and validates one configuration document.
///
/// The single-document spelling of [`parse_documents`], because most
/// configurations are one file and nothing that does not care about merging
/// should have to say so.
pub fn parse(text: &str) -> Result<Config> {
    parse_documents(&[Document {
        name: "config",
        text,
    }])
}

/// Parses and validates a configuration spread across several documents.
///
/// This is the config seam: a pure function from named TOML texts to a validated
/// [`Config`], so every merge rule below is asserted without touching a
/// filesystem.
///
/// Merging is deliberately blunt: **each section is declared exactly once, in
/// whichever document its author likes.** `[server]` and `[home_assistant]` may
/// appear in one document only; a device id or a dashboard name may be declared
/// once. Nothing is deep-merged and nothing overrides anything, because a
/// fragment that silently replaced part of another file's device would produce a
/// dashboard that no single file explains.
pub fn parse_documents(documents: &[Document<'_>]) -> Result<Config> {
    let mut server: Option<(&str, Server)> = None;
    let mut home_assistant: Option<(&str, HomeAssistant)> = None;
    let mut dashboards: Vec<(&str, RawDashboard)> = Vec::new();
    let mut devices: Vec<(&str, RawDevice)> = Vec::new();

    for document in documents {
        let file: File = toml::from_str(document.text)
            .with_context(|| format!("parsing TOML in {}", document.name))?;

        if let Some(incoming) = file.server {
            if let Some((first, _)) = &server {
                bail!(
                    "[server] is declared in both {first} and {}; it belongs to exactly \
                     one document",
                    document.name
                );
            }
            server = Some((document.name, incoming));
        }
        if let Some(incoming) = file.home_assistant {
            if let Some((first, _)) = &home_assistant {
                bail!(
                    "[home_assistant] is declared in both {first} and {}; it belongs to \
                     exactly one document",
                    document.name
                );
            }
            home_assistant = Some((document.name, incoming));
        }
        dashboards.extend(
            file.dashboards
                .into_iter()
                .map(|dashboard| (document.name, dashboard)),
        );
        devices.extend(
            file.devices
                .into_iter()
                .map(|device| (document.name, device)),
        );
    }

    let (_, server) = server
        .context("no [server] section: one document must declare `listen` and `public_base_url`")?;
    let server = Server {
        public_base_url: validate_base_url(
            &server.public_base_url,
            "server.public_base_url",
            true,
        )?,
        ..server
    };

    let home_assistant = match home_assistant {
        Some((_, ha)) => {
            ensure!(
                ha.token.is_some() != ha.token_env.is_some(),
                "home_assistant needs exactly one of `token` or `token_env`, not both and not neither"
            );
            if let Some(name) = &ha.token_env {
                ensure!(
                    !name.is_empty(),
                    "home_assistant.token_env must name an environment variable"
                );
            }
            Some(HomeAssistant {
                base_url: validate_base_url(&ha.base_url, "home_assistant.base_url", false)?,
                token: ha.token,
                token_env: ha.token_env,
            })
        }
        None => None,
    };

    let mut by_name: HashMap<String, (&str, RawDashboard)> = HashMap::new();
    for (source, dashboard) in dashboards {
        if let Some((first, _)) = by_name.get(&dashboard.name) {
            bail!(
                "dashboard `{}` is declared in both {first} and {source}",
                dashboard.name
            );
        }
        by_name.insert(dashboard.name.clone(), (source, dashboard));
    }
    let dashboards: HashMap<String, RawDashboard> = by_name
        .into_iter()
        .map(|(name, (_, dashboard))| (name, dashboard))
        .collect();

    // Checked before any device is validated, so a duplicate id is reported as the
    // merge problem it is rather than as whichever field of the second copy
    // happened to fail first.
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for (source, device) in &devices {
        if let Some(first) = seen.get(device.id.as_str()) {
            bail!(
                "device `{}` is declared in both {first} and {source}",
                device.id
            );
        }
        seen.insert(device.id.as_str(), source);
    }

    let mut validated = Vec::with_capacity(devices.len());
    for (_, device) in devices {
        validated.push(validate_device(
            device,
            &dashboards,
            home_assistant.is_some(),
        )?);
    }

    Ok(Config {
        server,
        home_assistant,
        devices: validated,
    })
}

/// Normalises a base URL, and says whether the panel has to be able to reach it.
///
/// A trailing slash is the single most likely cause of a silently blank panel:
/// both client families concatenate the base URL with the endpoint path without
/// normalising, so `http://host:4444/` yields `http://host:4444//api/display`.
/// That applies to any base URL here, so it is stripped from all of them.
///
/// `reachable_by_panel` is what differs, and conflating the two was a real defect.
/// The panel dials `public_base_url` itself, so a loopback there is a dashboard
/// nobody ever sees. Home Assistant is dialled by *this process*, so loopback and
/// container-internal names are not merely allowed but ordinary — a sidecar, a
/// tunnel, or Home Assistant on the same host.
fn validate_base_url(raw: &str, field: &str, reachable_by_panel: bool) -> Result<String> {
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

    if reachable_by_panel && is_unreachable_host(host) {
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

fn validate_device(
    device: RawDevice,
    dashboards: &HashMap<String, RawDashboard>,
    has_home_assistant: bool,
) -> Result<Device> {
    let RawDevice {
        id,
        width,
        height,
        palette,
        dither,
        refresh_rate,
        render_interval,
        max_frame_bytes,
        precision,
        grid,
        chrome,
        status_bar,
        dashboard,
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

    if let Some(precision) = precision {
        ensure!(
            precision <= MAX_PRECISION,
            "device `{id}` has precision {precision}, above the {MAX_PRECISION} decimal \
             places a panel can use"
        );
    }

    // A dashboard is adopted whole. It brings the grid its widgets were laid out
    // on, so taking one and overriding the other would place someone else's
    // widgets on a grid their author never saw.
    let (grid, raw_widgets, adopted) = match dashboard {
        Some(name) => {
            ensure!(
                grid.is_none() && widgets.is_empty(),
                "device `{id}` adopts dashboard `{name}` and also declares a `grid` or \
                 widgets of its own; a dashboard brings both, so drop one or the other"
            );
            let dashboard = dashboards.get(&name).with_context(|| {
                format!("device `{id}` adopts dashboard `{name}`, which nothing declares")
            })?;
            (dashboard.grid, dashboard.widgets.clone(), Some(name))
        }
        None => {
            let grid = grid.with_context(|| {
                format!("device `{id}` declares neither a `grid` nor a `dashboard` to adopt")
            })?;
            (grid, widgets, None)
        }
    };
    validate_grid(grid, &format!("device `{id}`"))?;

    let status_bar = match status_bar {
        Some(raw) => Some(validate_status_bar(raw, &id, width, height)?),
        None => None,
    };

    // Everything below is measured against the area the grid actually gets, which
    // is the frame less whatever a status bar took off an edge.
    let (_, _, area_w, area_h) = grid_area(width, height, status_bar.as_ref());
    let chrome = validate_chrome(chrome, &id, area_w, area_h, grid)?;
    let (cell_w, cell_h) = cell_size(area_w, area_h, grid, chrome);

    ensure!(
        cell_w >= MIN_CELL as f32 && cell_h >= MIN_CELL as f32,
        "device `{id}` gives its grid {area_w}x{area_h} pixels over {}x{} cells with a \
         {} pixel gap, leaving {cell_w:.0}x{cell_h:.0} cells; a cell must be at least \
         {MIN_CELL}x{MIN_CELL} to render anything",
        grid.cols,
        grid.rows,
        chrome.gap
    );
    ensure!(
        cell_w - chrome.inset() >= MIN_CONTENT as f32
            && cell_h - chrome.inset() >= MIN_CONTENT as f32,
        "device `{id}` has {} pixels of padding and a {} pixel border on \
         {cell_w:.0}x{cell_h:.0} cells, leaving a {:.0}x{:.0} content box; at least \
         {MIN_CONTENT}x{MIN_CONTENT} is needed to render anything",
        chrome.padding,
        chrome.border,
        cell_w - chrome.inset(),
        cell_h - chrome.inset()
    );

    let widgets = raw_widgets
        .into_iter()
        .map(|widget| validate_widget(widget, &id, precision, has_home_assistant))
        .collect::<Result<Vec<_>>>();
    // A dashboard's errors are reported against the device that adopted it: the
    // author's next move is to fix the dashboard, but the panel that went blank is
    // this one, and the message has to connect the two.
    let widgets = match &adopted {
        Some(name) => {
            widgets.with_context(|| format!("in dashboard `{name}`, adopted by device `{id}`"))?
        }
        None => widgets?,
    };

    validate_placement(&id, None, grid, &widgets)?;
    validate_ids(&id, &widgets)?;
    validate_group_geometry(&id, chrome, cell_w, cell_h, &widgets)?;

    Ok(Device {
        id,
        width,
        height,
        palette,
        dither,
        refresh_rate,
        render_interval,
        max_frame_bytes,
        grid,
        chrome,
        status_bar,
        widgets,
    })
}

/// Rejects a grid that cannot be laid out, or that is too large to be one.
fn validate_grid(grid: Grid, what: &str) -> Result<()> {
    ensure!(
        grid.cols >= 1 && grid.rows >= 1,
        "{what} has grid {}x{}; cols and rows must both be at least 1",
        grid.cols,
        grid.rows
    );
    ensure!(
        grid.cols <= MAX_TRACKS && grid.rows <= MAX_TRACKS,
        "{what} has grid {}x{}, above the {MAX_TRACKS} track ceiling",
        grid.cols,
        grid.rows
    );
    Ok(())
}

/// Resolves a device's spacing, filling in whatever the author left out.
fn validate_chrome(
    raw: Option<RawChrome>,
    device_id: &str,
    area_w: u32,
    area_h: u32,
    grid: Grid,
) -> Result<Chrome> {
    let derived = Chrome::derived(area_w, area_h, grid);
    let Some(raw) = raw else {
        return Ok(derived);
    };

    for (field, value) in [
        ("gap", raw.gap),
        ("padding", raw.padding),
        ("border", raw.border),
    ] {
        if let Some(value) = value {
            ensure!(
                value <= MAX_CHROME,
                "device `{device_id}` has chrome.{field} {value}, above the {MAX_CHROME} \
                 pixel ceiling"
            );
        }
    }

    Ok(Chrome {
        gap: raw.gap.map_or(derived.gap, |value| value as f32),
        padding: raw.padding.map_or(derived.padding, |value| value as f32),
        border: raw.border.map_or(derived.border, |value| value as f32),
    })
}

/// Resolves a device's status bar, and rejects one that leaves no dashboard.
fn validate_status_bar(
    raw: RawStatusBar,
    device_id: &str,
    width: u32,
    height: u32,
) -> Result<StatusBar> {
    let RawStatusBar {
        edge,
        thickness,
        fields,
        timezone,
    } = raw;

    ensure!(
        !fields.is_empty(),
        "device `{device_id}` has a status_bar with no `fields`; a bar with nothing in it \
         is a strip of panel that nothing can use"
    );

    let across = match edge {
        Edge::Top | Edge::Bottom => height,
        Edge::Left | Edge::Right => width,
    };
    let thickness = thickness.unwrap_or_else(|| default_thickness(width, height));
    ensure!(
        STATUS_BAR_THICKNESS_BOUNDS.contains(&thickness),
        "device `{device_id}` has a status_bar {thickness} pixels thick, outside {}..={}",
        STATUS_BAR_THICKNESS_BOUNDS.start(),
        STATUS_BAR_THICKNESS_BOUNDS.end()
    );
    ensure!(
        thickness + MIN_CELL < across,
        "device `{device_id}` has a {thickness} pixel status_bar on its {edge} edge, \
         leaving {} of that axis's {across} pixels for the widget grid",
        across.saturating_sub(thickness)
    );

    let timezone = match &timezone {
        Some(name) => Timezone::parse(name)
            .with_context(|| format!("device `{device_id}` has an invalid status_bar timezone"))?,
        None => Timezone::utc(),
    };

    Ok(StatusBar {
        edge,
        thickness,
        fields,
        timezone,
    })
}

/// A status bar's thickness when the author does not say, in pixels.
///
/// A twentieth of the frame's short side, bounded: enough for one line of chrome
/// at the size the rest of the dashboard's chrome is set at, and never so much of
/// a small panel that the dashboard is squeezed to make room for the clock.
fn default_thickness(width: u32, height: u32) -> u32 {
    ((width.min(height) as f32 * 0.05).round() as u32).clamp(18, 64)
}

/// Rejects a widget that leaves its grid, and any two widgets sharing a cell.
///
/// Applied to the device's own grid, and again to each group's sub-grid: nested
/// placement is the same rule against a smaller grid, and `group` says which grid
/// an error is about.
fn validate_placement(
    device_id: &str,
    group: Option<&str>,
    grid: Grid,
    widgets: &[Widget],
) -> Result<()> {
    let scope = match group {
        Some(group) => format!("in group `{group}` on device `{device_id}`"),
        None => format!("on device `{device_id}`"),
    };
    let mut occupant: Vec<Option<&str>> = vec![None; (grid.cols * grid.rows) as usize];

    for widget in widgets {
        ensure!(
            widget.col_span >= 1 && widget.row_span >= 1,
            "widget `{}` {scope} has a zero span; \
             col_span and row_span must both be at least 1",
            widget.id
        );

        let col_end = widget.col.saturating_add(widget.col_span);
        let row_end = widget.row.saturating_add(widget.row_span);
        ensure!(
            col_end <= grid.cols && row_end <= grid.rows,
            "widget `{}` {scope} spans to column {col_end} row {row_end}, \
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
                        "widgets `{other}` and `{}` {scope} \
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

/// Rejects two widgets on one device sharing an id.
///
/// Across groups as well as beside them, because an id is a content push address:
/// two widgets answering to `PUT /api/content/office_temp` means one publisher
/// feeds a cell nobody chose.
fn validate_ids(device_id: &str, widgets: &[Widget]) -> Result<()> {
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for widget in widgets.iter().flat_map(Widget::iter) {
        ensure!(
            !widget.id.is_empty(),
            "a widget on device `{device_id}` has an empty id"
        );
        if seen.insert(widget.id.as_str(), ()).is_some() {
            bail!(
                "two widgets on device `{device_id}` share the id `{}`, which is also the \
                 address `PUT /api/content/{}` writes to",
                widget.id,
                widget.id
            );
        }
    }
    Ok(())
}

/// Rejects a group whose sub-grid leaves its children nothing to render in.
///
/// The floor the device's own cells are held to, one level down. Without it a
/// group is the one place an author can still ask for a two-pixel content box, and
/// the layout engine answers that with a panic rather than an error.
fn validate_group_geometry(
    device_id: &str,
    chrome: Chrome,
    cell_w: f32,
    cell_h: f32,
    widgets: &[Widget],
) -> Result<()> {
    for widget in widgets {
        let Some(group) = &widget.group else {
            continue;
        };
        // The group's own content box, which is what its children share.
        let box_w = cell_w * widget.col_span as f32
            + chrome.gap * widget.col_span.saturating_sub(1) as f32
            - chrome.inset();
        let box_h = cell_h * widget.row_span as f32
            + chrome.gap * widget.row_span.saturating_sub(1) as f32
            - chrome.inset();
        let (sub_w, sub_h) = sub_cell_size(box_w, box_h, group.grid, chrome);
        ensure!(
            sub_w - chrome.inset() >= MIN_CONTENT as f32
                && sub_h - chrome.inset() >= MIN_CONTENT as f32,
            "group `{}` on device `{device_id}` is {box_w:.0}x{box_h:.0} pixels over a \
             {}x{} sub-grid, leaving each child a {:.0}x{:.0} content box; at least \
             {MIN_CONTENT}x{MIN_CONTENT} is needed to render anything",
            widget.id,
            group.grid.cols,
            group.grid.rows,
            sub_w - chrome.inset(),
            sub_h - chrome.inset()
        );
    }
    Ok(())
}

/// Resolves one widget, rejecting a combination of fields that cannot render.
fn validate_widget(
    raw: RawWidget,
    device_id: &str,
    device_precision: Option<u8>,
    has_home_assistant: bool,
) -> Result<Widget> {
    let RawWidget {
        id,
        kind,
        col,
        row,
        col_span,
        row_span,
        label,
        unit,
        precision,
        state_text,
        stale_after,
        entity,
        attribute,
        on_values,
        icon,
        icon_on,
        icon_off,
        readings,
        grid,
        widgets,
        tap,
    } = raw;

    let precision = precision.or(device_precision);
    if let Some(precision) = precision {
        ensure!(
            precision <= MAX_PRECISION,
            "widget `{id}` on device `{device_id}` has precision {precision}, above the \
             {MAX_PRECISION} decimal places a panel can use"
        );
    }

    let reads_home_assistant = matches!(
        kind,
        WidgetKind::HaEntity | WidgetKind::Weather | WidgetKind::List
    );
    if matches!(kind, WidgetKind::HaEntity | WidgetKind::Weather) {
        ensure!(
            entity.is_some(),
            "widget `{id}` on device `{device_id}` has kind {kind} but no `entity`"
        );
    }

    // A weather condition *is* the entity's state, so an `attribute` here is not
    // a harmless extra: it says "read something else", which this kind cannot do.
    // Silently ignoring it would leave an author staring at an icon that never
    // matches the number they asked for.
    ensure!(
        !(kind == WidgetKind::Weather && attribute.is_some()),
        "widget `{id}` on device `{device_id}` has kind weather and an `attribute`; \
         a weather cell draws the entity's own condition. Use a `reading` to show one \
         of its numbers beside the condition, or kind `ha_entity` to show one alone"
    );

    // A field only one kind can act on is rejected rather than ignored. A silently
    // ignored `icon_on` is an author staring at a dot, wondering which of the two
    // spellings was wrong.
    ensure!(
        kind == WidgetKind::Beacon || (icon_on.is_none() && icon_off.is_none()),
        "widget `{id}` on device `{device_id}` has kind {kind} and an `icon_on` or \
         `icon_off`; only a beacon has two states to draw"
    );
    ensure!(
        matches!(kind, WidgetKind::List | WidgetKind::Weather) || readings.is_empty(),
        "widget `{id}` on device `{device_id}` has kind {kind} and a `reading`; only \
         `list` and `weather` cells are made of readings"
    );
    if kind == WidgetKind::List {
        ensure!(
            !readings.is_empty(),
            "widget `{id}` on device `{device_id}` has kind list but no `reading`; a list \
             cell is its readings, so there would be nothing to draw"
        );
    }

    let readings = readings
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            validate_reading(raw, index, &id, device_id, entity.as_deref(), precision)
        })
        .collect::<Result<Vec<_>>>()?;

    let group = match kind {
        WidgetKind::Group => {
            let grid = grid.with_context(|| {
                format!("widget `{id}` on device `{device_id}` has kind group but no `grid`")
            })?;
            validate_grid(grid, &format!("group `{id}` on device `{device_id}`"))?;
            ensure!(
                !widgets.is_empty(),
                "widget `{id}` on device `{device_id}` has kind group but no children; \
                 write them as [[device.widget.widget]] tables"
            );
            // A group is a grid. A unit, an entity or an attribute on one would
            // describe a value it does not have.
            ensure!(
                entity.is_none() && attribute.is_none() && unit.is_none(),
                "widget `{id}` on device `{device_id}` has kind group and an `entity`, \
                 `attribute` or `unit`; a group draws no value of its own, its children do"
            );

            let children = widgets
                .into_iter()
                .map(|child| validate_widget(child, device_id, precision, has_home_assistant))
                .collect::<Result<Vec<_>>>()?;
            for child in &children {
                ensure!(
                    child.group.is_none(),
                    "widget `{}` is a group inside group `{id}` on device `{device_id}`; \
                     groups nest one level deep, which is the whole depth a panel can show \
                     legibly",
                    child.id
                );
            }
            validate_placement(device_id, Some(&id), grid, &children)?;
            Some(Group {
                grid,
                widgets: children,
            })
        }
        _ => {
            ensure!(
                grid.is_none() && widgets.is_empty(),
                "widget `{id}` on device `{device_id}` has kind {kind} and a `grid` or \
                 children of its own; only a group holds widgets"
            );
            None
        }
    };

    for (field, spec) in [
        ("icon", &icon),
        ("icon_on", &icon_on),
        ("icon_off", &icon_off),
    ] {
        if let Some(spec) = spec {
            crate::icon::validate(spec).with_context(|| {
                format!("widget `{id}` on device `{device_id}` has an invalid {field}")
            })?;
        }
    }

    let tap = match tap {
        Some(raw) => Some(validate_tap(raw, &id, device_id, entity.as_deref())?),
        None => None,
    };

    // Checked once, after both the kind and the tap have had their say, so the
    // error names every reason Home Assistant is needed rather than only the
    // first.
    ensure!(
        has_home_assistant || !(reads_home_assistant || matches!(tap, Some(Tap::Service(_)))),
        "widget `{id}` on device `{device_id}` needs Home Assistant, \
         so the config needs a [home_assistant] section with base_url and a token"
    );

    Ok(Widget {
        id,
        kind,
        col,
        row,
        col_span,
        row_span,
        label,
        unit,
        precision,
        state_text,
        stale_after,
        entity,
        attribute,
        on_values,
        icon,
        icon_on,
        icon_off,
        readings,
        group,
        tap,
    })
}

/// Resolves one of a cell's readings, inheriting what it does not say from the
/// widget holding it.
fn validate_reading(
    raw: RawReading,
    index: usize,
    widget_id: &str,
    device_id: &str,
    widget_entity: Option<&str>,
    widget_precision: Option<u8>,
) -> Result<Reading> {
    let RawReading {
        label,
        entity,
        attribute,
        unit,
        precision,
    } = raw;

    let entity = entity
        .or_else(|| widget_entity.map(str::to_owned))
        .with_context(|| {
            format!(
                "reading {} of widget `{widget_id}` on device `{device_id}` names no \
                 `entity`, and the widget has none for it to fall back on",
                index + 1
            )
        })?;
    if let Some(precision) = precision {
        ensure!(
            precision <= MAX_PRECISION,
            "reading {} of widget `{widget_id}` on device `{device_id}` has precision \
             {precision}, above the {MAX_PRECISION} decimal places a panel can use",
            index + 1
        );
    }

    Ok(Reading {
        label,
        entity,
        attribute,
        unit,
        precision: precision.or(widget_precision),
    })
}

/// Resolves a `tap`, whichever of its spellings the file used.
///
/// The terse form aims at the widget's own `entity` because that is the case
/// worth making short: a cell that already reads `light.desk` should be able to
/// say `tap = "light.toggle"` and mean it.
fn validate_tap(
    raw: RawTap,
    widget_id: &str,
    device_id: &str,
    widget_entity: Option<&str>,
) -> Result<Tap> {
    let (service, target, extra) = match raw {
        RawTap::Terse(verb) if verb == "refresh" => return Ok(Tap::Refresh),
        RawTap::Terse(verb) => {
            let entity = widget_entity.with_context(|| {
                format!(
                    "widget `{widget_id}` on device `{device_id}` has tap = \"{verb}\" \
                     but no `entity` for it to aim at; give the widget an `entity`, \
                     or use the table form: tap = {{ service = \"{verb}\", entity = \"...\" }}"
                )
            })?;
            (verb, Some(entity.to_owned()), toml::Table::new())
        }
        RawTap::Table(table) => {
            let target = table.entity.or_else(|| widget_entity.map(str::to_owned));
            (table.service, target, table.data)
        }
    };

    let (domain, service) = service.split_once('.').with_context(|| {
        format!(
            "widget `{widget_id}` on device `{device_id}` has tap service `{service}`, \
             which is not a Home Assistant service. Write it as `domain.service`, \
             e.g. `light.toggle`, or use the one bare verb, `refresh`"
        )
    })?;
    for (part, what) in [(domain, "domain"), (service, "service")] {
        ensure!(
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "widget `{widget_id}` on device `{device_id}` has tap service \
             `{domain}.{service}`, whose {what} is not a Home Assistant identifier; \
             those are lower-case letters, digits and underscores"
        );
    }

    let mut data = serde_json::Map::new();
    for (key, value) in extra {
        data.insert(key, to_json(value));
    }
    // Set after the extra data so a caller who really means to aim at several
    // entities can write `data = { entity_id = [..] }` and have it win.
    if let Some(target) = target {
        data.entry("entity_id")
            .or_insert_with(|| serde_json::Value::String(target));
    }

    Ok(Tap::Service(ServiceCall {
        domain: domain.to_owned(),
        service: service.to_owned(),
        data,
    }))
}

/// TOML service data as the JSON body Home Assistant expects.
///
/// TOML dates have no JSON counterpart and no meaning to a service call, so they
/// go across as their RFC 3339 text, which is what a Home Assistant service that
/// takes a datetime wants anyway.
fn to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(items) => items.into_iter().map(to_json).collect(),
        toml::Value::Table(table) => {
            serde_json::Value::Object(table.into_iter().map(|(k, v)| (k, to_json(v))).collect())
        }
    }
}

/// One document's shape.
///
/// Distinct from [`Config`] wherever a document is allowed to omit something the
/// rest of the program should not have to think about. Every section is optional
/// here because a fragment may carry only devices, or only a dashboard;
/// [`parse_documents`] is what insists that `[server]` was declared exactly once
/// across the set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    server: Option<Server>,
    home_assistant: Option<HomeAssistant>,
    #[serde(default, rename = "dashboard")]
    dashboards: Vec<RawDashboard>,
    #[serde(default, rename = "device")]
    devices: Vec<RawDevice>,
}

/// A named grid and widget set, declared once and adopted by any number of
/// devices.
///
/// Cloned into each adopting device rather than shared behind a handle: a
/// validated [`Device`] owns its widgets, which is what keeps rendering and tap
/// resolution from having to chase a reference into the configuration that
/// produced them.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawDashboard {
    name: String,
    grid: Grid,
    #[serde(default, rename = "widget")]
    widgets: Vec<RawWidget>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    id: String,
    width: u32,
    height: u32,
    palette: Palette,
    dither: Dither,
    refresh_rate: u32,
    render_interval: Option<u32>,
    #[serde(default = "default_max_frame_bytes")]
    max_frame_bytes: usize,
    /// Decimal places every widget on this device inherits.
    precision: Option<u8>,
    /// Required unless `dashboard` names one to adopt.
    grid: Option<Grid>,
    chrome: Option<RawChrome>,
    status_bar: Option<RawStatusBar>,
    /// A [`RawDashboard`] to adopt instead of declaring a grid and widgets.
    dashboard: Option<String>,
    #[serde(default, rename = "widget")]
    widgets: Vec<RawWidget>,
}

/// A device's spacing, as the file may leave it: any field absent is derived from
/// the cell size.
#[derive(Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct RawChrome {
    gap: Option<u32>,
    padding: Option<u32>,
    border: Option<u32>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawStatusBar {
    edge: Edge,
    thickness: Option<u32>,
    fields: Vec<StatusField>,
    timezone: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawWidget {
    id: String,
    kind: WidgetKind,
    col: u32,
    row: u32,
    #[serde(default = "one")]
    col_span: u32,
    #[serde(default = "one")]
    row_span: u32,
    label: Option<String>,
    unit: Option<String>,
    precision: Option<u8>,
    #[serde(default = "yes")]
    state_text: bool,
    #[serde(default)]
    stale_after: u64,
    entity: Option<String>,
    attribute: Option<String>,
    #[serde(default = "default_on_values")]
    on_values: Vec<String>,
    icon: Option<String>,
    icon_on: Option<String>,
    icon_off: Option<String>,
    #[serde(default, rename = "reading")]
    readings: Vec<RawReading>,
    /// A group's sub-grid.
    grid: Option<Grid>,
    /// A group's children.
    #[serde(default, rename = "widget")]
    widgets: Vec<RawWidget>,
    tap: Option<RawTap>,
}

/// One reading of a `list` or `weather` cell, as the file spells it.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawReading {
    label: Option<String>,
    /// Defaults to the widget's own `entity`.
    entity: Option<String>,
    attribute: Option<String>,
    unit: Option<String>,
    precision: Option<u8>,
}

/// A `tap` as the file may spell it.
///
/// Untagged, so both spellings are the same key. The short one is a string
/// because that is how it reads in a file — `tap = "light.toggle"` — and the long
/// one is a table because a service with data has nowhere else to put it.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum RawTap {
    Terse(String),
    Table(RawTapTable),
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawTapTable {
    /// `domain.service`, e.g. `light.turn_on`.
    service: String,
    /// The entity to aim at, overriding the widget's own.
    entity: Option<String>,
    /// Extra service data, merged under the resolved `entity_id`.
    #[serde(default)]
    data: toml::Table,
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

    /// A dashboard declared once, for the devices that adopt it.
    const DASHBOARD: &str = r#"
[[dashboard]]
name = "wall"
grid = { cols = 2, rows = 2 }

[[dashboard.widget]]
id = "clock"
kind = "text"
col = 0
row = 0
"#;

    /// A group of two children occupying the first cell of [`BASE`]'s grid.
    const GROUP: &str = r#"
[[device.widget]]
id = "cluster"
kind = "group"
col = 0
row = 0
grid = { cols = 2, rows = 1 }

[[device.widget.widget]]
id = "left"
kind = "text"
col = 0
row = 0

[[device.widget.widget]]
id = "right"
kind = "text"
col = 1
row = 0
"#;

    fn err(text: &str) -> String {
        chain(parse(text).expect_err("expected this config to be rejected"))
    }

    /// [`err`] for a configuration spread across several documents.
    fn err_documents(documents: &[Document<'_>]) -> String {
        chain(parse_documents(documents).expect_err("expected this config to be rejected"))
    }

    /// An error and its causes, flattened to one line.
    ///
    /// Validation context is attached with `with_context`, so the offending detail
    /// can be on any link of the chain.
    fn chain(error: anyhow::Error) -> String {
        error
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join(": ")
    }

    /// [`BASE`] without its device, for a test declaring devices of its own.
    fn server_only() -> &'static str {
        &BASE[..BASE.find("[[device]]").unwrap()]
    }

    /// [`BASE`]'s device alone, for a test that puts it in a second document.
    fn device_only() -> &'static str {
        &BASE[BASE.find("[[device]]").unwrap()..]
    }

    /// A device that adopts a dashboard rather than declaring a grid of its own.
    fn adopting(id: &str, dashboard: &str) -> String {
        format!(
            "\n[[device]]\nid = \"{id}\"\nwidth = 1024\nheight = 758\n\
             palette = \"gray16\"\ndither = \"atkinson\"\nrefresh_rate = 300\n\
             dashboard = \"{dashboard}\"\n"
        )
    }

    /// [`BASE`] with `line` added to its device table.
    fn with_device_line(line: &str) -> String {
        BASE.replace("refresh_rate = 300", &format!("refresh_rate = 300\n{line}"))
    }

    /// [`BASE`] with a status bar, written as an inline table so that a case still
    /// differs from the baseline by exactly one line.
    fn with_status_bar(inline: &str) -> String {
        with_device_line(&format!("status_bar = {{ {inline} }}"))
    }

    /// [`BASE`] with the `[home_assistant]` section every entity-reading kind
    /// needs, and `widgets` after it.
    fn with_home_assistant(widgets: &str) -> String {
        format!(
            "{BASE}\n[home_assistant]\nbase_url = \"http://ha.local:8123\"\ntoken = \"t\"\n\
             {widgets}"
        )
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
    fn a_home_assistant_base_url_may_be_one_the_panel_could_never_reach() {
        // The panel never dials Home Assistant — this process does. A loopback or a
        // container-internal name there is ordinary rather than a mistake: a
        // sidecar, a tunnel, or Home Assistant on the same host. Rejecting it with
        // "the device cannot reach this" was a rule borrowed from the wrong field.
        for base in [
            "http://127.0.0.1:8123",
            "http://localhost:8123",
            "http://homeassistant.default.svc.cluster.local:8123",
            "http://[::1]:8123",
        ] {
            let text = format!("{BASE}\n[home_assistant]\nbase_url = \"{base}\"\ntoken = \"t\"\n");
            let config = parse(&text).unwrap_or_else(|e| panic!("{base} rejected: {e:#}"));
            assert_eq!(config.home_assistant.unwrap().base_url, base);
        }
    }

    #[test]
    fn a_home_assistant_base_url_still_needs_a_scheme_and_a_host() {
        for (base, expected) in [
            ("homeassistant.local:8123", "http:// or https://"),
            ("http://:8123", "must include a host"),
            ("", "must not be empty"),
        ] {
            let text = format!("{BASE}\n[home_assistant]\nbase_url = \"{base}\"\ntoken = \"t\"\n");
            let message = err(&text);
            assert!(
                message.contains("home_assistant.base_url") && message.contains(expected),
                "{base:?} should be rejected for {expected}, got: {message}"
            );
        }
    }

    #[test]
    fn strips_a_trailing_slash_from_the_home_assistant_base_url() {
        // Not for the panel's sake here, but for ours: request paths are built by
        // plain concatenation on this side too.
        let text = format!(
            "{BASE}\n[home_assistant]\nbase_url = \"http://ha.local:8123/\"\ntoken = \"t\"\n"
        );
        assert_eq!(
            parse(&text).unwrap().home_assistant.unwrap().base_url,
            "http://ha.local:8123"
        );
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
        assert!(message.contains("declared in both"), "{message}");
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
        // A zero gap is what makes "exactly at the floor" expressible at all: with
        // one, a cell is the area less `n + 1` gaps, and the derived gap is itself
        // a function of the size being chosen.
        let text = with_device_line("chrome = { gap = 0 }")
            .replace("width = 1024", &format!("width = {}", MIN_CELL * 4))
            .replace("height = 758", &format!("height = {}", MIN_CELL * 3));
        let device = &parse(&text).unwrap().devices[0];
        assert_eq!(device.width, MIN_CELL * 4);
        assert_eq!(
            cell_size(device.width, device.height, device.grid, device.chrome),
            (MIN_CELL as f32, MIN_CELL as f32)
        );
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
    fn max_frame_bytes_defaults_and_is_overridable() {
        // A ceiling that suits the original TRMNL boards is wrong for a client with
        // real memory, so it is per-device rather than a constant.
        assert_eq!(
            parse(BASE).unwrap().devices[0].max_frame_bytes,
            DEFAULT_MAX_FRAME_BYTES
        );

        let raised = BASE.replace(
            "refresh_rate = 300",
            "refresh_rate = 300\nmax_frame_bytes = 250000",
        );
        assert_eq!(parse(&raised).unwrap().devices[0].max_frame_bytes, 250_000);
    }

    #[test]
    fn a_zero_max_frame_bytes_disables_the_check() {
        let text = BASE.replace(
            "refresh_rate = 300",
            "refresh_rate = 300\nmax_frame_bytes = 0",
        );
        assert_eq!(parse(&text).unwrap().devices[0].max_frame_bytes, 0);
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

    #[test]
    fn devices_from_two_documents_both_appear() {
        let hallway = device_only().replace("kindle", "hallway");
        let config = parse_documents(&[
            Document {
                name: "main.toml",
                text: BASE,
            },
            Document {
                name: "extra.toml",
                text: &hallway,
            },
        ])
        .unwrap();
        let ids: Vec<&str> = config.devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["kindle", "hallway"], "devices follow document order");
    }

    #[test]
    fn rejects_a_server_section_in_two_documents_naming_both() {
        let message = err_documents(&[
            Document {
                name: "main.toml",
                text: BASE,
            },
            Document {
                name: "extra.toml",
                text: "[server]\nlisten = \"0.0.0.0:4445\"\n\
                       public_base_url = \"http://192.168.0.51:4445\"\n",
            },
        ]);
        assert!(message.contains("[server]"), "{message}");
        assert!(message.contains("main.toml"), "{message}");
        assert!(message.contains("extra.toml"), "{message}");
    }

    #[test]
    fn rejects_a_configuration_with_no_server_section_anywhere() {
        let message = err_documents(&[Document {
            name: "extra.toml",
            text: device_only(),
        }]);
        assert!(message.contains("[server]"), "{message}");
    }

    #[test]
    fn rejects_a_duplicate_device_id_across_documents_naming_both() {
        let message = err_documents(&[
            Document {
                name: "main.toml",
                text: BASE,
            },
            Document {
                name: "extra.toml",
                text: device_only(),
            },
        ]);
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("main.toml"), "{message}");
        assert!(message.contains("extra.toml"), "{message}");
    }

    #[test]
    fn rejects_home_assistant_declared_twice_naming_both() {
        let section = "[home_assistant]\nbase_url = \"http://ha.local:8123\"\ntoken = \"t\"\n";
        let first = format!("{BASE}\n{section}");
        let message = err_documents(&[
            Document {
                name: "main.toml",
                text: &first,
            },
            Document {
                name: "extra.toml",
                text: section,
            },
        ]);
        assert!(message.contains("[home_assistant]"), "{message}");
        assert!(message.contains("main.toml"), "{message}");
        assert!(message.contains("extra.toml"), "{message}");
    }

    #[test]
    fn two_devices_adopting_one_dashboard_get_the_same_widgets() {
        let text = format!(
            "{}{DASHBOARD}{}{}",
            server_only(),
            adopting("kindle", "wall"),
            adopting("hallway", "wall")
        );
        let config = parse(&text).unwrap();
        assert_eq!(config.devices.len(), 2);
        assert_eq!(config.devices[0].grid, Grid { cols: 2, rows: 2 });
        assert_eq!(
            config.devices[0].widgets, config.devices[1].widgets,
            "a dashboard is adopted whole, so both devices hold the same cells"
        );
    }

    #[test]
    fn rejects_adopting_a_dashboard_nothing_declares() {
        let message = err(&format!("{}{}", server_only(), adopting("kindle", "hall")));
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("hall"), "{message}");
    }

    #[test]
    fn rejects_a_dashboard_name_declared_twice() {
        let text = format!(
            "{}{DASHBOARD}{DASHBOARD}{}",
            server_only(),
            adopting("kindle", "wall")
        );
        let message = err(&text);
        assert!(message.contains("declared in both"), "{message}");
        assert!(message.contains("wall"), "{message}");
    }

    #[test]
    fn rejects_a_device_that_adopts_a_dashboard_and_declares_its_own() {
        // A dashboard brings the grid its widgets were laid out on, so taking one
        // and overriding the other places someone else's widgets on a grid their
        // author never saw.
        for own in [
            "grid = { cols = 4, rows = 3 }\n",
            "\n[[device.widget]]\nid = \"solo\"\nkind = \"text\"\ncol = 0\nrow = 0\n",
        ] {
            let text = format!(
                "{}{DASHBOARD}{}{own}",
                server_only(),
                adopting("kindle", "wall")
            );
            let message = err(&text);
            assert!(message.contains("kindle"), "{own}: {message}");
            assert!(message.contains("wall"), "{own}: {message}");
        }
    }

    #[test]
    fn rejects_a_device_with_neither_a_grid_nor_a_dashboard() {
        let message = err(&BASE.replace("grid = { cols = 4, rows = 3 }\n", ""));
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("neither"), "{message}");
    }

    #[test]
    fn a_failure_inside_an_adopted_dashboard_names_the_adopting_device() {
        // The author's next move is to fix the dashboard, but the panel that went
        // blank is the device, and the message has to connect the two.
        let broken = DASHBOARD.replace(r#"kind = "text""#, r#"kind = "list""#);
        let text = format!("{}{broken}{}", server_only(), adopting("kindle", "wall"));
        let message = err(&text);
        assert!(message.contains("wall"), "{message}");
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("clock"), "{message}");
    }

    #[test]
    fn chrome_overrides_the_derived_spacing() {
        let chrome = parse(&with_device_line(
            "chrome = { gap = 4, padding = 12, border = 2 }",
        ))
        .unwrap()
        .devices[0]
            .chrome;
        assert_eq!(
            chrome,
            Chrome {
                gap: 4.0,
                padding: 12.0,
                border: 2.0
            }
        );
        assert_eq!(
            chrome.inset(),
            28.0,
            "a content box pays for its padding and its rule on both sides"
        );
    }

    #[test]
    fn omitting_chrome_reproduces_the_derived_spacing() {
        let device = &parse(BASE).unwrap().devices[0];
        assert_eq!(
            device.chrome,
            Chrome::derived(device.width, device.height, device.grid)
        );
    }

    #[test]
    fn a_partial_chrome_leaves_the_rest_derived() {
        let derived = Chrome::derived(1024, 758, Grid { cols: 4, rows: 3 });
        let chrome = parse(&with_device_line("chrome = { gap = 2 }"))
            .unwrap()
            .devices[0]
            .chrome;
        assert_eq!(chrome.gap, 2.0);
        assert_eq!(chrome.padding, derived.padding);
        assert_eq!(chrome.border, derived.border);
    }

    #[test]
    fn a_zero_border_draws_no_frame_and_is_accepted() {
        let chrome = parse(&with_device_line("chrome = { border = 0 }"))
            .unwrap()
            .devices[0]
            .chrome;
        assert_eq!(
            chrome.border, 0.0,
            "a dashboard of bare readings wants none"
        );
        assert_eq!(chrome.inset(), chrome.padding * 2.0);
    }

    #[test]
    fn rejects_a_gap_that_starves_the_grid() {
        // A gap is charged `n + 1` times per axis, so a value well inside the
        // MAX_CHROME ceiling still leaves cells below the legibility floor.
        let text = with_device_line("chrome = { gap = 60 }")
            .replace("width = 1024", "width = 400")
            .replace("height = 758", "height = 400");
        assert!(
            parse(&text.replace("chrome = { gap = 60 }\n", "")).is_ok(),
            "the panel itself has to be valid, so the gap is the only cause"
        );
        let message = err(&text);
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("60 pixel gap"), "{message}");
    }

    #[test]
    fn rejects_padding_and_a_border_that_leave_no_content_box() {
        let message = err(&with_device_line("chrome = { padding = 64, border = 64 }"));
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("content box"), "{message}");
    }

    #[test]
    fn rejects_a_chrome_measurement_above_the_ceiling() {
        for field in ["gap", "padding", "border"] {
            let message = err(&with_device_line(&format!(
                "chrome = {{ {field} = {} }}",
                MAX_CHROME + 1
            )));
            assert!(
                message.contains(&format!("chrome.{field} {}", MAX_CHROME + 1)),
                "{message}"
            );
        }
    }

    #[test]
    fn a_sub_cell_pays_for_one_fewer_gap_than_an_outer_cell() {
        // A group's own padding is its children's outer margin, so `n` sub-cells
        // want `n - 1` gaps where `n` outer cells want `n + 1`.
        let chrome = Chrome {
            gap: 10.0,
            padding: 8.0,
            border: 1.0,
        };
        let grid = Grid { cols: 2, rows: 1 };
        assert_eq!(cell_size(200, 100, grid, chrome), (85.0, 80.0));
        assert_eq!(sub_cell_size(200.0, 100.0, grid, chrome), (95.0, 100.0));
    }

    #[test]
    fn a_device_with_no_status_bar_gives_the_grid_the_whole_frame() {
        let device = &parse(BASE).unwrap().devices[0];
        assert!(device.status_bar.is_none());
        assert_eq!(device.grid_area(), (0, 0, 1024, 758));
        assert_eq!(device.status_bar_area(), None);
    }

    #[test]
    fn each_status_bar_edge_takes_its_strip_off_the_grid_area() {
        for (edge, grid, bar) in [
            ("top", (0, 40, 1024, 718), (0, 0, 1024, 40)),
            ("bottom", (0, 0, 1024, 718), (0, 718, 1024, 40)),
            ("left", (40, 0, 984, 758), (0, 0, 40, 758)),
            ("right", (0, 0, 984, 758), (984, 0, 40, 758)),
        ] {
            let text = with_status_bar(&format!(
                "edge = \"{edge}\", thickness = 40, fields = [\"time\"]"
            ));
            let device = &parse(&text)
                .unwrap_or_else(|e| panic!("a {edge} bar should parse: {e:#}"))
                .devices[0];
            assert_eq!(device.grid_area(), grid, "{edge}");
            assert_eq!(device.status_bar_area(), Some(bar), "{edge}");
        }
    }

    #[test]
    fn a_status_bar_thickness_defaults_to_a_twentieth_of_the_short_side() {
        let text = with_status_bar("edge = \"top\", fields = [\"date\", \"time\"]");
        let config = parse(&text).unwrap();
        let bar = config.devices[0].status_bar.as_ref().unwrap();
        assert_eq!(bar.thickness, 38, "5% of 758, bounded to 18..=64");
        assert_eq!(bar.edge, Edge::Top);
        assert_eq!(bar.fields, [StatusField::Date, StatusField::Time]);
    }

    #[test]
    fn parses_every_documented_status_field() {
        for (name, expected) in [
            ("date", StatusField::Date),
            ("time", StatusField::Time),
            ("battery", StatusField::Battery),
            ("refresh", StatusField::Refresh),
            ("device", StatusField::Device),
            ("signal", StatusField::Signal),
        ] {
            let text = with_status_bar(&format!("edge = \"top\", fields = [\"{name}\"]"));
            let config = parse(&text).unwrap_or_else(|e| panic!("{name} should parse: {e:#}"));
            assert_eq!(
                config.devices[0].status_bar.as_ref().unwrap().fields,
                [expected]
            );
        }
    }

    #[test]
    fn rejects_a_status_bar_with_no_fields() {
        let message = err(&with_status_bar("edge = \"top\", fields = []"));
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("fields"), "{message}");
    }

    #[test]
    fn rejects_a_status_bar_that_starves_the_grid() {
        let bar = "edge = \"left\", thickness = 250, fields = [\"time\"]";
        let text = with_status_bar(bar).replace("width = 1024", "width = 280");
        let without = text.replace(&format!("status_bar = {{ {bar} }}\n"), "");
        assert!(
            parse(&without).is_ok(),
            "the panel itself has to be valid, so the bar is the only cause"
        );
        let message = err(&text);
        assert!(message.contains("kindle"), "{message}");
        assert!(message.contains("status_bar"), "{message}");
    }

    #[test]
    fn rejects_a_status_bar_thickness_outside_its_bounds() {
        for thickness in [
            *STATUS_BAR_THICKNESS_BOUNDS.start() - 1,
            *STATUS_BAR_THICKNESS_BOUNDS.end() + 1,
        ] {
            let message = err(&with_status_bar(&format!(
                "edge = \"top\", thickness = {thickness}, fields = [\"time\"]"
            )));
            assert!(
                message.contains(&format!("{thickness} pixels thick")),
                "{message}"
            );
        }
    }

    #[test]
    fn resolves_an_iana_zone_and_follows_its_rules_across_a_transition() {
        let text =
            with_status_bar("edge = \"top\", fields = [\"time\"], timezone = \"Europe/Lisbon\"");
        let config = parse(&text).unwrap();
        let zone = &config.devices[0].status_bar.as_ref().unwrap().timezone;
        assert_eq!(zone.name(), "Europe/Lisbon");

        // The whole reason this is a zone and not an offset: one name, two offsets,
        // and the database is what knows which applies when. 2026-01-15 is WET and
        // 2026-07-15 is WEST.
        let winter = OffsetDateTime::from_unix_timestamp(1_768_478_400).unwrap();
        let summer = OffsetDateTime::from_unix_timestamp(1_784_116_800).unwrap();
        assert_eq!(zone.at(winter).offset().whole_hours(), 0, "WET");
        assert_eq!(zone.at(summer).offset().whole_hours(), 1, "WEST");
    }

    #[test]
    fn a_zone_south_of_the_equator_moves_the_other_way() {
        // A northern-hemisphere-only test would pass on a sign error.
        let text =
            with_status_bar("edge = \"top\", fields = [\"time\"], timezone = \"Australia/Sydney\"");
        let config = parse(&text).unwrap();
        let zone = &config.devices[0].status_bar.as_ref().unwrap().timezone;

        let january = OffsetDateTime::from_unix_timestamp(1_768_478_400).unwrap();
        let july = OffsetDateTime::from_unix_timestamp(1_784_116_800).unwrap();
        assert_eq!(zone.at(january).offset().whole_hours(), 11, "AEDT");
        assert_eq!(zone.at(july).offset().whole_hours(), 10, "AEST");
    }

    #[test]
    fn rejects_a_zone_the_database_does_not_have() {
        for raw in ["Europe/Nowhere", "midday", "+02:00", ""] {
            let text = with_status_bar(&format!(
                "edge = \"top\", fields = [\"time\"], timezone = \"{raw}\""
            ));
            let message = err(&text);
            assert!(
                message.contains("timezone"),
                "`{raw}` should be rejected as a zone, got: {message}"
            );
        }
    }

    #[test]
    fn a_fixed_offset_is_no_longer_a_time_zone() {
        // `utc_offset` was what this took before the database went in. Rejected
        // rather than tolerated: a panel that quietly ignored it would keep the
        // wrong clock it was configured to have.
        let text = with_status_bar("edge = \"top\", fields = [\"time\"], utc_offset = \"+01:00\"");
        let message = err(&text);
        assert!(message.contains("utc_offset"), "{message}");
    }

    #[test]
    fn the_default_zone_is_utc() {
        // The only zone paneld can assume without implying it knows where the panel
        // hangs.
        let text = with_status_bar("edge = \"top\", fields = [\"time\"]");
        let config = parse(&text).unwrap();
        assert_eq!(
            config.devices[0]
                .status_bar
                .as_ref()
                .unwrap()
                .timezone
                .name(),
            "UTC"
        );
    }

    #[test]
    fn the_embedded_database_is_named() {
        // Frozen at build time, so an operator has to be able to see which release
        // the clock's rules came from.
        assert!(
            TZDATA_VERSION.len() >= 5 && TZDATA_VERSION.is_ascii(),
            "expected an IANA release like `2026b`, got `{TZDATA_VERSION}`"
        );
    }

    #[test]
    fn derived_chrome_is_measured_against_the_grid_area_not_the_frame() {
        // The cells live in the grid area, so spacing derived from the whole frame
        // would be spacing for a grid this device does not have.
        let text = with_status_bar("edge = \"top\", thickness = 150, fields = [\"time\"]")
            .replace("width = 1024", "width = 400")
            .replace("height = 758", "height = 400");
        let device = &parse(&text).unwrap().devices[0];
        assert_eq!(device.grid_area(), (0, 150, 400, 250));
        assert_eq!(device.chrome, Chrome::derived(400, 250, device.grid));
        assert_ne!(device.chrome, Chrome::derived(400, 400, device.grid));
    }

    #[test]
    fn precision_is_absent_unless_asked_for() {
        let text =
            format!("{BASE}\n[[device.widget]]\nid = \"a\"\nkind = \"value\"\ncol = 0\nrow = 0\n");
        assert_eq!(
            parse(&text).unwrap().devices[0].widgets[0].precision,
            None,
            "no precision renders whatever the source said, digit for digit"
        );
    }

    #[test]
    fn a_device_precision_is_inherited_by_its_widgets() {
        let text = format!(
            "{}\n[[device.widget]]\nid = \"a\"\nkind = \"value\"\ncol = 0\nrow = 0\n",
            with_device_line("precision = 1")
        );
        assert_eq!(
            parse(&text).unwrap().devices[0].widgets[0].precision,
            Some(1)
        );
    }

    #[test]
    fn a_widget_precision_overrides_the_device_default() {
        let text = format!(
            "{}\n[[device.widget]]\nid = \"a\"\nkind = \"value\"\ncol = 0\nrow = 0\n\
             precision = 3\n",
            with_device_line("precision = 1")
        );
        assert_eq!(
            parse(&text).unwrap().devices[0].widgets[0].precision,
            Some(3)
        );
    }

    #[test]
    fn a_reading_precision_overrides_the_widget_and_the_device() {
        let text = with_home_assistant(
            r#"
[[device.widget]]
id = "climate"
kind = "list"
col = 0
row = 0
precision = 2
entity = "sensor.office"

[[device.widget.reading]]
label = "Temp"
attribute = "temperature"

[[device.widget.reading]]
label = "Humidity"
attribute = "humidity"
precision = 0
"#,
        )
        .replace("refresh_rate = 300", "refresh_rate = 300\nprecision = 4");
        let config = parse(&text).unwrap();
        let widget = &config.devices[0].widgets[0];
        assert_eq!(
            widget.precision,
            Some(2),
            "the widget's own precision wins over the device's"
        );
        assert_eq!(
            widget.readings[0].precision,
            Some(2),
            "a reading that says nothing inherits the widget's"
        );
        assert_eq!(
            widget.readings[1].precision,
            Some(0),
            "a reading's own precision wins over both"
        );
    }

    #[test]
    fn rejects_a_device_precision_above_the_ceiling() {
        let message = err(&with_device_line(&format!(
            "precision = {}",
            MAX_PRECISION + 1
        )));
        assert!(message.contains("kindle"), "{message}");
        assert!(
            message.contains(&format!("precision {}", MAX_PRECISION + 1)),
            "{message}"
        );
    }

    #[test]
    fn rejects_a_widget_precision_above_the_ceiling() {
        let text = format!(
            "{BASE}\n[[device.widget]]\nid = \"a\"\nkind = \"value\"\ncol = 0\nrow = 0\n\
             precision = {}\n",
            MAX_PRECISION + 1
        );
        let message = err(&text);
        assert!(message.contains("widget `a`"), "{message}");
        assert!(
            message.contains(&format!("precision {}", MAX_PRECISION + 1)),
            "{message}"
        );
    }

    #[test]
    fn rejects_a_reading_precision_above_the_ceiling() {
        let text = with_home_assistant(&format!(
            "\n[[device.widget]]\nid = \"climate\"\nkind = \"list\"\ncol = 0\nrow = 0\n\
             entity = \"sensor.office\"\n\n[[device.widget.reading]]\nlabel = \"Temp\"\n\
             precision = {}\n",
            MAX_PRECISION + 1
        ));
        let message = err(&text);
        assert!(message.contains("reading 1"), "{message}");
        assert!(
            message.contains(&format!("precision {}", MAX_PRECISION + 1)),
            "{message}"
        );
    }

    #[test]
    fn rejects_a_list_with_no_reading() {
        // A list cell *is* its readings, so there would be nothing to draw.
        let text = with_home_assistant(
            "\n[[device.widget]]\nid = \"empty\"\nkind = \"list\"\ncol = 0\nrow = 0\n\
             entity = \"sensor.office\"\n",
        );
        let message = err(&text);
        assert!(message.contains("empty"), "{message}");
        assert!(message.contains("reading"), "{message}");
    }

    #[test]
    fn rejects_a_reading_with_no_entity_anywhere_naming_its_position() {
        let text = with_home_assistant(
            r#"
[[device.widget]]
id = "climate"
kind = "list"
col = 0
row = 0

[[device.widget.reading]]
entity = "sensor.office"

[[device.widget.reading]]
label = "Humidity"
"#,
        );
        let message = err(&text);
        assert!(message.contains("reading 2"), "{message}");
        assert!(message.contains("climate"), "{message}");
    }

    #[test]
    fn a_reading_inherits_the_widgets_entity() {
        let text = with_home_assistant(
            r#"
[[device.widget]]
id = "climate"
kind = "list"
col = 0
row = 0
entity = "sensor.office"

[[device.widget.reading]]
label = "Temp"
attribute = "temperature"
"#,
        );
        let config = parse(&text).unwrap();
        let reading = &config.devices[0].widgets[0].readings[0];
        assert_eq!(reading.entity, "sensor.office");
        assert_eq!(reading.attribute.as_deref(), Some("temperature"));
    }

    #[test]
    fn rejects_a_reading_on_a_kind_that_is_not_made_of_them() {
        let text = format!(
            "{BASE}\n[[device.widget]]\nid = \"solo\"\nkind = \"value\"\ncol = 0\nrow = 0\n\n\
             [[device.widget.reading]]\nentity = \"sensor.office\"\n"
        );
        let message = err(&text);
        assert!(message.contains("solo"), "{message}");
        assert!(message.contains("value"), "{message}");
    }

    #[test]
    fn a_weather_widget_takes_readings_but_still_refuses_an_attribute() {
        let text = with_home_assistant(
            r#"
[[device.widget]]
id = "sky"
kind = "weather"
col = 0
row = 0
entity = "weather.home"

[[device.widget.reading]]
label = "Temp"
attribute = "temperature"
"#,
        );
        let config = parse(&text).unwrap();
        let widget = &config.devices[0].widgets[0];
        assert_eq!(widget.readings.len(), 1);
        assert_eq!(
            widget.readings[0].entity, "weather.home",
            "a reading falls back to the cell's own entity"
        );

        // A weather condition *is* the entity's state, so an `attribute` on the
        // cell says "read something else", which this kind cannot do.
        let message = err(&text.replace(
            "entity = \"weather.home\"",
            "entity = \"weather.home\"\nattribute = \"temperature\"",
        ));
        assert!(message.contains("sky"), "{message}");
        assert!(message.contains("weather"), "{message}");
    }

    #[test]
    fn a_groups_children_parse_and_are_reachable_through_all_widgets() {
        let config = parse(&format!("{BASE}{GROUP}")).unwrap();
        let device = &config.devices[0];
        assert_eq!(
            device.widgets.len(),
            1,
            "a group is one cell of the outer grid"
        );
        let group = device.widgets[0].group.as_ref().unwrap();
        assert_eq!(group.grid, Grid { cols: 2, rows: 1 });
        let ids: Vec<&str> = device.all_widgets().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids,
            ["cluster", "left", "right"],
            "render prep walks all_widgets, so a child it misses reads `no data`"
        );
    }

    #[test]
    fn rejects_a_group_inside_a_group() {
        // Arbitrary nesting would make placement, geometry and tap resolution
        // recursive to serve a composition nobody asked for.
        let message = err(&format!(
            r#"{BASE}
[[device.widget]]
id = "outer"
kind = "group"
col = 0
row = 0
grid = {{ cols = 1, rows = 1 }}

[[device.widget.widget]]
id = "inner"
kind = "group"
col = 0
row = 0
grid = {{ cols = 1, rows = 1 }}

[[device.widget.widget.widget]]
id = "leaf"
kind = "text"
col = 0
row = 0
"#
        ));
        assert!(message.contains("inner"), "{message}");
        assert!(message.contains("outer"), "{message}");
    }

    #[test]
    fn rejects_a_group_child_outside_the_sub_grid() {
        let message = err(&format!("{BASE}{}", GROUP.replace("col = 1", "col = 2")));
        assert!(message.contains("cluster"), "{message}");
        assert!(message.contains("right"), "{message}");
    }

    #[test]
    fn rejects_two_group_children_sharing_a_sub_cell() {
        let message = err(&format!(
            "{BASE}{}",
            GROUP.replace("col = 1\nrow = 0", "col = 0\nrow = 0")
        ));
        assert!(message.contains("cluster"), "{message}");
        assert!(message.contains("left"), "{message}");
        assert!(message.contains("right"), "{message}");
    }

    #[test]
    fn rejects_two_widgets_sharing_an_id_anywhere_on_the_device() {
        // An id is a content push address, so two cells answering to it means one
        // publisher feeds a cell nobody chose.
        let text = format!(
            "{BASE}{GROUP}\n[[device.widget]]\nid = \"left\"\nkind = \"text\"\ncol = 1\nrow = 0\n"
        );
        let message = err(&text);
        assert!(message.contains("share the id `left`"), "{message}");
    }

    #[test]
    fn rejects_a_group_with_no_grid() {
        let message = err(&format!(
            "{BASE}{}",
            GROUP.replace("grid = { cols = 2, rows = 1 }\n", "")
        ));
        assert!(message.contains("cluster"), "{message}");
        assert!(message.contains("grid"), "{message}");
    }

    #[test]
    fn rejects_a_group_with_no_children() {
        let childless = &GROUP[..GROUP.find("[[device.widget.widget]]").unwrap()];
        let message = err(&format!("{BASE}{childless}"));
        assert!(message.contains("cluster"), "{message}");
        assert!(message.contains("children"), "{message}");
    }

    #[test]
    fn rejects_a_group_that_carries_a_value_of_its_own() {
        // A group draws no value, its children do, so any of these would describe
        // something it does not have.
        for field in [
            "entity = \"sensor.office\"",
            "attribute = \"temperature\"",
            "unit = \"°C\"",
        ] {
            let text = format!(
                "{BASE}{}",
                GROUP.replace(
                    "grid = { cols = 2, rows = 1 }",
                    &format!("grid = {{ cols = 2, rows = 1 }}\n{field}")
                )
            );
            let message = err(&text);
            assert!(message.contains("cluster"), "{field}: {message}");
            assert!(message.contains("group"), "{field}: {message}");
        }
    }

    #[test]
    fn rejects_a_sub_grid_too_dense_to_leave_a_content_box() {
        // The floor the device's own cells are held to, one level down: without it
        // a group is the one place an author can still ask for a two-pixel box, and
        // the layout engine answers that with a panic rather than an error.
        let text = format!(
            "{BASE}{}",
            GROUP.replace(
                "grid = { cols = 2, rows = 1 }",
                "grid = { cols = 6, rows = 6 }"
            )
        );
        let message = err(&text);
        assert!(message.contains("cluster"), "{message}");
        assert!(message.contains("sub-grid"), "{message}");
    }

    #[test]
    fn a_beacon_takes_an_icon_for_each_of_its_two_states() {
        let text = format!(
            "{BASE}\n[[device.widget]]\nid = \"lamp\"\nkind = \"beacon\"\ncol = 0\nrow = 0\n\
             icon_on = \"mdi-lightbulb\"\nicon_off = \"mdi-lightbulb-outline\"\n"
        );
        let config = parse(&text).unwrap();
        let widget = &config.devices[0].widgets[0];
        assert_eq!(widget.icon_on.as_deref(), Some("mdi-lightbulb"));
        assert_eq!(widget.icon_off.as_deref(), Some("mdi-lightbulb-outline"));
    }

    #[test]
    fn rejects_two_state_icons_on_a_kind_with_one_state() {
        // Rejected rather than ignored: a silently dropped `icon_on` is an author
        // staring at a dot, wondering which of the two spellings was wrong.
        for kind in ["value", "text"] {
            for field in ["icon_on", "icon_off"] {
                let text = format!(
                    "{BASE}\n[[device.widget]]\nid = \"w\"\nkind = \"{kind}\"\ncol = 0\nrow = 0\n\
                     {field} = \"mdi-lightbulb\"\n"
                );
                let message = err(&text);
                assert!(
                    message.contains("only a beacon"),
                    "{kind}/{field}: {message}"
                );
            }
        }
    }

    #[test]
    fn rejects_an_invalid_icon_spec_naming_the_field_it_came_from() {
        for field in ["icon", "icon_on", "icon_off"] {
            let text = format!(
                "{BASE}\n[[device.widget]]\nid = \"lamp\"\nkind = \"beacon\"\ncol = 0\nrow = 0\n\
                 {field} = \"mdi-\"\n"
            );
            let message = err(&text);
            assert!(message.contains(&format!("invalid {field}")), "{message}");
        }
    }

    #[test]
    fn state_text_defaults_to_on_and_can_be_turned_off() {
        // The graphic is the reading and the word only confirms it, so on a dense
        // dashboard the word is the part worth dropping.
        let text = format!(
            "{BASE}\n[[device.widget]]\nid = \"lamp\"\nkind = \"beacon\"\ncol = 0\nrow = 0\n"
        );
        assert!(parse(&text).unwrap().devices[0].widgets[0].state_text);
        let quiet = format!("{text}state_text = false\n");
        assert!(!parse(&quiet).unwrap().devices[0].widgets[0].state_text);
    }

    /// A configuration directory under the temp dir, removed when a test ends.
    ///
    /// A guard rather than a bare path because these tests write real files: a
    /// failing assertion unwinds, and a leftover directory would be picked up by
    /// the next run of the same test.
    struct TempConfig {
        dir: PathBuf,
    }

    impl TempConfig {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "paneld-config-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        /// The main configuration file's path, whether or not it exists yet.
        fn main(&self) -> PathBuf {
            self.dir.join("paneld.toml")
        }

        fn write_main(&self, text: &str) {
            std::fs::write(self.main(), text).unwrap();
        }

        /// Writes a file into the drop-in directory, creating the directory if it
        /// is the first one.
        fn write_fragment(&self, name: &str, text: &str) {
            let dir = fragment_dir(&self.main());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), text).unwrap();
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Sets a path's modification time, so a test can say which of the inputs
    /// [`modified_at`] folded rather than race the clock's resolution.
    ///
    /// Opened for reading, which is all a directory can be opened for and all
    /// `futimens` needs from the caller of a file it owns.
    fn stamp(path: &Path, at: SystemTime) {
        std::fs::File::options()
            .read(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(at))
            .unwrap();
    }

    #[test]
    fn load_reads_the_main_file_and_every_fragment() {
        let config = TempConfig::new("load");
        config.write_main(BASE);
        config.write_fragment(
            "10-hallway.toml",
            &device_only().replace("kindle", "hallway"),
        );
        // Neither of these is configuration, and reading either would make the
        // panel's behaviour depend on an editor's habits. Both are invalid TOML, so
        // picking one up fails loudly rather than quietly.
        config.write_fragment("notes.txt", "id =");
        config.write_fragment(".hidden.toml", "id =");

        let loaded = load(&config.main()).unwrap();
        let ids: Vec<&str> = loaded.devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["kindle", "hallway"]);
    }

    #[test]
    fn sources_lists_the_main_file_then_its_fragments_in_name_order() {
        let config = TempConfig::new("sources");
        config.write_main(BASE);
        // Written out of order deliberately: the load order is the sorted one, so
        // that a merge conflict is reported the same way twice running rather than
        // in whatever order the filesystem answered in.
        config.write_fragment("20-later.toml", "");
        config.write_fragment("10-earlier.toml", "");
        config.write_fragment("notes.txt", "");

        let names: Vec<String> = sources(&config.main())
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["paneld.toml", "10-earlier.toml", "20-later.toml"]);
    }

    #[test]
    fn a_fragment_that_is_not_toml_is_named_by_its_path() {
        let config = TempConfig::new("broken");
        config.write_main(BASE);
        config.write_fragment("10-broken.toml", "id =\n");

        let error = load(&config.main()).expect_err("a fragment that is not TOML is not usable");
        assert!(chain(error).contains("10-broken.toml"));
    }

    #[test]
    fn modified_at_folds_the_main_file_the_directory_and_every_fragment() {
        let config = TempConfig::new("modified");
        config.write_main(BASE);
        config.write_fragment("10-hallway.toml", "");

        let main = config.main();
        let dir = fragment_dir(&main);
        let fragment = dir.join("10-hallway.toml");
        let epoch = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        for path in [&main, &dir, &fragment] {
            stamp(path, epoch);
        }
        assert_eq!(modified_at(&main), Some(epoch));

        // Each input in turn is the newest, and each has to be the answer. The
        // directory is the case worth having: adding or deleting a fragment
        // modifies no file, so a check that stats only files keeps the panel
        // rendering a configuration that is no longer on disk.
        let later = epoch + std::time::Duration::from_secs(60);
        for path in [&main, &dir, &fragment] {
            stamp(path, later);
            assert_eq!(
                modified_at(&main),
                Some(later),
                "{} was the newest input",
                path.display()
            );
            stamp(path, epoch);
        }
    }
}
