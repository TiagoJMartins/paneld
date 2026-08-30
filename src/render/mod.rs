//! The render pipeline: a device's configuration plus whatever data is available
//! in, encoded PNG frame bytes out.
//!
//! [`render_frame`] is a pure synchronous function, which is the whole point of
//! its shape. Home Assistant states are fetched by the caller and handed in
//! already resolved, so nothing here awaits, nothing here reads a clock, and the
//! same inputs always produce the same bytes. That determinism is load-bearing
//! rather than tidy: the device caches frames by filename, the filename is a hash
//! of these bytes, and a non-deterministic encoder would make the panel repaint
//! on every poll.
//!
//! Split by concern rather than left as one file: [`types`] is the shared
//! vocabulary (a cell, its ink, the box a body draws in), [`resolve`] turns a
//! widget's configuration plus whatever data exists into a [`types::Cell`],
//! [`scaffold`] lays out the grid and a cell's header, [`body`] draws what is
//! below it, [`rows`] is the column arithmetic a `list` and a weather cell's
//! readings share, and [`paint`] is the drawing primitives both reach for.

mod body;
mod encode;
mod grid;
mod icon;
mod natural;
mod paint;
mod resolve;
mod rows;
mod scaffold;
mod status_bar;
#[cfg(test)]
mod test_support;
mod types;

pub use encode::{frame_hash, quantise_and_encode};
pub use grid::Layout;

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use takumi::prelude::*;
use time::OffsetDateTime;

use crate::config::{Device, Dither, Edge, Palette};
use crate::content::ContentRecord;
use crate::ha::{Reading, Reported};
use crate::icon::Icon;
use crate::telemetry::Telemetry;

use paint::{family, text_node, text_style};
use resolve::truncate;
use scaffold::grid_node;
use types::{Greys, ink, paper};

/// Family name the UI text is registered and requested under.
///
/// Pinned by us rather than taken from the font file, so a vendor renaming their
/// embedded family string cannot silently change what we render with.
pub const UI_FAMILY: &str = "Panel UI";

/// Family name for numeric readouts, where tabular figures stop a changing value
/// from shifting its neighbours around.
pub const NUMERIC_FAMILY: &str = "Panel Numeric";

const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
const INTER_BOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-Bold.ttf");
const MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf");
const MONO_BOLD: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Bold.ttf");

/// Everything one frame is rendered from.
///
/// Every field is already resolved: nothing here is fetched, and nothing reads a
/// clock. That is what makes [`render_frame`] reproducible, which the device's
/// filename-based frame cache depends on.
pub struct RenderInputs<'a> {
    pub device: &'a Device,
    /// Pushed content, keyed by widget id.
    pub content: &'a HashMap<String, ContentRecord>,
    /// Home Assistant readings, keyed by what was read. A reading the latest
    /// request failed to confirm arrives as [`Reported::Held`], carrying the last
    /// value that was, so one unreachable integration mutes a cell rather than
    /// emptying it.
    pub ha_states: &'a HashMap<Reading, Reported>,
    /// Resolved widget icons, keyed by the `icon` spec that asked for them. A
    /// spec absent from this map could not be resolved, and its cell simply
    /// renders without an icon.
    pub icons: &'a HashMap<String, Icon>,
    /// The instant staleness is measured against. A parameter rather than a call
    /// to `now_utc()` so a frame is reproducible.
    pub now: OffsetDateTime,
    /// What the device last told us about itself, for a status bar to show.
    ///
    /// Handed in already resolved like everything else here, and that is the point
    /// of it being a field rather than a store this module reads: a status bar adds
    /// no fetch, no lock and no clock to [`render_frame`], so a frame is still a
    /// pure function of its inputs.
    pub telemetry: &'a Telemetry,
}

/// Builds the embedded font collection.
///
/// Fonts are compiled into the binary and never read from the filesystem, so
/// rendering does not depend on system font configuration. takumi constructs its
/// font collection with system font discovery disabled, so there is no host
/// fallback to accidentally rely on.
pub fn fonts() -> Result<Fonts> {
    let mut fonts = Fonts::default();

    // Registration order matters and is fixed here on purpose: takumi builds its
    // fallback bucket from registration order precisely because iterating a
    // hash map would make font selection vary between renders.
    for (bytes, family, weight) in [
        (INTER_REGULAR, UI_FAMILY, 400.0),
        (INTER_BOLD, UI_FAMILY, 700.0),
        (MONO_REGULAR, NUMERIC_FAMILY, 400.0),
        (MONO_BOLD, NUMERIC_FAMILY, 700.0),
    ] {
        fonts
            .register(
                FontResource::new(FontSource::from_static(bytes)).override_info(FontOverride {
                    family_name: Some(family.into()),
                    weight: Some(weight),
                    ..Default::default()
                }),
            )
            .map_err(|e| anyhow!("registering the embedded {family} {weight} face: {e}"))?;
    }
    Ok(fonts)
}

/// Renders one device's dashboard to encoded PNG bytes.
pub fn render_frame(fonts: &Fonts, inputs: RenderInputs<'_>) -> Result<Vec<u8>> {
    let device = inputs.device;
    let node = dashboard_node(fonts, &inputs);
    let raster = rasterise(fonts, node, device.width, device.height)?;
    let bytes = quantise_and_encode(
        &raster,
        device.width,
        device.height,
        device.palette,
        device.dither,
    )
    .with_context(|| format!("encoding the frame for device `{}`", device.id))?;

    // Advisory: some BYOS clients buffer the whole PNG in RAM before decoding and
    // simply fail to fetch a larger one. Whether that applies is a property of the
    // panel, so it is configured per device and zero switches it off.
    if device.max_frame_bytes > 0 && bytes.len() >= device.max_frame_bytes {
        tracing::warn!(
            device = device.id.as_str(),
            frame_bytes = bytes.len(),
            limit = device.max_frame_bytes,
            "encoded frame reached this device's configured ceiling; a client that \
             buffers the whole frame may fail to fetch it"
        );
    }
    Ok(bytes)
}

/// Renders the frame served when a poll names a device that is not configured.
///
/// Because every configured device is rendered at startup, a known device always
/// has a real frame; reaching this means the base URL is wrong. So the frame's
/// whole job is to make that diagnosable on the panel itself, which is the only
/// screen the owner is looking at.
pub fn render_placeholder(
    fonts: &Fonts,
    requested: &str,
    configured: &[String],
    width: u32,
    height: u32,
    palette: Palette,
    dither: Dither,
) -> Result<Vec<u8>> {
    // The requested dimensions come from device-supplied headers, so they are
    // clamped: a malformed poll must not be able to ask for an enormous buffer.
    let width = width.clamp(1, crate::config::MAX_DIMENSION);
    let height = height.clamp(1, crate::config::MAX_DIMENSION);

    let body = if configured.is_empty() {
        "No devices are configured on this server.".to_owned()
    } else {
        format!("Configured device ids: {}", configured.join(", "))
    };
    let scale = (height as f32 / 480.0).clamp(0.5, 2.5);

    let node = Node::container(vec![
        text_node("Unknown device", text_style(28.0 * scale, 700.0, UI_FAMILY)),
        text_node(
            &truncate(requested, 64),
            text_style(40.0 * scale, 700.0, NUMERIC_FAMILY),
        ),
        text_node(&body, text_style(20.0 * scale, 400.0, UI_FAMILY)),
        text_node(
            "This server does not serve that id. Check the base URL on the device.",
            text_style(17.0 * scale, 400.0, UI_FAMILY),
        ),
    ])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::justify_content(JustifyContent::Center))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::width(Length::Px(width as f32)))
            .with(StyleDeclaration::height(Length::Px(height as f32)))
            .with(StyleDeclaration::padding_left(Length::Px(32.0)))
            .with(StyleDeclaration::padding_right(Length::Px(32.0)))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                14.0 * scale,
            ))))
            .with(StyleDeclaration::background_color(paper()))
            .with(StyleDeclaration::color(ink(Greys::of(&Default::default()))))
            .with(StyleDeclaration::text_align(TextAlign::Center))
            .with(StyleDeclaration::font_family(family(UI_FAMILY))),
    );

    let raster = rasterise(fonts, node, width, height)?;
    quantise_and_encode(&raster, width, height, palette, dither)
        .context("encoding the unknown-device placeholder")
}

/// Rasterises a node tree to straight-alpha RGBA bytes.
fn rasterise(fonts: &Fonts, node: Node, width: u32, height: u32) -> Result<Vec<u8>> {
    let options = RenderOptions::builder()
        // `device_pixel_ratio` is left at its default of 1.0: it multiplies into
        // the output size, and the frame must be exactly the panel's resolution
        // so it is neither letterboxed nor scaled.
        .viewport(Viewport::new((width, height)))
        .node(node)
        .fonts(fonts)
        .build();

    let bitmap = takumi::render(options).map_err(|e| anyhow!("rasterising: {e}"))?;
    anyhow::ensure!(
        (bitmap.width(), bitmap.height()) == (width, height),
        "rasteriser produced {}x{}, expected {width}x{height}",
        bitmap.width(),
        bitmap.height()
    );
    Ok(bitmap.into_raw())
}

/// The whole frame: the widget grid, and the status bar strip beside it when one
/// is configured.
///
/// The background, the text colour and the font family live on this outermost node
/// rather than on the grid, so a status bar inherits exactly what a cell does
/// instead of restating it.
fn dashboard_node(fonts: &Fonts, inputs: &RenderInputs<'_>) -> Node {
    let device = inputs.device;
    let frame = Style::default()
        .with(StyleDeclaration::width(Length::Px(device.width as f32)))
        .with(StyleDeclaration::height(Length::Px(device.height as f32)))
        .with(StyleDeclaration::background_color(paper()))
        .with(StyleDeclaration::color(ink(Greys::of(&device.style))))
        .with(StyleDeclaration::font_family(family(UI_FAMILY)));

    let grid = grid_node(fonts, inputs);
    let Some(bar) = device.status_bar.as_ref() else {
        // Block flow with a single child sized to the whole frame, which is what a
        // barless device rendered as before this wrapper existed. Byte-identical
        // matters rather than being tidy: the panel caches frames by their hash, so
        // a wrapper that shifted one pixel would repaint every panel in service.
        return Node::container(vec![grid])
            .with_style(frame.with(StyleDeclaration::display(Display::Block)));
    };

    // The flex axis crosses the edge the bar sits on: a bar runs *along* its edge,
    // so the bar and the grid stack perpendicular to it. Which of the two comes
    // first is the edge as well — a top or left bar precedes the grid, a bottom or
    // right one follows it.
    let (direction, bar_first) = match bar.edge {
        Edge::Top => (FlexDirection::Column, true),
        Edge::Bottom => (FlexDirection::Column, false),
        Edge::Left => (FlexDirection::Row, true),
        Edge::Right => (FlexDirection::Row, false),
    };
    let bar = status_bar::node(fonts, device, bar, inputs);
    let children = if bar_first {
        vec![bar, grid]
    } else {
        vec![grid, bar]
    };

    Node::container(children).with_style(
        frame
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(direction)),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use crate::config::{Chrome, DEFAULT_MAX_FRAME_BYTES, Fit, Grid, WidgetKind};

    use super::test_support::*;
    use super::*;

    #[test]
    fn every_style_key_moves_the_frame() {
        // The contract that makes the table worth having, and the one a config surface
        // fails quietly: a key that parses, validates, and is then ignored by the
        // renderer reads to its author as "this panel does not care what I asked for".
        // So every key is turned, once, and the frame has to change.
        let shipped = crate::config::Style::SHIPPED;
        let (device, content, states) = styled_panel(shipped);
        let baseline = frame_hash(&render_with(&device, &content, &states, &HashMap::new()));

        let turned: [(&str, crate::config::Style); 16] = [
            (
                "type_scale",
                crate::config::Style {
                    type_scale: 1.6,
                    ..shipped
                },
            ),
            (
                "chrome_scale",
                crate::config::Style {
                    chrome_scale: 0.06,
                    ..shipped
                },
            ),
            (
                "min_type",
                crate::config::Style {
                    min_type: 40.0,
                    ..shipped
                },
            ),
            (
                "reading_ceiling",
                crate::config::Style {
                    reading_ceiling: 1.2,
                    ..shipped
                },
            ),
            (
                "unit_scale",
                crate::config::Style {
                    unit_scale: 0.9,
                    ..shipped
                },
            ),
            (
                "glyph_share",
                crate::config::Style {
                    glyph_share: 0.8,
                    ..shipped
                },
            ),
            (
                "row_type",
                crate::config::Style {
                    row_type: 18.0,
                    ..shipped
                },
            ),
            (
                "row_rule",
                crate::config::Style {
                    row_rule: true,
                    ..shipped
                },
            ),
            (
                "row_fill",
                crate::config::Style {
                    row_rule: true,
                    row_fill: true,
                    ..shipped
                },
            ),
            (
                "row_width",
                crate::config::Style {
                    row_width: crate::config::RowWidth::Full,
                    ..shipped
                },
            ),
            ("ink", crate::config::Style { ink: 60, ..shipped }),
            (
                "muted",
                crate::config::Style {
                    muted: 150,
                    ..shipped
                },
            ),
            (
                "rule",
                crate::config::Style {
                    rule: 200,
                    ..shipped
                },
            ),
            (
                "bar_type_scale",
                crate::config::Style {
                    bar_type_scale: 0.8,
                    ..shipped
                },
            ),
            (
                "bar_gap_scale",
                crate::config::Style {
                    bar_gap_scale: 3.0,
                    ..shipped
                },
            ),
            (
                "bar_margin_scale",
                crate::config::Style {
                    bar_margin_scale: 4.0,
                    ..shipped
                },
            ),
        ];

        for (key, style) in turned {
            let (device, content, states) = styled_panel(style);
            let frame = frame_hash(&render_with(&device, &content, &states, &HashMap::new()));
            assert_ne!(
                frame, baseline,
                "turning style.{key} left the frame byte-identical, so the renderer is \
                 ignoring it"
            );
        }
    }

    #[test]
    fn a_style_saying_nothing_is_the_look_that_shipped() {
        // The other half of the promise, and the reason the defaults are a named
        // constant rather than scattered literals: a dashboard that never mentions
        // style must render what it rendered before the table existed. Asserted
        // against `Style::default()` rather than against a stored hash, because a
        // stored hash would also pass if both sides drifted together.
        let (device, content, states) = styled_panel(crate::config::Style::default());
        let (shipped, shipped_content, shipped_states) =
            styled_panel(crate::config::Style::SHIPPED);

        assert_eq!(
            frame_hash(&render_with(&device, &content, &states, &HashMap::new())),
            frame_hash(&render_with(
                &shipped,
                &shipped_content,
                &shipped_states,
                &HashMap::new()
            )),
        );
    }

    #[test]
    fn embedded_fonts_register() {
        // Guards against a vendored face being replaced with something takumi
        // cannot parse; without fonts every frame would be blank.
        LazyLock::force(&FONTS);
    }

    #[test]
    fn a_frame_renders_at_the_configured_dimensions_with_no_stored_content() {
        let single = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        assert_eq!(
            dimensions(&render(&single, &HashMap::new())),
            (single.width, single.height)
        );

        let multi = device(vec![
            widget("a", WidgetKind::Value, 0, 0),
            widget("b", WidgetKind::Beacon, 1, 0),
            widget("c", WidgetKind::Text, 0, 1),
        ]);
        assert_eq!(dimensions(&render(&multi, &HashMap::new())), (400, 300));
    }

    #[test]
    fn rendering_the_same_inputs_twice_is_byte_identical() {
        // The filename-stability behaviour is only correct if this holds.
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let mut content = HashMap::new();
        content.insert("a".to_owned(), record(serde_json::json!(42), now()));
        assert_eq!(render(&device, &content), render(&device, &content));
    }

    #[test]
    fn a_changed_value_changes_the_bytes() {
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let mut first = HashMap::new();
        first.insert("a".to_owned(), record(serde_json::json!(1), now()));
        let mut second = HashMap::new();
        second.insert("a".to_owned(), record(serde_json::json!(2), now()));
        assert_ne!(render(&device, &first), render(&device, &second));
    }

    #[test]
    fn a_full_kindle_dashboard_renders_under_the_fetch_ceiling() {
        let mut widgets = Vec::new();
        for row in 0..3 {
            for col in 0..4 {
                let mut w = widget(&format!("w{row}{col}"), WidgetKind::Value, col, row);
                w.unit = Some("C".to_owned());
                widgets.push(w);
            }
        }
        const GRID: Grid = Grid {
            cols: 4,
            rows: 3,
            fit: Fit::Stretch,
        };
        let device = Device {
            width: 1024,
            height: 758,
            grid: GRID,
            chrome: Chrome::derived(1024, 758, GRID),
            ..device(widgets)
        };
        let mut content = HashMap::new();
        for row in 0..3 {
            for col in 0..4 {
                content.insert(
                    format!("w{row}{col}"),
                    record(serde_json::json!(20 + row * 4 + col), now()),
                );
            }
        }

        let bytes = render(&device, &content);
        assert_eq!(dimensions(&bytes), (1024, 758));
        assert!(
            bytes.len() < DEFAULT_MAX_FRAME_BYTES,
            "a full dashboard encoded to {} bytes, over the {DEFAULT_MAX_FRAME_BYTES} ceiling",
            bytes.len()
        );
    }

    #[test]
    fn the_placeholder_names_the_requested_and_configured_ids() {
        let configured = vec!["kindle".to_owned(), "kitchen".to_owned()];
        let bytes = render_placeholder(
            &FONTS,
            "kindel",
            &configured,
            1024,
            758,
            Palette::Gray16,
            Dither::Bayer,
        )
        .unwrap();
        assert_eq!(dimensions(&bytes), (1024, 758));

        // Different requested ids must produce different pixels, which is what
        // makes the frame diagnostic rather than decorative.
        let other = render_placeholder(
            &FONTS,
            "kitchn",
            &configured,
            1024,
            758,
            Palette::Gray16,
            Dither::Bayer,
        )
        .unwrap();
        assert_ne!(bytes, other);
    }

    #[test]
    fn the_placeholder_clamps_absurd_dimensions() {
        let bytes = render_placeholder(
            &FONTS,
            "x",
            &["kindle".to_owned()],
            u32::MAX,
            u32::MAX,
            Palette::Mono,
            Dither::Bayer,
        )
        .unwrap();
        let (w, h) = dimensions(&bytes);
        assert_eq!(
            (w, h),
            (crate::config::MAX_DIMENSION, crate::config::MAX_DIMENSION)
        );
    }

    #[test]
    fn the_placeholder_renders_with_no_configured_devices() {
        let bytes =
            render_placeholder(&FONTS, "x", &[], 800, 480, Palette::Mono, Dither::None).unwrap();
        assert_eq!(dimensions(&bytes), (800, 480));
    }

    #[test]
    fn a_status_bar_takes_its_edge_and_leaves_the_grid_the_rest() {
        use crate::config::{StatusBar, StatusField, Timezone};

        // All four edges, because the edge decides both the flex axis and which of
        // the bar and the grid comes first. Getting the order backwards draws the bar
        // over the grid's first row, which on the glass looks exactly like a bar that
        // simply did not render.
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            let mut barred = Device {
                dither: Dither::None,
                status_bar: Some(StatusBar {
                    edge,
                    thickness: 24,
                    fields: vec![StatusField::Device, StatusField::Refresh],
                    alerts: Vec::new(),
                    timezone: Timezone::utc(),
                }),
                ..device(vec![widget("a", WidgetKind::Value, 0, 0)])
            };
            // Spacing comes off the grid *area* and not the frame, which is how
            // `validate_device` derives it once the bar's strip is subtracted. A
            // fixture that kept the frame's spacing would be a dashboard no config
            // file can produce, and its cells a gap away from where they belong.
            let (_, _, area_w, area_h) = barred.grid_area();
            barred.chrome = Chrome::derived(area_w, area_h, barred.grid);
            let content =
                HashMap::from([("a".to_owned(), record(serde_json::json!("21.4"), now()))]);

            let png = render(&barred, &content);
            assert_eq!(
                dimensions(&png),
                (barred.width, barred.height),
                "a {edge} bar must not change the frame's size"
            );
            let (width, _, levels) = greys(&png);

            // The strip inks, and only the bar can have inked it: the grid area starts
            // where the strip ends, and a cell's own rule is a gutter inside that.
            let (bx, by, bw, bh) = barred.status_bar_area().expect("this device has a bar");
            assert!(
                inked(&levels, width, (bx as f32, by as f32, bw as f32, bh as f32)),
                "the {edge} bar must draw inside its own strip"
            );

            // And the grid moved out of its way rather than under it.
            let (x, y, w, h) =
                Layout::for_device(&barred, &nothing_pushed()).rect(&barred.widgets[0]);
            let (gx, gy, gw, gh) = barred.grid_area();
            assert!(
                x >= gx as f32
                    && y >= gy as f32
                    && x + w <= (gx + gw) as f32
                    && y + h <= (gy + gh) as f32,
                "a {edge} bar must shrink the grid, not overlap it: cell at \
                 ({x}, {y}, {w}, {h}) against area ({gx}, {gy}, {gw}, {gh})"
            );
            assert!(
                inked(&levels, width, (x, y, w, h)),
                "the cell beside the {edge} bar must still draw its contents"
            );
        }
    }

    #[test]
    fn a_device_without_a_status_bar_places_its_grid_exactly_as_before() {
        // The barless frame is now a grid inside a wrapper rather than a grid at the
        // root, and it has to be the same frame down to the pixel: the panel caches
        // frames by their hash, so a wrapper that nudged the grid would repaint every
        // device in service for a change nobody asked for and nobody can see.
        let bare = Device {
            dither: Dither::None,
            ..device(vec![widget("a", WidgetKind::Value, 0, 0)])
        };
        assert_eq!(
            bare.grid_area(),
            (0, 0, bare.width, bare.height),
            "with no bar the grid is the whole frame"
        );

        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("21.4"), now()))]);
        let (width, _, levels) = greys(&render(&bare, &content));

        // The cell's top rule traces the rect the layout places it at, and nothing
        // else reaches that line — a cell's content sits a padding inside its own
        // rule. Had the wrapper shifted the grid, the row above would be the inked one.
        let (x, y, w, _) = Layout::for_device(&bare, &nothing_pushed()).rect(&bare.widgets[0]);
        assert!(
            inked(&levels, width, (x + 2.0, y, w - 4.0, 1.0)),
            "the cell's rule must still trace the top edge the layout gives it"
        );
        assert!(
            !inked(&levels, width, (x + 2.0, y - 2.0, w - 4.0, 1.0)),
            "and the row above it must be blank: the wrapper moved nothing"
        );
    }

    #[test]
    fn an_alert_that_is_not_up_costs_the_frame_nothing() {
        // The whole point of an alert over a beacon cell. Three frames: nothing ever
        // pushed, an explicit off, and an on. The first two must be the same bytes —
        // not merely similar — because that is what "takes no space until it fires"
        // means on a panel that repaints when the bytes change. And the third must
        // differ, or the alert never appears at all.
        let quiet = alerting_frame(None);
        let cleared = alerting_frame(Some("off"));
        let raised = alerting_frame(Some("alert"));

        assert_eq!(
            quiet, cleared,
            "a cleared alert must leave the frame exactly as it was before anything \
             was ever pushed"
        );
        assert_ne!(
            quiet, raised,
            "a raised alert has to show up somewhere on the frame"
        );
    }

    #[test]
    fn a_frame_past_its_max_bytes_still_renders_and_only_warns() {
        // The ceiling is advisory: exceeding it must not fail the render, only
        // warn, since some clients never buffer the whole frame anyway.
        let device = Device {
            max_frame_bytes: 1,
            ..device(vec![widget("a", WidgetKind::Value, 0, 0)])
        };
        let bytes = render(&device, &HashMap::new());
        assert!(
            !bytes.is_empty(),
            "a frame over its configured ceiling must still be produced"
        );
    }
}
