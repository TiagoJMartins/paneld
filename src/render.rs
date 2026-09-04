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

mod encode;
mod grid;
mod icon;

pub use encode::{frame_hash, quantise_and_encode};
pub use grid::Layout;

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use takumi::prelude::*;
use time::OffsetDateTime;

use crate::config::{Device, Dither, Palette, Widget, WidgetKind};
use crate::content::{ContentRecord, Row};
use crate::ha::{Reading, Reported};
use crate::icon::Icon;

/// Family name the UI text is registered and requested under.
///
/// Pinned by us rather than taken from the font file, so a vendor renaming their
/// embedded family string cannot silently change what we render with.
pub const UI_FAMILY: &str = "Panel UI";

/// Family name for numeric readouts, where tabular figures stop a changing value
/// from shifting its neighbours around.
pub const NUMERIC_FAMILY: &str = "Panel Numeric";

const INTER_REGULAR: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
const INTER_BOLD: &[u8] = include_bytes!("../assets/fonts/Inter-Bold.ttf");
const MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf");
const MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Bold.ttf");

/// A unit's size relative to the figure it belongs to.
///
/// Slightly smaller, not much: the unit is part of the reading, and shrinking it
/// to caption size makes `23.4 °C` read as a number with a footnote. Big enough
/// to be read at the same glance, small enough that the number is still the thing
/// the eye lands on.
const UNIT_SCALE: f32 = 0.55;

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
            .with(StyleDeclaration::color(ink()))
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

/// The whole dashboard: a CSS grid container with one child per widget.
fn dashboard_node(fonts: &Fonts, inputs: &RenderInputs<'_>) -> Node {
    let device = inputs.device;
    let grid = device.grid;
    let layout = Layout::for_device(device);
    let gutter = layout.gutter();

    let children = device
        .widgets
        .iter()
        .map(|widget| cell_node(fonts, widget, &resolve(widget, inputs), inputs, &layout))
        .collect::<Vec<_>>();

    Node::container(children).with_style(
        Style::default()
            // `Display`'s default is `Inline`, so a container that should lay out
            // as a grid must say so or its children are laid out inline and every
            // grid placement is silently ignored.
            .with(StyleDeclaration::display(Display::Grid))
            .with(StyleDeclaration::width(Length::Px(device.width as f32)))
            .with(StyleDeclaration::height(Length::Px(device.height as f32)))
            .with(StyleDeclaration::grid_template_columns(Some(equal_tracks(
                grid.cols,
            ))))
            .with(StyleDeclaration::grid_template_rows(Some(equal_tracks(
                grid.rows,
            ))))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                gutter,
            ))))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gutter))))
            .with(StyleDeclaration::padding_top(Length::Px(gutter)))
            .with(StyleDeclaration::padding_right(Length::Px(gutter)))
            .with(StyleDeclaration::padding_bottom(Length::Px(gutter)))
            .with(StyleDeclaration::padding_left(Length::Px(gutter)))
            .with(StyleDeclaration::background_color(paper()))
            .with(StyleDeclaration::color(ink()))
            .with(StyleDeclaration::font_family(family(UI_FAMILY))),
    )
}

/// `n` equal-width tracks, i.e. `repeat(n, 1fr)`.
fn equal_tracks(n: u32) -> GridTemplateComponents {
    (0..n)
        .map(|_| GridTemplateComponent::Single(GridTrackSize::Fixed(GridLength::Fr(1.0))))
        .collect()
}

/// One widget's cell, placed explicitly on the grid.
fn cell_node(
    fonts: &Fonts,
    widget: &Widget,
    cell: &Cell,
    inputs: &RenderInputs<'_>,
    layout: &Layout,
) -> Node {
    // The rect the grid will actually hand this widget, rather than cells times
    // span: a spanning cell swallows the gutters it spans over, so computing the
    // box here as `cell * span` measured a 2x2 ten pixels short of where
    // `Layout::rect` places it — the exact disagreement `rect` exists to prevent.
    let (_, _, span_w, span_h) = layout.rect(widget);
    let padding = layout.padding();
    // Padding on both sides, plus the hairline rule on each.
    let chrome = padding * 2.0 + 2.0;
    // What a cell's contents actually have to fit inside.
    let content_w = (span_w - chrome).max(1.0);
    // Chrome is sized to one cell and never to the span: a label is chrome, and a
    // label that grew with its widget's span would set the same word at two sizes
    // on one dashboard.
    let label_px = (layout.cell().1 * 0.11).max(MIN_TYPE_PX);
    let gap = (span_h * 0.03).clamp(2.0, 8.0);

    let header = header_node(fonts, widget, cell, inputs, content_w, label_px);
    // Measured rather than derived from `label_px`: a line box is the layout
    // engine's answer and not the font size, and the body is sized from whatever
    // the header leaves — so an estimate here shows up either as a body that
    // overflows its cell or as a strip of the cell nothing ever uses.
    let content_h = (span_h - chrome - intrinsic_size(fonts, header.clone()).1 - gap).max(1.0);

    let mut children = vec![header];
    // The body sits in its own growing box so that the header is pinned to the top
    // of every cell while the body is centred in whatever space is left. Laying
    // both out in one column instead would centre them as a group, which makes a
    // label's height depend on how tall its neighbour's content happens to be.
    children.push(
        Node::container(body_nodes(
            fonts, &cell.body, cell.ink, content_w, content_h, label_px,
        ))
        .with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(FlexDirection::Column))
                .with(StyleDeclaration::justify_content(JustifyContent::Center))
                .with(StyleDeclaration::align_items(AlignItems::Start))
                .with(StyleDeclaration::flex_grow(Some(FlexGrow(1.0))))
                .with(StyleDeclaration::width(Length::Percentage(100.0)))
                .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                    (span_h * 0.02).clamp(1.0, 6.0),
                )))),
        ),
    );

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::justify_content(JustifyContent::Start))
            .with(StyleDeclaration::align_items(AlignItems::Start))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap))))
            .with(StyleDeclaration::padding_top(Length::Px(padding)))
            .with(StyleDeclaration::padding_right(Length::Px(padding)))
            .with(StyleDeclaration::padding_bottom(Length::Px(padding)))
            .with(StyleDeclaration::padding_left(Length::Px(padding)))
            .with(StyleDeclaration::border_top_width(hairline()))
            .with(StyleDeclaration::border_right_width(hairline()))
            .with(StyleDeclaration::border_bottom_width(hairline()))
            .with(StyleDeclaration::border_left_width(hairline()))
            .with(StyleDeclaration::border_top_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_right_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_left_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_top_color(rule()))
            .with(StyleDeclaration::border_right_color(rule()))
            .with(StyleDeclaration::border_bottom_color(rule()))
            .with(StyleDeclaration::border_left_color(rule()))
            .with(StyleDeclaration::grid_column_start(line(widget.col)))
            .with(StyleDeclaration::grid_column_end(line(
                widget.col + widget.col_span,
            )))
            .with(StyleDeclaration::grid_row_start(line(widget.row)))
            .with(StyleDeclaration::grid_row_end(line(
                widget.row + widget.row_span,
            ))),
    )
}

/// The strip across the top of a cell: its icon and label on the left, and on
/// the right the mark that says the value below is not confirmed current.
///
/// Always emitted, even for a cell with no label and no icon, because the mark
/// has to have somewhere to sit. An empty header collapses to nothing taller than
/// its own zero-height children, so it costs an unlabelled cell no space.
fn header_node(
    fonts: &Fonts,
    widget: &Widget,
    cell: &Cell,
    inputs: &RenderInputs<'_>,
    content_w: f32,
    label_px: f32,
) -> Node {
    let icon = widget.icon.as_ref().and_then(|spec| inputs.icons.get(spec));
    // Kept inside the label's own line box, so a cell going stale does not grow its
    // header and shove the value down. A dashboard where cells shift as sensors
    // come and go reads as broken even when every number on it is right.
    let mark_w = match cell.ink {
        Ink::Held => (label_px * 1.15).min(content_w * 0.2),
        Ink::Current => 0.0,
    };
    // Sized to the label rather than to the cell, so an icon and its label read as
    // one line of chrome however tall the cell is.
    let icon_w = icon.map_or(0.0, |_| label_px * 1.15 + label_px * 0.35);
    let label_w = (content_w - mark_w - icon_w - label_px * 0.3).max(1.0);

    let mut left = Vec::new();
    if let Some(icon) = icon {
        left.push(icon_node(icon, label_px * 1.15, cell.ink));
    }
    if let Some(label) = &widget.label {
        left.push(fitted(fonts, label_w, label_px, |size| {
            text_node(
                label,
                one_line(
                    text_style(size, 700.0, UI_FAMILY)
                        .with(StyleDeclaration::color(muted()))
                        .with(StyleDeclaration::letter_spacing(Length::Px(size * 0.06)))
                        .with(StyleDeclaration::text_transform(TextTransform::Uppercase)),
                ),
            )
        }));
    }

    let mut children = vec![
        Node::container(left).with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                .with(StyleDeclaration::align_items(AlignItems::Center))
                .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                    label_px * 0.35,
                ))))
                // Shrinks before the mark does: the label has already been sized to
                // fit, so if anything still has to give it should not be the mark,
                // which is the only thing saying the value cannot be trusted.
                .with(StyleDeclaration::flex_shrink(Some(FlexGrow(1.0)))),
        ),
    ];
    if cell.ink == Ink::Held {
        children.push(icon_node(
            &Icon::Svg {
                markup: icon::NOT_CONFIRMED.to_owned(),
                ink: None,
            },
            mark_w,
            Ink::Held,
        ));
    }

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::justify_content(
                JustifyContent::SpaceBetween,
            ))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                label_px * 0.3,
            ))))
            .with(StyleDeclaration::width(Length::Percentage(100.0))),
    )
}

/// A zero-based grid coordinate as a CSS grid line.
///
/// Grid lines are 1-based, so line `n` is the start edge of cell `n`. Config
/// bounds the grid well inside `i16`, and named placement is deliberately not
/// used: takumi lowers a named placement to `Auto`, so it would be silently
/// ignored.
fn line(coordinate: u32) -> GridPlacement {
    GridPlacement::Line(coordinate as i16 + 1)
}

/// The nodes that make up a cell below its header.
///
/// Every size here comes out of the content box rather than out of a fraction of
/// the cell capped at some pixel count. Those caps were set against a 400x300 test
/// device and silently bound everything on the 1448x1072 panel in service: a label
/// asked for 45px and got 32, a weather caption asked for 82px and got 34.
fn body_nodes(
    fonts: &Fonts,
    body: &Body,
    ink: Ink,
    content_w: f32,
    content_h: f32,
    label_px: f32,
) -> Vec<Node> {
    match body {
        Body::Figure { text, unit } => {
            // The design size *is* the box's height: a run set at that size overflows
            // it by exactly its line height, so fitting both axes lands the figure on
            // whichever limit actually binds. Fitting width alone, as this did, left
            // the height unused — which on a nearly square cell is most of the cell.
            vec![fitted_box(fonts, content_w, content_h, content_h, |size| {
                figure_node(text, unit.as_deref(), ink, size)
            })]
        }

        Body::Sky { svg, condition } => {
            // Sized as one block, not as a glyph and a caption that each guessed at
            // the cell: the glyph is the reading and the words are its caption, so
            // their ratio belongs in [`sky_node`] and the pair is fitted together.
            // Sizing them apart is what put a 316px glyph and 34px words in a 699x688
            // cell.
            //
            // The centring wrapper is outside the fit because `width: 100%` inside a
            // measured node measures as the measuring viewport, not as the cell.
            vec![
                Node::container(vec![fitted_box(
                    fonts,
                    content_w,
                    content_h,
                    content_h,
                    |size| sky_node(svg, condition, ink, size),
                )])
                .with_style(
                    Style::default()
                        .with(StyleDeclaration::display(Display::Flex))
                        .with(StyleDeclaration::justify_content(JustifyContent::Center))
                        .with(StyleDeclaration::width(Length::Percentage(100.0))),
                ),
            ]
        }

        Body::Beacon { on } => {
            // The dot is the reading, so it takes the height it is given, bounded by
            // the width it has to share with the word beside it.
            let dot = (content_h * 0.42).min(content_w * 0.35).max(14.0);
            vec![
                Node::container(vec![
                    // Drawn as a shape rather than a glyph so the indicator does not
                    // depend on the embedded faces covering any particular symbol.
                    Node::container(Vec::new()).with_style(
                        Style::default()
                            .with(StyleDeclaration::width(Length::Px(dot)))
                            .with(StyleDeclaration::height(Length::Px(dot)))
                            .with(StyleDeclaration::border_top_left_radius(radius(dot)))
                            .with(StyleDeclaration::border_top_right_radius(radius(dot)))
                            .with(StyleDeclaration::border_bottom_right_radius(radius(dot)))
                            .with(StyleDeclaration::border_bottom_left_radius(radius(dot)))
                            .with(StyleDeclaration::background_color(if *on {
                                ink.colour()
                            } else {
                                paper()
                            }))
                            .with(StyleDeclaration::border_top_width(LineWidth::Length(
                                Length::Px(2.0),
                            )))
                            .with(StyleDeclaration::border_right_width(LineWidth::Length(
                                Length::Px(2.0),
                            )))
                            .with(StyleDeclaration::border_bottom_width(LineWidth::Length(
                                Length::Px(2.0),
                            )))
                            .with(StyleDeclaration::border_left_width(LineWidth::Length(
                                Length::Px(2.0),
                            )))
                            .with(StyleDeclaration::border_top_style(BorderStyle::Solid))
                            .with(StyleDeclaration::border_right_style(BorderStyle::Solid))
                            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
                            .with(StyleDeclaration::border_left_style(BorderStyle::Solid))
                            .with(StyleDeclaration::border_top_color(ink.colour()))
                            .with(StyleDeclaration::border_right_color(ink.colour()))
                            .with(StyleDeclaration::border_bottom_color(ink.colour()))
                            .with(StyleDeclaration::border_left_color(ink.colour())),
                    ),
                    fitted(
                        fonts,
                        (content_w - dot * 1.5).max(1.0),
                        (dot * 0.78).max(13.0),
                        |size| {
                            text_node(
                                if *on { "ON" } else { "OFF" },
                                one_line(
                                    text_style(size, 700.0, UI_FAMILY)
                                        .with(StyleDeclaration::color(ink.colour())),
                                ),
                            )
                        },
                    ),
                ])
                .with_style(
                    Style::default()
                        .with(StyleDeclaration::display(Display::Flex))
                        .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                        .with(StyleDeclaration::align_items(AlignItems::Center))
                        .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                            dot * 0.5,
                        )))),
                ),
            ]
        }

        Body::Prose(text) => {
            let prose_px = (content_h * 0.14).max(MIN_TYPE_PX);
            let lines = (content_h / (prose_px * 1.35)).floor();
            vec![text_node(
                text,
                text_style(prose_px, 400.0, UI_FAMILY)
                    .with(StyleDeclaration::color(ink.colour()))
                    .with(StyleDeclaration::line_height(LineHeight::Unitless(1.3)))
                    // Bounded so a long push is clipped to the cell instead of
                    // pushing the layout around.
                    .with(StyleDeclaration::max_lines(Some(
                        (lines as u32).clamp(1, 12),
                    )))
                    .with(StyleDeclaration::text_overflow(TextOverflow::Ellipsis)),
            )]
        }

        Body::Rows(rows) => {
            // Every row plus every gap between them fills the box: a row's line box
            // is about 1.2 of its size and the gaps are 0.28 of it, so `n` rows want
            // `1.48n - 0.28` sizes' worth of height. That is the height's say; the
            // width has one too, or a two-row list in a tall cell is set at 100px
            // and its longest row clips at the cell edge. Each row is fitted on its
            // own and the list set at the smallest, so the rows stay one size.
            let row_px = (content_h / (rows.len().max(1) as f32 * 1.48 - 0.28)).max(MIN_TYPE_PX);
            let row_px = rows.iter().fold(row_px, |px, row| {
                let intrinsic = intrinsic_width(fonts, row_node(row, px, ink));
                fit_size(intrinsic, content_w, px)
            });
            let children = rows
                .iter()
                .map(|row| row_node(row, row_px, ink))
                .collect::<Vec<_>>();
            vec![
                Node::container(children).with_style(
                    Style::default()
                        .with(StyleDeclaration::display(Display::Flex))
                        .with(StyleDeclaration::flex_direction(FlexDirection::Column))
                        .with(StyleDeclaration::width(Length::Percentage(100.0)))
                        .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                            row_px * 0.28,
                        )))),
                ),
            ]
        }

        // An absence is not a reading, so it is set at chrome size rather than filling
        // the cell a value would have filled. Scaling it to the box put `no data`
        // across a 2x2 in 97px type, which shouts about the one cell with nothing to
        // say, and made the same words two sizes on one dashboard.
        Body::Absent(reason) => vec![fitted(
            fonts,
            content_w,
            (label_px * 1.1).max(MIN_TYPE_PX),
            |size| {
                text_node(
                    reason,
                    one_line(
                        text_style(size, 400.0, UI_FAMILY).with(StyleDeclaration::color(muted())),
                    ),
                )
            },
        )],
    }
}

/// A large figure with its unit set beside it.
///
/// The unit shares the figure's line and sits on its baseline, at a little over
/// half its size. Stacking it underneath, as this once did, reads as a caption
/// about the number rather than as part of it — `23.4` and `°C` are one reading,
/// and a panel should say so the way a thermometer does.
///
/// Takes its size rather than choosing one: [`fitted`] decides that, so the unit
/// is measured into the fit rather than estimated around it.
fn figure_node(text: &str, unit: Option<&str>, ink: Ink, size: f32) -> Node {
    let mut children = vec![text_node(
        text,
        one_line(
            text_style(size, 700.0, NUMERIC_FAMILY).with(StyleDeclaration::color(ink.colour())),
        ),
    )];
    if let Some(unit) = unit {
        children.push(
            // Nudged up by the difference in descender depth. `align_items: End`
            // aligns the two runs' *boxes*, and each run's baseline sits its own
            // descender above its box bottom — so the smaller run lands low by
            // exactly the difference. `AlignItems::Baseline` is the property that
            // should do this, but it leaves the unit sitting visibly below the
            // figure, so the correction is applied here where it can be seen and
            // checked.
            Node::container(vec![text_node(
                unit,
                one_line(
                    text_style(size * UNIT_SCALE, 400.0, UI_FAMILY)
                        .with(StyleDeclaration::color(ink.colour())),
                ),
            )])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::padding_bottom(Length::Px(
                        size * (1.0 - UNIT_SCALE) * UNIT_BASELINE_LIFT,
                    ))),
            ),
        );
    }

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::align_items(AlignItems::End))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                size * 0.06,
            )))),
    )
}

/// The sky block: the condition as a glyph, with the condition in words beneath it.
///
/// `size` is the glyph's side and the caption is a fixed fraction of it, so one
/// number sizes the pair and [`fitted_box`] can fit them to a cell together. The
/// two are centred as one block: a big glyph with a short caption under one corner
/// of it reads as two unrelated things sharing a box.
fn sky_node(svg: &str, condition: &str, ink: Ink, size: f32) -> Node {
    let caption_px = size * SKY_CAPTION_SCALE;
    Node::container(vec![
        icon_node(
            &Icon::Svg {
                markup: svg.to_owned(),
                ink: None,
            },
            size,
            ink,
        ),
        text_node(
            condition,
            one_line(
                text_style(caption_px, 400.0, UI_FAMILY)
                    .with(StyleDeclaration::color(ink.colour())),
            ),
        ),
    ])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::justify_content(JustifyContent::Center))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                caption_px * 0.5,
            )))),
    )
}

/// The sky caption's size, as a fraction of the glyph's side.
///
/// The glyph is the reading and the words underneath confirm it, so the caption is
/// deliberately a sixth of the glyph: large enough to read across a room, small
/// enough that it never competes with the picture for the eye.
const SKY_CAPTION_SCALE: f32 = 0.16;

/// How far the unit must be lifted to share the figure's baseline, as a fraction
/// of the size difference between the two runs.
///
/// Two runs in a row are laid out with their boxes bottom-aligned, and each run's
/// baseline sits a fixed fraction of its own font size above its box bottom — so
/// the smaller run lands low in proportion to how much smaller it is. That makes
/// the correction exactly linear in `size * (1 - UNIT_SCALE)`, which is why one
/// coefficient covers every size.
///
/// Measured rather than derived from the faces' metrics: the value that matters is
/// the layout engine's line box, not the font's declared descender, and the two
/// are not the same number. `a_unit_sits_on_the_figures_baseline` is what pins it.
const UNIT_BASELINE_LIFT: f32 = 0.279;

/// Smallest type this will shrink to, in pixels.
///
/// A floor rather than an assertion that everything fits: past this the glyphs
/// stop being readable on the glass, and a reading nobody can read is no better
/// than one that overflows. A value long enough to hit this is a config or
/// publisher problem, and it should look like one.
const MIN_TYPE_PX: f32 = 12.0;

/// Builds a run at the largest size, up to `design`, at which it fits `available`
/// pixels wide.
///
/// Measured, not estimated. The estimate this replaces assumed a fixed advance per
/// character, which in a proportional face is wrong by more than a factor of two
/// between `1` and `W`: it shrank readings that would have fitted and let wide ones
/// overflow, where the one-line bound then cut them off mid-glyph. A clipped
/// reading is the worst failure a panel has, because it looks like a value rather
/// than like an error.
fn fitted(fonts: &Fonts, available: f32, design: f32, build: impl Fn(f32) -> Node) -> Node {
    let intrinsic = intrinsic_width(fonts, build(design));
    let size = fit_size(intrinsic, available, design);
    if size == design {
        return build(design);
    }
    build(size)
}

/// Builds a node at the largest size, up to `design`, at which it fits inside
/// `available_w` by `available_h`.
///
/// Two axes because a cell is a box and not a line. Fitting width alone sized a
/// figure by how many glyphs it happened to have and left the rest of the cell
/// empty; the height is a real constraint too, and on the panel in service it is
/// often the looser of the two, which is where the space went. Whichever axis
/// overflows by more decides, and one corrective step is exact for both because
/// every length in these nodes is proportional to `size`.
fn fitted_box(
    fonts: &Fonts,
    available_w: f32,
    available_h: f32,
    design: f32,
    build: impl Fn(f32) -> Node,
) -> Node {
    let (width, height) = intrinsic_size(fonts, build(design));
    let size = fit_size(width, available_w, design).min(fit_size(height, available_h, design));
    if size == design {
        return build(design);
    }
    build(size)
}

/// The size a run measuring `intrinsic` pixels wide at `design` should be set at
/// to fit `available`.
///
/// One corrective step is exact rather than iterative, because a text run's
/// advance is proportional to its font size: the ratio of overflow *is* the ratio
/// to shrink by. The result is shaved slightly, because a run set to exactly the
/// space it has is a rounding error away from wrapping.
///
/// The readable floor is itself bounded by `design`. A grid dense enough to put a
/// cell's design size under [`MIN_TYPE_PX`] is legal configuration, and a floor
/// above the ceiling is a panic rather than a small glyph.
fn fit_size(intrinsic: f32, available: f32, design: f32) -> f32 {
    if intrinsic <= available || intrinsic <= 0.0 {
        return design;
    }
    (design * available / intrinsic * 0.99).clamp(MIN_TYPE_PX.min(design), design)
}

/// How wide a node wants to be, with nothing constraining it.
fn intrinsic_width(fonts: &Fonts, node: Node) -> f32 {
    intrinsic_size(fonts, node).0
}

/// How large a node wants to be, with nothing constraining it, as `(width,
/// height)`.
///
/// Measured in a viewport far larger than any panel so that no wrapping or
/// shrink-to-fit has kicked in: the answer wanted here is the node's natural size,
/// not what it would do in a box. Returns `(0.0, 0.0)` if measurement fails, which
/// leaves the design size in place — better than refusing to draw the cell.
///
/// **The node is wrapped in a flex row before measuring, and that wrapper is not
/// cosmetic.** A bare text node is block-level, so it reports its *container's*
/// width — against this viewport, 32768px — and every run measured that way was
/// shrunk to [`MIN_TYPE_PX`] regardless of how short it was. That is why a label
/// asking for 45px was set at 12. A flex container is sized to its content, so
/// wrapping asks the question that was meant: how wide is this run?
///
/// Nothing measured here may size itself as a percentage of its container, because
/// against this viewport that resolves to a number with no relation to the cell.
fn intrinsic_size(fonts: &Fonts, node: Node) -> (f32, f32) {
    let probe = Node::container(vec![node]).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::align_items(AlignItems::Start)),
    );
    let options = RenderOptions::builder()
        .viewport(Viewport::new((
            crate::config::MAX_DIMENSION * 8,
            crate::config::MAX_DIMENSION * 8,
        )))
        .node(probe)
        .fonts(fonts)
        .build();
    takumi::measure(options)
        .map(|measured| (measured.width, measured.height))
        .unwrap_or((0.0, 0.0))
}

/// A grey level as a colour, for an icon that asked for one.
fn grey_ink(level: u8) -> ColorInput {
    ColorInput::Value(Color([level, level, level, 255]))
}

/// A square icon node, drawn in `ink` unless the icon asked for a grey of its own.
///
/// SVG markup is handed to the rasteriser with the colour injected as a root
/// `color` presentation attribute rather than left to the cascade. takumi resolves
/// `currentColor` against the SVG root's own `color` when one is set, so this makes
/// an icon's colour a property of the node that asked for it — which is what lets
/// the same weather glyph draw black in a live cell and grey in a held one.
///
/// A held cell always wins over an icon's own grey. The muting is what the corner
/// mark means, and an icon that stayed its configured colour would undercut it.
fn icon_node(icon: &Icon, size: f32, ink: Ink) -> Node {
    let data = match icon {
        Icon::Svg { markup, ink: own } => {
            let colour = match (ink, own) {
                (Ink::Current, Some(grey)) => grey_ink(*grey),
                _ => ink.colour(),
            };
            ImageData {
                src: ImageSourceInput::Url(paint_svg(markup, colour).into()),
                width: None,
                height: None,
            }
        }
        Icon::Raster {
            data,
            width,
            height,
        } => match RgbaImage::new(data.clone(), *width, *height, false) {
            Ok(raw) => ImageData {
                src: ImageSourceInput::Rgba(raw),
                width: None,
                height: None,
            },
            // Decoding already checked the buffer against the dimensions, so this
            // is unreachable from a real icon; an empty box is still better than a
            // panic in the render task.
            Err(_) => return Node::container(Vec::new()),
        },
    };

    Node::image(data).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::width(Length::Px(size)))
            .with(StyleDeclaration::height(Length::Px(size)))
            .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0))))
            .with(StyleDeclaration::object_fit(ObjectFit::Contain)),
    )
}

/// Injects a `color` presentation attribute on an SVG's root element.
///
/// Written as a string edit rather than through a parsed tree because the
/// rasteriser takes markup: it parses this once and caches by content, so handing
/// it text is the cheap path as well as the simple one.
fn paint_svg(markup: &str, colour: ColorInput) -> String {
    let ColorInput::Value(Color([red, green, blue, alpha])) = colour else {
        return markup.to_owned();
    };
    let Some(open) = markup.find("<svg") else {
        return markup.to_owned();
    };
    let at = open + "<svg".len();
    // Only when the tag really ends there, so `<svgfoo` is left alone.
    if !matches!(markup[at..].chars().next(), Some(c) if c == '>' || c == '/' || c.is_whitespace())
    {
        return markup.to_owned();
    }
    format!(
        "{}{}{}",
        &markup[..at],
        format_args!(" color=\"#{red:02x}{green:02x}{blue:02x}{alpha:02x}\""),
        &markup[at..]
    )
}

/// One line of a multi-reading widget: label on the left, value on the right.
fn row_node(row: &Row, size: f32, ink: Ink) -> Node {
    let label = row
        .label
        .clone()
        .or_else(|| row.id.clone())
        .unwrap_or_default();
    let mut value = row.value.as_ref().map(value_text).unwrap_or_default();
    if let Some(unit) = &row.unit {
        value.push(' ');
        value.push_str(unit);
    }

    Node::container(vec![
        text_node(
            &label,
            one_line(text_style(size, 400.0, UI_FAMILY).with(StyleDeclaration::color(muted()))),
        ),
        text_node(
            &value,
            one_line(
                text_style(size, 700.0, NUMERIC_FAMILY).with(StyleDeclaration::color(ink.colour())),
            ),
        ),
    ])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::justify_content(
                JustifyContent::SpaceBetween,
            ))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                size * 0.5,
            ))))
            .with(StyleDeclaration::width(Length::Percentage(100.0))),
    )
}

/// A run of text.
///
/// Takes its complete style, because `Node::with_style` *replaces* a node's style
/// rather than merging into it: chaining a second call silently discards the first,
/// which is how a 96px figure ends up rendering at the inherited 16px.
fn text_node(text: &str, style: Style) -> Node {
    Node::text(text.to_owned()).with_style(style)
}

/// The base style for a run of text, to be extended with `.with(..)`.
fn text_style(size: f32, weight: f32, family_name: &str) -> Style {
    Style::default()
        .with(StyleDeclaration::font_size(FontSize::Length(Length::Px(
            size,
        ))))
        .with(StyleDeclaration::font_weight(FontWeight::Absolute(weight)))
        .with(StyleDeclaration::font_family(family(family_name)))
}

/// Text that must stay on one line rather than wrap out of its cell.
fn one_line(style: Style) -> Style {
    style.with(StyleDeclaration::max_lines(Some(1)))
}

fn family(name: &str) -> FontFamily {
    FontFamily::from_names([name.to_owned()])
}

fn hairline() -> LineWidth {
    LineWidth::Length(Length::Px(1.0))
}

fn radius(diameter: f32) -> SpacePair<Length> {
    SpacePair::from_single(Length::Px(diameter / 2.0))
}

fn paper() -> ColorInput {
    ColorInput::Value(Color([255, 255, 255, 255]))
}

fn ink() -> ColorInput {
    ColorInput::Value(Color([0, 0, 0, 255]))
}

/// Secondary text. Mid grey reads as secondary on a 16-level panel and still
/// resolves to something legible when quantised to fewer levels.
fn muted() -> ColorInput {
    ColorInput::Value(Color([102, 102, 102, 255]))
}

/// Cell rules, light enough not to compete with the content.
fn rule() -> ColorInput {
    ColorInput::Value(Color([170, 170, 170, 255]))
}

/// What a cell shows, resolved from configuration plus whatever data exists.
///
/// Extracted from node building so that "what should this cell say" is decided
/// once, in one readable place, rather than tangled through style declarations.
#[derive(Debug, Clone, PartialEq)]
struct Cell {
    body: Body,
    ink: Ink,
}

/// How much a cell's contents can be trusted.
///
/// A cell renders its last known value either way; this is what stops that being
/// a lie. A held value is drawn in the secondary grey and its cell carries a mark,
/// so "21.4, as of the last time we could ask" is visibly not "21.4, now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ink {
    /// Confirmed by the request or push that produced this frame.
    Current,
    /// The last value that was confirmed, kept because the newest attempt to
    /// confirm it failed.
    Held,
}

impl Ink {
    fn colour(self) -> ColorInput {
        match self {
            Self::Current => ink(),
            Self::Held => muted(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Body {
    /// A large figure with an optional unit.
    Figure { text: String, unit: Option<String> },
    /// A weather condition: its icon, and its name underneath.
    Sky {
        svg: &'static str,
        condition: String,
    },
    /// A two-state indicator.
    Beacon { on: bool },
    /// Free text, wrapped to the cell.
    Prose(String),
    /// A small group of related readings.
    Rows(Vec<Row>),
    /// Nothing has ever been pushed, or nothing has ever been read.
    ///
    /// Distinct from a held value, and the distinction is the point: "no data"
    /// says a publisher has never spoken, which is a wiring problem, whereas a
    /// muted value with a mark says the source is known but currently unreachable.
    Absent(&'static str),
}

fn resolve(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    match widget.kind {
        WidgetKind::HaEntity | WidgetKind::Weather => resolve_ha(widget, inputs),
        _ => resolve_pushed(widget, inputs),
    }
}

/// A cell fed by `PUT /api/content/{id}`.
fn resolve_pushed(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    let Some(record) = inputs.content.get(&widget.id) else {
        return Cell {
            body: Body::Absent("no data"),
            ink: Ink::Current,
        };
    };

    // A publisher that has gone quiet past its `stale_after` keeps its last value
    // on the glass, muted and marked. It is still the most recent thing anyone
    // said, and replacing it with a countdown throws away the only information
    // the cell had.
    let ink = match is_stale(widget, record, inputs.now) {
        true => Ink::Held,
        false => Ink::Current,
    };

    // `rows` is a presentation override available to any kind: when it is present
    // the scalar `value` is ignored, which is what lets one widget show a small
    // group of related readings.
    if let Some(rows) = &record.rows {
        return Cell {
            body: Body::Rows(rows.clone()),
            ink,
        };
    }

    let body = match widget.kind {
        WidgetKind::Value => Body::Figure {
            text: value_text(&record.value),
            unit: record.unit.clone().or_else(|| widget.unit.clone()),
        },
        WidgetKind::Beacon => Body::Beacon {
            on: beacon_is_on(record, &widget.on_values),
        },
        WidgetKind::Text => Body::Prose(value_text(&record.value)),
        WidgetKind::HaEntity | WidgetKind::Weather => unreachable!("handled by resolve_ha"),
    };
    Cell { body, ink }
}

/// A cell read from Home Assistant. A fetch failure degrades this cell only: the
/// frame still renders, because one unreachable integration must not blank the
/// dashboard.
fn resolve_ha(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    let Some(entity) = &widget.entity else {
        // Config validation rejects this, so it cannot happen from a config file.
        return Cell {
            body: Body::Absent("no entity"),
            ink: Ink::Current,
        };
    };
    let reading = match &widget.attribute {
        Some(attribute) => Reading::attribute(entity, attribute),
        None => Reading::state(entity),
    };

    let (value, ink) = match inputs.ha_states.get(&reading) {
        Some(Reported::Fresh(value)) => (value.as_str(), Ink::Current),
        Some(Reported::Held(value)) => (value.as_str(), Ink::Held),
        // Nothing was ever read. A missing key means the caller never asked, which
        // for a validated config means Home Assistant is not configured.
        //
        // Unmarked, unlike a held value: the mark says the value below is not
        // confirmed current, and there is no value below for it to qualify. `no data`
        // and a corner mark say the same thing twice, and the absence is already
        // drawn muted.
        Some(Reported::Lost) | None => {
            return Cell {
                body: Body::Absent("no data"),
                ink: Ink::Current,
            };
        }
    };

    let body = match widget.kind {
        WidgetKind::Weather => match icon::Condition::parse(value) {
            Some(condition) => Body::Sky {
                svg: condition.svg(),
                condition: condition.label().to_owned(),
            },
            // An unrecognised condition still shows what Home Assistant said,
            // because a new condition slug is a thing to notice rather than hide.
            None => Body::Sky {
                svg: icon::UNKNOWN_SKY,
                condition: value.to_owned(),
            },
        },
        _ => Body::Figure {
            text: value.to_owned(),
            unit: widget.unit.clone(),
        },
    };
    Cell { body, ink }
}

/// Whether a pushed record is older than its widget's `stale_after`.
///
/// Computed at render time rather than stamped at push time, so raising or
/// lowering `stale_after` takes effect on the next frame. A record stamped in the
/// future is never stale: that is a clock disagreement, not freshness
/// information, and treating it as stale would mute a cell that is fine.
fn is_stale(widget: &Widget, record: &ContentRecord, now: OffsetDateTime) -> bool {
    if widget.stale_after == 0 {
        return false;
    }
    let age = now - record.received_at;
    age.whole_seconds() >= 0 && age.unsigned_abs().as_secs() > widget.stale_after
}

/// Whether a beacon reads as "on".
///
/// `state` takes precedence: when a publisher sends one it is the authoritative
/// signal, so a non-matching `state` means off rather than falling through to
/// `value`. Falling through would make `{"state":"idle","value":"on"}` read as on,
/// which is not what the publisher said.
fn beacon_is_on(record: &ContentRecord, on_values: &[String]) -> bool {
    let candidate = match &record.state {
        Some(state) => state.clone(),
        None => value_text(&record.value),
    };
    on_values
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(candidate.trim()))
}

/// Renders a pushed JSON value as display text.
fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "\u{2014}".to_owned(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Truncates to a character count, so a hostile device id cannot blow up the
/// placeholder's layout.
fn truncate(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    text.chars().take(chars).collect::<String>() + "\u{2026}"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_FRAME_BYTES;
    use crate::config::{Grid, Palette};
    use std::sync::LazyLock;
    use time::Duration;

    /// One font collection for the whole test module: registration is the
    /// expensive part and it is immutable once built.
    static FONTS: LazyLock<Fonts> = LazyLock::new(|| fonts().expect("embedded fonts must load"));

    fn device(widgets: Vec<Widget>) -> Device {
        Device {
            id: "kindle".to_owned(),
            width: 400,
            height: 300,
            palette: Palette::Gray16,
            dither: Dither::Atkinson,
            refresh_rate: 300,
            render_interval: 300,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            grid: Grid { cols: 2, rows: 2 },
            widgets,
            sink: None,
        }
    }

    fn widget(id: &str, kind: WidgetKind, col: u32, row: u32) -> Widget {
        Widget {
            id: id.to_owned(),
            kind,
            col,
            row,
            col_span: 1,
            row_span: 1,
            label: Some(id.to_owned()),
            unit: None,
            stale_after: 0,
            entity: None,
            attribute: None,
            on_values: vec!["on".to_owned(), "true".to_owned(), "alert".to_owned()],
            icon: None,
            tap: None,
        }
    }

    /// An `ha_entity` widget reading `entity`'s own state.
    fn ha_widget(id: &str, entity: &str, kind: WidgetKind) -> Widget {
        Widget {
            entity: Some(entity.to_owned()),
            ..widget(id, kind, 0, 0)
        }
    }

    fn record(value: serde_json::Value, received_at: OffsetDateTime) -> ContentRecord {
        ContentRecord {
            value,
            state: None,
            unit: None,
            rows: None,
            received_at,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn render(device: &Device, content: &HashMap<String, ContentRecord>) -> Vec<u8> {
        render_with(device, content, &HashMap::new(), &HashMap::new())
    }

    fn render_with(
        device: &Device,
        content: &HashMap<String, ContentRecord>,
        ha_states: &HashMap<Reading, Reported>,
        icons: &HashMap<String, Icon>,
    ) -> Vec<u8> {
        render_frame(
            &FONTS,
            RenderInputs {
                device,
                content,
                ha_states,
                icons,
                now: now(),
            },
        )
        .expect("frame should render")
    }

    /// The cell one widget resolves to, given only Home Assistant readings.
    fn resolved(widget: &Widget, ha_states: &HashMap<Reading, Reported>) -> Cell {
        let device = device(vec![widget.clone()]);
        resolve(
            widget,
            &RenderInputs {
                device: &device,
                content: &HashMap::new(),
                ha_states,
                icons: &HashMap::new(),
                now: now(),
            },
        )
    }

    fn dimensions(png: &[u8]) -> (u32, u32) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let reader = decoder.read_info().expect("should be a PNG");
        (reader.info().width, reader.info().height)
    }

    #[test]
    fn embedded_fonts_register() {
        // Guards against a vendored face being replaced with something takumi
        // cannot parse; without fonts every frame would be blank.
        LazyLock::force(&FONTS);
    }

    #[test]
    fn output_is_a_png_at_the_configured_dimensions() {
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let bytes = render(&device, &HashMap::new());
        assert_eq!(dimensions(&bytes), (device.width, device.height));
    }

    #[test]
    fn a_widget_with_no_stored_content_renders_without_error() {
        let device = device(vec![
            widget("a", WidgetKind::Value, 0, 0),
            widget("b", WidgetKind::Beacon, 1, 0),
            widget("c", WidgetKind::Text, 0, 1),
        ]);
        let bytes = render(&device, &HashMap::new());
        assert_eq!(dimensions(&bytes), (400, 300));
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
    fn a_stale_widget_renders_differently_from_a_fresh_one() {
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let device = device(vec![w]);

        let mut fresh = HashMap::new();
        fresh.insert("a".to_owned(), record(serde_json::json!(42), now()));
        let mut stale = HashMap::new();
        stale.insert(
            "a".to_owned(),
            record(serde_json::json!(42), now() - Duration::seconds(3_600)),
        );

        assert_ne!(
            render(&device, &fresh),
            render(&device, &stale),
            "a stale widget must not render its last value as though current"
        );
    }

    #[test]
    fn stale_after_zero_disables_the_staleness_timer() {
        let w = widget("a", WidgetKind::Value, 0, 0);
        assert_eq!(w.stale_after, 0);
        let ancient = record(serde_json::json!(1), now() - Duration::days(400));
        assert!(!is_stale(&w, &ancient, now()));
    }

    #[test]
    fn staleness_triggers_strictly_after_the_window() {
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let at_limit = record(serde_json::json!(1), now() - Duration::seconds(60));
        assert!(
            !is_stale(&w, &at_limit, now()),
            "exactly at the window is still fresh"
        );
        let past = record(serde_json::json!(1), now() - Duration::seconds(61));
        assert!(is_stale(&w, &past, now()));
    }

    #[test]
    fn a_record_from_the_future_is_not_stale() {
        // A publisher's clock is irrelevant here because we stamp receipt
        // ourselves, but a clock step backwards on this host must not read as a
        // negative age.
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let ahead = record(serde_json::json!(1), now() + Duration::seconds(500));
        assert!(!is_stale(&w, &ahead, now()));
    }

    #[test]
    fn a_stale_push_keeps_its_value_and_is_marked_not_confirmed() {
        // The whole point of holding a value: the last thing a publisher said is
        // still the best answer the panel has, so it stays on the glass, muted,
        // rather than being replaced by a countdown that says nothing.
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let device = device(vec![w.clone()]);
        let mut content = HashMap::new();
        content.insert(
            "a".to_owned(),
            record(serde_json::json!(42), now() - Duration::seconds(3_600)),
        );

        let cell = resolve(
            &w,
            &RenderInputs {
                device: &device,
                content: &content,
                ha_states: &HashMap::new(),
                icons: &HashMap::new(),
                now: now(),
            },
        );
        assert_eq!(
            cell,
            Cell {
                body: Body::Figure {
                    text: "42".to_owned(),
                    unit: None
                },
                ink: Ink::Held,
            }
        );
    }

    #[test]
    fn a_beacon_matches_state_before_value() {
        let on_values = vec!["on".to_owned(), "alert".to_owned()];

        let mut alerting = record(serde_json::json!("off"), now());
        alerting.state = Some("alert".to_owned());
        assert!(
            beacon_is_on(&alerting, &on_values),
            "state decides when present"
        );

        // A non-matching state means off. Falling through to `value` would report
        // on for a publisher that explicitly said it was idle.
        let mut contradictory = record(serde_json::json!("on"), now());
        contradictory.state = Some("idle".to_owned());
        assert!(!beacon_is_on(&contradictory, &on_values));
    }

    #[test]
    fn a_beacon_falls_back_to_value_when_no_state_is_pushed() {
        let on_values = vec!["on".to_owned(), "true".to_owned()];
        assert!(beacon_is_on(
            &record(serde_json::json!("on"), now()),
            &on_values
        ));
        assert!(beacon_is_on(
            &record(serde_json::json!(true), now()),
            &on_values
        ));
        assert!(!beacon_is_on(
            &record(serde_json::json!("off"), now()),
            &on_values
        ));
        assert!(!beacon_is_on(
            &record(serde_json::json!(false), now()),
            &on_values
        ));
    }

    #[test]
    fn beacon_matching_ignores_case_and_surrounding_space() {
        let on_values = vec!["on".to_owned()];
        assert!(beacon_is_on(
            &record(serde_json::json!(" ON "), now()),
            &on_values
        ));
    }

    #[test]
    fn beacon_states_render_differently() {
        let device = device(vec![widget("a", WidgetKind::Beacon, 0, 0)]);
        let mut on = HashMap::new();
        on.insert("a".to_owned(), record(serde_json::json!("on"), now()));
        let mut off = HashMap::new();
        off.insert("a".to_owned(), record(serde_json::json!("off"), now()));
        assert_ne!(render(&device, &on), render(&device, &off));
    }

    #[test]
    fn renders_every_value_shape() {
        assert_eq!(value_text(&serde_json::json!("text")), "text");
        assert_eq!(value_text(&serde_json::json!(7)), "7");
        assert_eq!(value_text(&serde_json::json!(1.5)), "1.5");
        assert_eq!(value_text(&serde_json::json!(true)), "true");
        assert_eq!(value_text(&serde_json::Value::Null), "\u{2014}");
    }

    #[test]
    fn rows_replace_the_scalar_value() {
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let mut with_rows = HashMap::new();
        let mut rec = record(serde_json::json!("ignored"), now());
        rec.rows = Some(vec![
            Row {
                id: Some("one".to_owned()),
                label: Some("One".to_owned()),
                value: Some(serde_json::json!(1)),
                unit: Some("C".to_owned()),
                state: None,
            },
            Row {
                id: None,
                label: Some("Two".to_owned()),
                value: Some(serde_json::json!(2)),
                unit: None,
                state: None,
            },
        ]);
        with_rows.insert("a".to_owned(), rec);

        let mut scalar = HashMap::new();
        scalar.insert("a".to_owned(), record(serde_json::json!("ignored"), now()));

        assert_ne!(render(&device, &with_rows), render(&device, &scalar));
        assert_eq!(dimensions(&render(&device, &with_rows)), (400, 300));
    }

    #[test]
    fn a_home_assistant_failure_holds_the_last_value_rather_than_blanking_the_cell() {
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        let device = device(vec![w.clone()]);
        let reading = Reading::state("sensor.office");

        let held = HashMap::from([(reading.clone(), Reported::Held("21.4".to_owned()))]);
        let fresh = HashMap::from([(reading, Reported::Fresh("21.4".to_owned()))]);

        let muted_frame = render_with(&device, &HashMap::new(), &held, &HashMap::new());
        let live_frame = render_with(&device, &HashMap::new(), &fresh, &HashMap::new());

        assert_eq!(dimensions(&muted_frame), (400, 300));
        assert_ne!(
            muted_frame, live_frame,
            "a held value must be visibly distinct from a confirmed one"
        );
    }

    #[test]
    fn a_held_reading_keeps_its_value_and_a_lost_one_says_so() {
        // The distinction that matters: "the request failed but I know what it said
        // last" is a muted figure, whereas "nothing has ever been read" is an
        // absence. Collapsing both to the word `unavailable`, as this once did,
        // threw away the reading a viewer actually wanted.
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        let reading = Reading::state("sensor.office");

        assert_eq!(
            resolved(
                &w,
                &HashMap::from([(reading.clone(), Reported::Held("21.4".to_owned()))])
            ),
            Cell {
                body: Body::Figure {
                    text: "21.4".to_owned(),
                    unit: None
                },
                ink: Ink::Held,
            }
        );
        assert_eq!(
            resolved(
                &w,
                &HashMap::from([(reading.clone(), Reported::Fresh("21.4".to_owned()))])
            ),
            Cell {
                body: Body::Figure {
                    text: "21.4".to_owned(),
                    unit: None
                },
                ink: Ink::Current,
            }
        );
        // Lost carries no mark: `Ink::Current` here is not "this is fresh" but "there
        // is nothing for the mark to qualify". The absence is drawn muted either way.
        assert_eq!(
            resolved(&w, &HashMap::from([(reading, Reported::Lost)])),
            Cell {
                body: Body::Absent("no data"),
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_reading_that_was_never_fetched_reads_as_no_data() {
        // A missing key means the caller never asked, which for a validated config
        // means Home Assistant is not configured at all.
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        assert_eq!(
            resolved(&w, &HashMap::new()),
            Cell {
                body: Body::Absent("no data"),
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_weather_cell_draws_a_condition_as_an_icon_and_a_name() {
        // `partlycloudy` in the tabular-numeric figure style was the defect: a word
        // from a closed set put where a number goes.
        let w = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        let reading = Reading::state("weather.braga");

        let cell = resolved(
            &w,
            &HashMap::from([(reading.clone(), Reported::Fresh("partlycloudy".to_owned()))]),
        );
        assert_eq!(
            cell,
            Cell {
                body: Body::Sky {
                    svg: icon::Condition::PartlyCloudy.svg(),
                    condition: "Partly cloudy".to_owned(),
                },
                ink: Ink::Current,
            }
        );

        // An unrecognised slug still shows what Home Assistant said, because a new
        // condition is a thing to notice rather than to hide.
        let unknown = resolved(
            &w,
            &HashMap::from([(reading, Reported::Fresh("meteor-shower".to_owned()))]),
        );
        assert_eq!(
            unknown,
            Cell {
                body: Body::Sky {
                    svg: icon::UNKNOWN_SKY,
                    condition: "meteor-shower".to_owned(),
                },
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn every_weather_condition_renders_a_frame() {
        // The rasteriser silently draws nothing for markup usvg rejects, so the
        // only assertion worth making is that a real frame comes out with ink in it.
        let mut w = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        w.col_span = 2;
        w.row_span = 2;
        let device = device(vec![w]);
        let reading = Reading::state("weather.braga");

        let mut frames = std::collections::HashSet::new();
        for slug in [
            "clear-night",
            "cloudy",
            "exceptional",
            "fog",
            "hail",
            "lightning",
            "lightning-rainy",
            "partlycloudy",
            "pouring",
            "rainy",
            "snowy",
            "snowy-rainy",
            "sunny",
            "windy",
            "windy-variant",
        ] {
            let ha = HashMap::from([(reading.clone(), Reported::Fresh(slug.to_owned()))]);
            let frame = render_with(&device, &HashMap::new(), &ha, &HashMap::new());
            assert_eq!(dimensions(&frame), (400, 300), "rendering {slug}");
            frames.insert(frame);
        }
        assert_eq!(
            frames.len(),
            15,
            "every condition must be visually distinguishable, so no two frames may match"
        );
    }

    /// The lowest inked row per column of a rasterised node, split into the run
    /// left of the widest internal gap and the run right of it.
    ///
    /// The baseline is taken as the *mode* of those rows rather than the maximum, so
    /// a descender, a decimal point or a curve's overshoot does not move it.
    fn baselines(node: Node, width: u32, height: u32) -> (u32, u32) {
        let raster = rasterise(&FONTS, node, width, height).expect("should rasterise");
        let dark = |x: u32, y: u32| raster[((y * width + x) * 4 + 3) as usize] > 128;

        let mut lowest = Vec::new();
        for x in 0..width {
            let low = (0..height).rfind(|&y| dark(x, y));
            lowest.push(low);
        }

        // The widest run of empty columns between two inked ones is the gap between
        // the figure and its unit.
        let inked: Vec<u32> = (0..width)
            .filter(|&x| lowest[x as usize].is_some())
            .collect();
        assert!(inked.len() > 4, "nothing was drawn");
        let (mut split, mut widest) = (0, 0);
        for pair in inked.windows(2) {
            let gap = pair[1] - pair[0];
            if gap > widest {
                widest = gap;
                split = pair[0] + gap / 2;
            }
        }
        assert!(widest > 1, "the figure and its unit are not separable");

        let mode = |from: u32, to: u32| {
            let mut counts: HashMap<u32, usize> = HashMap::new();
            for x in from..to {
                if let Some(y) = lowest[x as usize] {
                    *counts.entry(y).or_default() += 1;
                }
            }
            *counts
                .iter()
                .max_by_key(|(y, n)| (*n, std::cmp::Reverse(**y)))
                .expect("a run must have ink")
                .0
        };
        (mode(0, split), mode(split, width))
    }

    /// How much of a widget's cell its content actually inks, as fractions of the
    /// cell's width and height.
    ///
    /// Measured off the rasterised frame rather than from the style declarations,
    /// because the number that matters is the one on the glass: every size in a cell
    /// is chosen by measurement and fitting, so nothing short of pixels says whether
    /// a reading used the cell it was given.
    fn cell_fill(
        device: &Device,
        widget_id: &str,
        content: &HashMap<String, ContentRecord>,
        ha: &HashMap<Reading, Reported>,
    ) -> (f32, f32) {
        let widget = device
            .widgets
            .iter()
            .find(|w| w.id == widget_id)
            .expect("the widget must be on the device");
        let inputs = RenderInputs {
            device,
            content,
            ha_states: ha,
            icons: &HashMap::new(),
            now: now(),
        };
        let raster = rasterise(
            &FONTS,
            dashboard_node(&FONTS, &inputs),
            device.width,
            device.height,
        )
        .expect("should rasterise");

        let (x, y, w, h) = Layout::for_device(device).rect(widget);
        // Inset past the cell's own rule, which would otherwise ink every cell fully.
        let (x0, y0) = ((x + 3.0) as u32, (y + 3.0) as u32);
        let (x1, y1) = ((x + w - 3.0) as u32, (y + h - 3.0) as u32);
        let dark = |x: u32, y: u32| raster[((y * device.width + x) * 4) as usize] < 128;

        let inked_x: Vec<u32> = (x0..x1).filter(|&x| (y0..y1).any(|y| dark(x, y))).collect();
        let inked_y: Vec<u32> = (y0..y1).filter(|&y| (x0..x1).any(|x| dark(x, y))).collect();
        assert!(
            !inked_x.is_empty() && !inked_y.is_empty(),
            "widget `{widget_id}` inked nothing"
        );
        (
            (inked_x[inked_x.len() - 1] - inked_x[0] + 1) as f32 / (x1 - x0) as f32,
            (inked_y[inked_y.len() - 1] - inked_y[0] + 1) as f32 / (y1 - y0) as f32,
        )
    }

    /// The panel in service: a 6-inch Kindle Paperwhite, landscape, on a 4x3 grid.
    fn panel(widgets: Vec<Widget>) -> Device {
        Device {
            width: 1448,
            height: 1072,
            grid: Grid { cols: 4, rows: 3 },
            ..device(widgets)
        }
    }

    #[test]
    fn a_reading_fills_the_cell_it_is_given() {
        // The defect this pins: every type size was a fraction of the cell capped at
        // a pixel count chosen against the 400x300 test device, and fitting only ever
        // shrank to width. On the real panel a 2x2 weather cell is 699x688 and held a
        // 316px glyph with 34px words — under half of each axis — while a figure cell
        // used 40% of its height. Both axes are asserted because either one alone
        // passes with the cell half empty.
        let mut weather = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        weather.col_span = 2;
        weather.row_span = 2;
        let mut figure = widget("temp", WidgetKind::Value, 2, 0);
        figure.unit = Some("\u{b0}C".to_owned());
        let device = panel(vec![weather, figure]);

        let content =
            HashMap::from([("temp".to_owned(), record(serde_json::json!("23.4"), now()))]);
        let ha = HashMap::from([(
            Reading::state("weather.braga"),
            Reported::Fresh("partlycloudy".to_owned()),
        )]);

        let (sky_w, sky_h) = cell_fill(&device, "sky", &content, &ha);
        assert!(
            sky_w > 0.7 && sky_h > 0.7,
            "the weather block must use its cell: {sky_w:.2} of the width, {sky_h:.2} \
             of the height"
        );

        let (figure_w, figure_h) = cell_fill(&device, "temp", &content, &ha);
        assert!(
            figure_w > 0.9,
            "a figure is width-bound on this grid, so it must use nearly all of it: \
             {figure_w:.2}"
        );
        // Not higher, and this is the honest limit of the fix: a four-glyph reading
        // with a unit is width-bound on a 350x344 cell, so the leftover height is
        // structural. Buying it back means a wider cell — a grid choice, not a
        // rendering one.
        assert!(
            figure_h > 0.55,
            "and it must still use over half the height it is given: {figure_h:.2}"
        );
    }

    #[test]
    fn a_unit_sits_on_the_figures_baseline() {
        // Measured in pixels because this is the kind of thing that looks right in
        // the style declarations and wrong on the glass: `align_items: Baseline`
        // does not give two runs at different sizes a shared baseline here, and the
        // unit ended up sitting a quarter of its height below the number it belongs
        // to.
        //
        // Both runs are the same digits on purpose. The question is where the layout
        // puts a smaller run, and a real unit string would confound the measurement:
        // the lowest inked row under `°` is the degree sign's bottom, nowhere near
        // the baseline, and the `p` of `hPa` has a descender below it.
        for size in [40.0, 60.0, 100.0, 143.0, 168.0] {
            let node = figure_node("88", Some("88"), Ink::Current, size);
            let (value, unit) = baselines(node, 1_400, 500);
            let delta = unit as i64 - value as i64;
            assert!(
                delta.abs() <= 2,
                "at {size}px the unit's baseline is {delta:+}px off the figure's \
                 ({value} vs {unit})"
            );
        }
    }

    #[test]
    fn a_painted_icon_carries_exactly_one_colour_attribute() {
        // The composed result of both stages. `icon::decode` sets
        // `fill="currentColor"` on a single-colour icon and this sets the `color`
        // that `currentColor` resolves against. When both wrote a colour there were
        // two `color` attributes on one element — malformed XML that usvg rejects
        // outright, so every icon on the dashboard drew nothing and no test noticed.
        let fetched = r#"<svg fill="currentColor" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M0 0h8v8z"/></svg>"#;
        let painted = paint_svg(fetched, muted());
        assert_eq!(
            painted.matches("color=").count(),
            1,
            "exactly one colour attribute may reach the rasteriser: {painted}"
        );
        assert!(painted.contains(r##"color="#666666ff""##), "{painted}");

        // And it must still draw: markup usvg rejects rasterises to nothing at all,
        // which is the failure mode this whole pair of tests exists to catch.
        let node = icon_node(
            &Icon::Svg {
                markup: fetched.to_owned(),
                ink: None,
            },
            48.0,
            Ink::Current,
        );
        let raster = rasterise(&FONTS, node, 48, 48).expect("should rasterise");
        let inked = raster.chunks_exact(4).filter(|px| px[3] > 128).count();
        assert!(
            inked > 20,
            "a painted icon must actually draw ink, got {inked}"
        );
    }

    #[test]
    fn a_widget_icon_changes_the_frame_and_a_missing_one_does_not_fail_it() {
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.icon = Some("mdi-thermometer".to_owned());
        let device = device(vec![w]);

        let unresolved = render_with(&device, &HashMap::new(), &HashMap::new(), &HashMap::new());
        let icons = HashMap::from([(
            "mdi-thermometer".to_owned(),
            Icon::Svg {
                markup: r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="9"/></svg>"#
                    .to_owned(),
                ink: None,
            },
        )]);
        let resolved_frame = render_with(&device, &HashMap::new(), &HashMap::new(), &icons);

        assert_eq!(dimensions(&unresolved), (400, 300));
        assert_ne!(
            unresolved, resolved_frame,
            "an icon that resolved must actually be drawn"
        );
    }

    #[test]
    fn a_long_figure_scales_down_rather_than_being_clipped() {
        // Measured fitting, not a character-count estimate: the point is that the
        // run's real advance decides the size, so nothing is ever cut mid-glyph.
        let short = figure_px_for("7", None);
        let long = figure_px_for("123456789", None);
        assert!(
            long < short,
            "a wide reading must shrink: {long} should be under {short}"
        );

        // And the unit is counted in, so adding one shrinks the figure further.
        let bare = figure_px_for("1234", None);
        let with_unit = figure_px_for("1234", Some("kWh"));
        assert!(
            with_unit <= bare,
            "a unit competes for the same width: {with_unit} should not exceed {bare}"
        );
    }

    /// The size a figure settles on inside a 200px-wide box, measured with the real
    /// font metrics rather than estimated from a character count.
    fn figure_px_for(text: &str, unit: Option<&str>) -> f32 {
        const DESIGN: f32 = 96.0;
        let intrinsic = intrinsic_width(&FONTS, figure_node(text, unit, Ink::Current, DESIGN));
        assert!(intrinsic > 0.0, "measuring {text:?} must produce a width");
        fit_size(intrinsic, 200.0, DESIGN)
    }

    #[test]
    fn a_figure_that_already_fits_keeps_the_design_size() {
        assert_eq!(figure_px_for("7", None), 96.0);
    }

    #[test]
    fn fitting_never_clamps_to_a_floor_above_its_ceiling() {
        // A 40x40 cell is legal configuration and puts the design size under the
        // readable floor. Clamping to a floor above the ceiling panics, and a panic
        // in the render task is the one failure mode that leaves a stale frame on
        // the glass with nothing to say why.
        //
        // Below the floor the answer is the design size: shrinking an 11px reading
        // to fit would take it to a fraction of a pixel, so it is better to set it
        // as designed and let the one-line bound clip what will not fit.
        assert_eq!(fit_size(500.0, 10.0, 11.0), 11.0);

        // Above the floor, shrinking happens and stops at the floor.
        assert_eq!(fit_size(1_000.0, 10.0, 96.0), MIN_TYPE_PX);
        assert!(fit_size(200.0, 100.0, 96.0) < 96.0);
    }

    #[test]
    fn an_ha_entity_ignores_pushed_content() {
        // The kind reads from Home Assistant, so a push to the same id must not
        // masquerade as the entity's state.
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        let device = device(vec![w.clone()]);
        let mut content = HashMap::new();
        content.insert("temp".to_owned(), record(serde_json::json!("99"), now()));
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("21.4".to_owned()),
        )]);

        assert_eq!(
            resolve(
                &w,
                &RenderInputs {
                    device: &device,
                    content: &content,
                    ha_states: &ha,
                    icons: &HashMap::new(),
                    now: now(),
                }
            ),
            Cell {
                body: Body::Figure {
                    text: "21.4".to_owned(),
                    unit: None
                },
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_spanning_widget_renders_and_differs_from_a_single_cell() {
        let mut wide = widget("a", WidgetKind::Value, 0, 0);
        wide.col_span = 2;
        let spanning = device(vec![wide]);
        let single = device(vec![widget("a", WidgetKind::Value, 0, 0)]);

        let mut content = HashMap::new();
        content.insert("a".to_owned(), record(serde_json::json!(42), now()));

        assert_ne!(
            render(&spanning, &content),
            render(&single, &content),
            "grid placement must actually affect the raster"
        );
    }

    #[test]
    fn widget_position_affects_the_raster() {
        // Guards the single biggest layout footgun: if grid placement were being
        // ignored, these two would be identical.
        let mut content = HashMap::new();
        content.insert("a".to_owned(), record(serde_json::json!(42), now()));
        let top_left = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let bottom_right = device(vec![widget("a", WidgetKind::Value, 1, 1)]);
        assert_ne!(render(&top_left, &content), render(&bottom_right, &content));
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
        let device = Device {
            width: 1024,
            height: 758,
            grid: Grid { cols: 4, rows: 3 },
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
    fn truncates_an_over_long_device_id() {
        assert_eq!(truncate("short", 64), "short");
        assert_eq!(truncate("abcdef", 3), "abc\u{2026}");
    }
}
