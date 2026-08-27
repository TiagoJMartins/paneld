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
mod natural;
mod status_bar;

pub use encode::{frame_hash, quantise_and_encode};
pub use grid::Layout;

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use takumi::prelude::*;
use time::OffsetDateTime;

use crate::config::{Device, Dither, Edge, Fit, Palette, Widget, WidgetKind};
use crate::content::{ContentRecord, Row};
use crate::ha::{Reading, Reported};
use crate::icon::Icon;
use crate::telemetry::Telemetry;

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

/// The widget grid: a CSS grid container with one child per widget, sized to the
/// area [`Device::grid_area`] left it rather than to the frame.
///
/// Sized from the area and never from the device, because those differ by exactly
/// the strip a status bar took. Deriving the size here instead would be a second
/// copy of that arithmetic, and the copy that drifted would put every cell
/// somewhere the tap hit test does not look.
fn grid_node(fonts: &Fonts, inputs: &RenderInputs<'_>) -> Node {
    let device = inputs.device;
    let grid = device.grid;
    let layout = Layout::for_device(device, inputs.content);
    let gutter = layout.gutter();
    let (_, _, area_w, area_h) = device.grid_area();

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
            .with(StyleDeclaration::width(Length::Px(area_w as f32)))
            .with(StyleDeclaration::height(Length::Px(area_h as f32)))
            .with(StyleDeclaration::grid_template_columns(Some(equal_tracks(
                grid.cols,
            ))))
            // The row tracks come from the layout rather than being restated here,
            // and that is the whole of `fit = "content"` on the render side: the
            // layout is what the tap hit test resolves against, so a track it did not
            // compute is a cell a finger cannot land on.
            .with(StyleDeclaration::grid_template_rows(Some(pixel_tracks(
                layout.row_tracks(),
            ))))
            // A content-fitted grid whose tracks do not fill the area leaves the
            // remainder at the bottom rather than sharing it out. That is the point of
            // it: a cell is as tall as what is in it, and the slack is margin.
            .with(StyleDeclaration::align_content(JustifyContent::Start))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                gutter,
            ))))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gutter))))
            .with(StyleDeclaration::padding_top(Length::Px(gutter)))
            .with(StyleDeclaration::padding_right(Length::Px(gutter)))
            .with(StyleDeclaration::padding_bottom(Length::Px(gutter)))
            .with(StyleDeclaration::padding_left(Length::Px(gutter)))
            // Never shrunk to make room for the bar beside it. The two are already
            // sized to partition the frame exactly, so any shrinking here would be
            // the flex algorithm moving cells away from where `Layout::rect` — and
            // therefore the tap hit test — says they are.
            .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0)))),
    )
}

/// `n` equal-width tracks, i.e. `repeat(n, 1fr)`.
fn equal_tracks(n: u32) -> GridTemplateComponents {
    (0..n)
        .map(|_| GridTemplateComponent::Single(GridTrackSize::Fixed(GridLength::Fr(1.0))))
        .collect()
}

/// Tracks at exactly the pixel heights the layout computed.
///
/// Stated in pixels rather than as fractions even under `fit = "stretch"`, where
/// they are all equal: the layout has already divided the area, and having the grid
/// divide it a second time is how a rounding difference between the two puts a cell
/// half a pixel from where a tap resolves it.
fn pixel_tracks(heights: &[f32]) -> GridTemplateComponents {
    heights
        .iter()
        .map(|&h| {
            GridTemplateComponent::Single(GridTrackSize::Fixed(GridLength::Unit(Length::Px(h))))
        })
        .collect()
}

/// One widget's cell, placed explicitly on the grid.
///
/// Recursive, and terminating without a depth guard of any kind: a group's
/// children are ordinary widgets drawn by this same function, and config
/// validation rejects a group inside a group, so the recursion is exactly one
/// level deep by construction.
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
    let border = layout.border();
    // Padding on both sides, plus a rule on each. Read from the layout rather than
    // added up here, so the box a reading is fitted into is the same box validation
    // held to its floor.
    let chrome = layout.inset();
    // What a cell's contents actually have to fit inside.
    let content_w = (span_w - chrome).max(1.0);
    let style = &widget.style;
    let greys = Greys::of(style);
    // Chrome is sized to the *panel*, bounded by what a cell can hold — never to
    // the span, and never to the cell alone.
    //
    // Two things went wrong when this was a fraction of the cell's height. A label
    // that grew with its widget's span set the same word at two sizes on one
    // dashboard, which is what the cell rather than the span fixed. But a cell is
    // itself unbounded: a two-cell grid on this panel gave cells 1008 pixels tall
    // and set `BRAGA` at 110px, the same size as the reading underneath it. Chrome
    // that competes with content is not chrome. A label's job is to name a cell at
    // a glance and then get out of the way, and the size that does that is a
    // property of how far away the glass is — which is the frame, not the grid.
    //
    // Under `fit = "content"` the cell bound is dropped rather than tightened: a
    // track there is as tall as its content, its content is sized from the chrome,
    // and the chrome cannot be sized from the track without the three of them
    // chasing each other. The panel alone decides it.
    let chrome_px = natural::chrome_type(inputs.device, *style);
    let label_px = match inputs.device.grid.fit {
        Fit::Content => chrome_px,
        Fit::Stretch => chrome_px.min(layout.cell().1 * 0.11).max(style.min_type),
    };
    let gap = (span_h * 0.03).clamp(2.0, 8.0);

    let mut children = Vec::new();
    let mut content_h = span_h - chrome;
    // A group's header is optional where a leaf cell's is not. A leaf always emits
    // one because the not-confirmed mark has to have somewhere to sit; a group reads
    // nothing of its own, so it has no value to mark and no reason to spend any of
    // its box unless it was given a title. That is what lets an untitled group's
    // children fill exactly the content box `Layout::sub_layout` measures them
    // against, and so what keeps a tap landing on the child a finger was over.
    if widget.group.is_none() || widget.label.is_some() || widget.icon.is_some() {
        let header = header_node(fonts, widget, cell, inputs, content_w, label_px);
        // Measured rather than derived from `label_px`: a line box is the layout
        // engine's answer and not the font size, and the body is sized from whatever
        // the header leaves — so an estimate here shows up either as a body that
        // overflows its cell or as a strip of the cell nothing ever uses.
        content_h -= intrinsic_size(fonts, header.clone()).1 + gap;
        children.push(header);
    }
    let content_h = content_h.max(1.0);

    children.push(match &widget.group {
        Some(group) => group_node(fonts, widget, group, inputs, layout),
        // The body sits in its own growing box so that the header is pinned to the
        // top of every cell while the body is centred in whatever space is left.
        // Laying both out in one column instead would centre them as a group, which
        // makes a label's height depend on how tall its neighbour's content happens
        // to be.
        None => Node::container(body_nodes(
            fonts,
            &cell.body,
            cell.ink,
            Space {
                width: content_w,
                height: content_h,
                label_px,
                style: &widget.style,
            },
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
    });

    let mut style = Style::default()
        .with(StyleDeclaration::display(Display::Flex))
        .with(StyleDeclaration::flex_direction(FlexDirection::Column))
        .with(StyleDeclaration::justify_content(JustifyContent::Start))
        .with(StyleDeclaration::align_items(AlignItems::Start))
        .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap))))
        .with(StyleDeclaration::padding_top(Length::Px(padding)))
        .with(StyleDeclaration::padding_right(Length::Px(padding)))
        .with(StyleDeclaration::padding_bottom(Length::Px(padding)))
        .with(StyleDeclaration::padding_left(Length::Px(padding)));
    // A frameless cell is the *absence* of these declarations, not a width of
    // nothing. A zero-width solid edge still costs the layout engine a pass on each
    // of the four sides, and `border = 0` is an author asking for a dashboard of
    // bare readings — which a rule drawn at zero width is only accidentally.
    if border > 0.0 {
        style = style
            .with(StyleDeclaration::border_top_width(rule_width(border)))
            .with(StyleDeclaration::border_right_width(rule_width(border)))
            .with(StyleDeclaration::border_bottom_width(rule_width(border)))
            .with(StyleDeclaration::border_left_width(rule_width(border)))
            .with(StyleDeclaration::border_top_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_right_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_left_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_top_color(rule(greys)))
            .with(StyleDeclaration::border_right_color(rule(greys)))
            .with(StyleDeclaration::border_bottom_color(rule(greys)))
            .with(StyleDeclaration::border_left_color(rule(greys)));
    }

    Node::container(children).with_style(
        style
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

/// A group's body: a grid of its children, filling the group's content box.
///
/// The children are laid out with one gutter between them and no margin of their
/// own, because the group's own padding already *is* that margin. That is the
/// arrangement [`crate::config::sub_cell_size`] computes and the tap hit test
/// resolves against, so the grid is told to take the whole box rather than left to
/// size itself to its content: all three have to agree to the pixel or a finger on
/// one child fires another child's action.
///
/// Each child places itself with the same `grid_column_start` and `grid_row_start`
/// styles a top-level cell writes, because a child's `col` and `row` are already
/// coordinates on this grid rather than on the device's.
fn group_node(
    fonts: &Fonts,
    widget: &Widget,
    group: &crate::config::Group,
    inputs: &RenderInputs<'_>,
    layout: &Layout,
) -> Node {
    let sub = layout
        .sub_layout(widget)
        .expect("a widget carrying a group has a sub-layout");
    let gutter = layout.gutter();
    let children = group
        .widgets
        .iter()
        .map(|child| cell_node(fonts, child, &resolve(child, inputs), inputs, &sub))
        .collect::<Vec<_>>();

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Grid))
            .with(StyleDeclaration::grid_template_columns(Some(equal_tracks(
                group.grid.cols,
            ))))
            .with(StyleDeclaration::grid_template_rows(Some(equal_tracks(
                group.grid.rows,
            ))))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                gutter,
            ))))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gutter))))
            .with(StyleDeclaration::width(Length::Percentage(100.0)))
            .with(StyleDeclaration::height(Length::Percentage(100.0))),
    )
}

/// The strip across the top of a cell: its icon and label on the left, and on
/// the right the mark that says the value below is not confirmed current.
///
/// Emitted for every leaf cell, even one with no label and no icon, because the
/// mark has to have somewhere to sit. An empty header collapses to nothing taller
/// than its own zero-height children, so it costs an unlabelled cell no space.
///
/// A group is the exception, and [`cell_node`] is where that is decided: a group
/// reads nothing of its own, so it has no unconfirmed value to mark and is given a
/// header only when it was given a title to put in one. The label and icon drawn
/// here are then the group's own — each child resolves and draws its own header
/// inside its own sub-cell.
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
        left.push(icon_node(
            icon,
            label_px * 1.15,
            cell.ink,
            Greys::of(&widget.style),
        ));
    }
    if let Some(label) = &widget.label {
        left.push(fitted(fonts, label_w, label_px, |size| {
            text_node(
                label,
                one_line(
                    text_style(size, 700.0, UI_FAMILY)
                        .with(StyleDeclaration::color(muted(Greys::of(&widget.style))))
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
            Greys::of(&widget.style),
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

/// The box a body is drawn in, and the chrome it is drawn against.
///
/// One type because the three always travel together: every size in a body comes
/// out of the box, and the one size that does not — a column of readings, which is
/// capped — is measured against the chrome that names it. Passing them separately
/// was six arguments deep by the time a weather cell had split its box in two.
#[derive(Debug, Clone, Copy)]
struct Space<'a> {
    width: f32,
    height: f32,
    /// The cell's label size: chrome, and the yardstick a reading is held to.
    label_px: f32,
    /// Every size and grey this cell is drawn with, already resolved from the
    /// device's style and the widget's own.
    style: &'a crate::config::Style,
}

impl<'a> Space<'a> {
    /// The same chrome and style, over a smaller box.
    fn sized(&self, width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            label_px: self.label_px,
            style: self.style,
        }
    }

    fn greys(&self) -> Greys {
        Greys::of(self.style)
    }
}

/// The nodes that make up a cell below its header.
///
/// Every size here comes out of the content box rather than out of a fraction of
/// the cell capped at some pixel count. Those caps were set against a 400x300 test
/// device and silently bound everything on the 1448x1072 panel in service: a label
/// asked for 45px and got 32, a weather caption asked for 82px and got 34.
///
/// The one exception is a column of readings, which is capped — see
/// [`ROW_TYPE_CEILING`] for why a table is not a figure.
fn body_nodes(fonts: &Fonts, body: &Body, ink: Ink, space: Space) -> Vec<Node> {
    let Space {
        width: content_w,
        height: content_h,
        label_px,
        style,
    } = space;
    let greys = space.greys();
    match body {
        Body::Figure { text, unit } => {
            // The design size *is* the box's height: a run set at that size overflows
            // it by exactly its line height, so fitting both axes lands the figure on
            // whichever limit actually binds. Fitting width alone, as this did, left
            // the height unused — which on a nearly square cell is most of the cell.
            vec![fitted_box(fonts, content_w, content_h, content_h, |size| {
                figure_node(text, unit.as_deref(), ink, size, style, greys)
            })]
        }

        Body::Sky {
            svg,
            condition,
            rows,
        } => sky_nodes(fonts, svg, condition.as_deref(), rows, ink, space),

        Body::Beacon { on, icon, text } => {
            beacon_nodes(fonts, *on, icon.as_ref(), *text, ink, space)
        }

        Body::Prose(text) => {
            let prose_px = (content_h * 0.14).max(style.min_type);
            let lines = (content_h / (prose_px * 1.35)).floor();
            vec![text_node(
                text,
                text_style(prose_px, 400.0, UI_FAMILY)
                    .with(StyleDeclaration::color(ink.colour(greys)))
                    .with(StyleDeclaration::line_height(LineHeight::Unitless(1.3)))
                    // The width the run wraps at, stated rather than inherited. A
                    // bare text node is block-level, so left alone it takes its
                    // container's width — and inside a group that container is a
                    // grid item of a grid item, where the width resolved during
                    // measurement and the width resolved during line breaking
                    // disagree by more than a pixel and the text engine asserts.
                    // Every other length in this module comes out of the content
                    // box; this one has to as well.
                    .with(StyleDeclaration::width(Length::Px(content_w)))
                    // Bounded so a long push is clipped to the cell instead of
                    // pushing the layout around.
                    .with(StyleDeclaration::max_lines(Some(
                        (lines as u32).clamp(1, 12),
                    )))
                    .with(StyleDeclaration::text_overflow(TextOverflow::Ellipsis)),
            )]
        }

        Body::Rows(rows) => vec![rows_node(fonts, rows, space)],

        // A group's children are cells in their own right, drawn by `cell_node`.
        // There is nothing to add here, and an empty container would still be a box
        // — one taking a share of the very space those children are laid out in.
        Body::Group => Vec::new(),

        // An absence is not a reading, so it is set at chrome size rather than filling
        // the cell a value would have filled. Scaling it to the box put `no data`
        // across a 2x2 in 97px type, which shouts about the one cell with nothing to
        // say, and made the same words two sizes on one dashboard.
        //
        // Wrapped in a box of a definite width, and the wrapper is not cosmetic: the
        // run inside it is block-level, so on its own it takes its container's width
        // — and inside a group that container is a grid item of a grid item, where
        // the width resolved while measuring and the width resolved while breaking
        // lines disagree by more than a pixel and the text engine asserts. The width
        // goes on the wrapper rather than on the run because [`fitted`] measures the
        // run, and a run already told how wide to be measures as exactly that.
        Body::Absent(reason) => vec![
            Node::container(vec![fitted(
                fonts,
                content_w,
                (label_px * 1.1).max(style.min_type),
                |size| {
                    text_node(
                        reason,
                        one_line(
                            text_style(size, 400.0, UI_FAMILY)
                                .with(StyleDeclaration::color(muted(greys))),
                        ),
                    )
                },
            )])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                    .with(StyleDeclaration::width(Length::Px(content_w))),
            ),
        ],
    }
}

/// The weather body: the condition's glyph, its caption when the cell asked for
/// one, and any readings hung beside or beneath it.
///
/// With no readings the glyph block gets the whole content box, which is what a
/// weather cell has always done.
///
/// With readings the box is split along its *long* axis — the glyph beside the rows
/// in a wide cell, above them in a tall or square one. Splitting the short axis
/// instead would hand both halves a letterbox, and neither survives one: a glyph is
/// square, and labelled rows need width for their labels before they need anything
/// else. Splitting the long axis keeps both halves as close to square as the cell
/// allows.
fn sky_nodes(
    fonts: &Fonts,
    svg: &str,
    condition: Option<&str>,
    rows: &[Line],
    ink: Ink,
    space: Space,
) -> Vec<Node> {
    let Space {
        width: content_w,
        height: content_h,
        ..
    } = space;
    if rows.is_empty() {
        // Sized as one block, not as a glyph and a caption that each guessed at
        // the cell: the glyph is the reading and the words are its caption, so
        // their ratio belongs in [`sky_node`] and the pair is fitted together.
        // Sizing them apart is what put a 316px glyph and 34px words in a 699x688
        // cell.
        //
        // The centring wrapper is outside the fit because `width: 100%` inside a
        // measured node measures as the measuring viewport, not as the cell.
        return vec![
            Node::container(vec![fitted_box(
                fonts,
                content_w,
                content_h,
                content_h,
                |size| sky_node(svg, condition, ink, size, space.greys()),
            )])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::justify_content(JustifyContent::Center))
                    .with(StyleDeclaration::width(Length::Percentage(100.0))),
            ),
        ];
    }

    let beside = content_w > content_h;
    let gap = (content_w.min(content_h) * 0.06).clamp(2.0, 12.0);
    // `glyph_share` of a split height, and four fifths of that share when the two sit
    // side by side instead: there the readings carry far more glyphs than the picture
    // does and their labels are what runs out of room first, where stacked the two are
    // equals. One key rather than two, because an author moving the picture's weight
    // means it in both orientations.
    let share = space.style.glyph_share;
    let (glyph_w, glyph_h) = match beside {
        true => ((content_w - gap) * share * 0.8, content_h),
        false => (content_w, (content_h - gap) * share),
    };
    let (rows_w, rows_h) = match beside {
        true => (content_w - gap - glyph_w, content_h),
        false => (content_w, content_h - gap - glyph_h),
    };

    let glyph = Node::container(vec![fitted_box(fonts, glyph_w, glyph_h, glyph_h, |size| {
        sky_node(svg, condition, ink, size, space.greys())
    })])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::justify_content(JustifyContent::Center))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::width(Length::Px(glyph_w)))
            .with(StyleDeclaration::height(Length::Px(glyph_h)))
            // Never gives its half back: the picture is the reading, and a row of
            // long labels would otherwise squeeze it to nothing.
            .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0)))),
    );

    vec![
        Node::container(vec![
            glyph,
            rows_node(fonts, rows, space.sized(rows_w, rows_h)),
        ])
        .with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(match beside {
                    true => FlexDirection::Row,
                    false => FlexDirection::Column,
                }))
                .with(StyleDeclaration::align_items(AlignItems::Center))
                // Only the gap on the main axis applies, and which axis that is has
                // just been decided, so both are set rather than branched over again.
                .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(gap))))
                .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap))))
                .with(StyleDeclaration::width(Length::Percentage(100.0))),
        ),
    ]
}

/// The beacon body: an indicator, and `ON`/`OFF` beside it when the cell asked for
/// words.
///
/// Without the caption the indicator is sized against the whole content box rather
/// than against a word's share of the width. Merely dropping the text node would
/// leave the indicator at the size it had while it was sharing the cell, and a third
/// of the cell blank beside a dot that could have been half again as large.
fn beacon_nodes(
    fonts: &Fonts,
    on: bool,
    icon: Option<&Icon>,
    text: bool,
    ink: Ink,
    space: Space<'_>,
) -> Vec<Node> {
    let (content_w, content_h) = (space.width, space.height);
    let greys = space.greys();
    // The indicator is the reading, so it takes the height it is given, bounded by
    // whatever width the caption leaves it. With no caption it is bounded by the box
    // itself, just short of it so that it never touches the cell's rule.
    let size = match text {
        true => (content_h * 0.42).min(content_w * 0.35),
        false => (content_h * 0.9).min(content_w * 0.9),
    }
    .max(14.0);

    let indicator = match icon {
        // At the dot's size and in the dot's place, so configuring an icon is a
        // change of picture rather than a change of layout.
        Some(icon) => icon_node(icon, size, ink, greys),
        None => dot_node(size, on, ink, greys),
    };
    let mut children = vec![indicator];
    if text {
        children.push(fitted(
            fonts,
            (content_w - size * 1.5).max(1.0),
            (size * 0.78).max(13.0),
            |px| {
                text_node(
                    if on { "ON" } else { "OFF" },
                    one_line(
                        text_style(px, 700.0, UI_FAMILY)
                            .with(StyleDeclaration::color(ink.colour(greys))),
                    ),
                )
            },
        ));
    }

    vec![
        Node::container(children).with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                .with(StyleDeclaration::align_items(AlignItems::Center))
                .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                    size * 0.5,
                )))),
        ),
    ]
}

/// A beacon's indicator as a filled or hollow circle.
///
/// Drawn as a shape rather than a glyph so the indicator does not depend on the
/// embedded faces covering any particular symbol.
fn dot_node(size: f32, on: bool, ink: Ink, greys: Greys) -> Node {
    Node::container(Vec::new()).with_style(
        Style::default()
            .with(StyleDeclaration::width(Length::Px(size)))
            .with(StyleDeclaration::height(Length::Px(size)))
            .with(StyleDeclaration::border_top_left_radius(radius(size)))
            .with(StyleDeclaration::border_top_right_radius(radius(size)))
            .with(StyleDeclaration::border_bottom_right_radius(radius(size)))
            .with(StyleDeclaration::border_bottom_left_radius(radius(size)))
            .with(StyleDeclaration::background_color(match on {
                true => ink.colour(greys),
                false => paper(),
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
            .with(StyleDeclaration::border_top_color(ink.colour(greys)))
            .with(StyleDeclaration::border_right_color(ink.colour(greys)))
            .with(StyleDeclaration::border_bottom_color(ink.colour(greys)))
            .with(StyleDeclaration::border_left_color(ink.colour(greys))),
    )
}

/// How much of a cell's width the widest reading is grown to occupy.
///
/// Not all of it: a column set to the full width has its figures hard against the
/// cell's rule, and the eye needs the margin to see the column as a block. Four
/// fifths reads as filled.
const COLUMN_TARGET: f32 = 0.80;

/// How much wider than its widest row a column of readings is drawn.
///
/// A little slack so the figures do not sit hard against the glyphs naming them,
/// and no more: the column is centred, so every pixel of width beyond this is a
/// void down the middle of the cell rather than margin around it.
const COLUMN_SLACK: f32 = 1.15;

/// The most space between two readings, as a multiple of their type size.
///
/// Bounds the air rather than sharing out whatever the type ceiling left over. A
/// tall cell has more height than four readings need, and distributing all of it
/// put the readings so far apart that they stopped reading as one table.
const ROW_GAP_CEILING: f32 = 1.1;

/// A column of labelled rows filling a box, each row inked by its own trust.
///
/// Shared by a `list` body and by a weather cell's readings, because they are one
/// thing: `n` labelled values sized and aligned to a box together.
///
/// Three keys of a cell's [`crate::config::Style`] decide whether that column reads
/// as a block of figures or as a ruled table: `row_width` puts a line's name and its
/// figure at opposite edges of the cell instead of close together, `row_rule` draws a
/// hairline under each one, and `row_fill` carries those rules on to the foot of the
/// cell so a short table reads as a form rather than as a hole. All three default
/// off, which is the block.
fn rows_node(fonts: &Fonts, rows: &[Line], space: Space) -> Node {
    let Space {
        width,
        height,
        label_px,
        style,
    } = space;
    let greys = space.greys();
    let pinned = style.row_type > 0.0;
    // Three things bound a reading's size, and the smallest wins.
    //
    // The height, because the rows are laid out down the box. The width, because a
    // column that filled a third of a wide cell and centred itself left the rest as
    // a margin nobody asked for — so the type grows until the widest row occupies
    // `COLUMN_TARGET` of the width, which is what makes a cell look filled rather
    // than furnished. And the ceiling, because past it a table stops being a table.
    let by_height = height / block(rows.len());
    let by_width = width_driven_size(fonts, rows, width, greys);
    let ceiling = natural::row_ceiling(label_px, *style);
    // A pinned row is fitted to nothing, and that is the whole of what pinning
    // means: the box stops being a constraint on the type and becomes a consequence
    // of it. A panel wants the other direction — every pixel of the frame spent, so
    // a short list in a tall cell grows to meet it — but a roll has no height to
    // fill, only a length that is spent, so on paper the type is a decision about
    // the reader and the length is whatever the content came to.
    let design = match pinned {
        true => ceiling,
        false => by_height.min(by_width).min(ceiling).max(style.min_type),
    };
    // Shaved against the real measurement at the size actually chosen: the estimate
    // above is linear in the type size, which is exact for a text run and slightly
    // out once a glyph's own margins are in the row. A pinned row is shaved too — a
    // pin is a request for a size, not a licence to print past the paper's edge.
    let row_px = rows_size(fonts, rows, width, design, greys);

    // The column is as wide as its widest row needs and no wider, then centred.
    //
    // A row is glyph-or-label at one end and figure at the other, and stretching
    // that across the whole content box put a void between them: on a 950 pixel
    // cell the eye had to cross half the panel to join a sofa to its temperature.
    // Giving up the alignment instead — letting each row sit as wide as its own
    // content — would have cost the thing that makes a column of readings
    // scannable, which is that the figures line up. So the column keeps its right
    // edge and loses the emptiness.
    //
    // `row_width = "full"` is the other trade, for an author who wants the table:
    // the rules then reach both edges of the cell and the eye crosses the gap on
    // one of them instead of on nothing.
    let column = match style.row_width {
        crate::config::RowWidth::Full => width,
        crate::config::RowWidth::Content => {
            let widest = rows
                .iter()
                .map(|line| intrinsic_width(fonts, row_runs(line, row_px, greys)))
                .fold(0.0, f32::max);
            (widest * COLUMN_SLACK).clamp(1.0, width)
        }
    };

    let gap = row_gap(row_px, rows.len(), height, pinned);

    let mut children = rows
        .iter()
        // Each line's own ink, not the cell's: one unreachable sensor mutes its own
        // line and leaves the readings around it black.
        .map(|line| row_node(line, row_px, column, style, greys))
        .collect::<Vec<_>>();

    // The rest of the page, ruled and left blank. Only offered with the rules on,
    // because without them it would draw nothing at all: the blank lines *are* their
    // rules.
    if style.row_rule && style.row_fill {
        let pitch = row_px * natural::LINE_BOX + gap;
        let ruled = (height / pitch).floor() as usize;
        children.extend(
            (rows.len()..ruled).map(|_| blank_row_node(row_px * natural::LINE_BOX, column, greys)),
        );
    }

    // Ruled from the top when the rules are on, because a form starts at the top of
    // its page. Pinned rows start there too, and give up the centring across the box
    // as well, for one reason: a pinned column is not fitted to its box, so there is
    // no middle of anything it was trying to fill. Down the box what is left below
    // the last row is paper a ticket declined to spend; across it, a narrow column
    // centred in a cell reads as an indent nobody asked for.
    let (justify, align) = match (pinned, style.row_rule) {
        (true, _) => (JustifyContent::Start, AlignItems::Start),
        (false, true) => (JustifyContent::Start, AlignItems::Center),
        (false, false) => (JustifyContent::Center, AlignItems::Center),
    };

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::justify_content(justify))
            .with(StyleDeclaration::align_items(align))
            .with(StyleDeclaration::height(Length::Px(height)))
            .with(StyleDeclaration::width(Length::Px(width)))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap)))),
    )
}

/// What a column of `count` rows occupies at the tightest pitch it is ever drawn
/// at, as a multiple of one row's type size.
///
/// A line box each and the air between two of them, which is the same sum
/// [`natural`] charges a track for the same column — and it has to be. A column at
/// one pitch and drawn at another is either a reading clipped by the cell's edge or
/// a strip of the cell nothing ever uses.
///
/// The last row's gap is not in it, because a gap is between two rows and the last
/// has nothing under it. The track estimate charges for it anyway; that is the
/// difference between an estimate that must never come out short and an arrangement
/// that must fit exactly what it was given.
fn block(count: usize) -> f32 {
    count.max(1) as f32 * (natural::LINE_BOX + natural::ROW_GAP_FLOOR) - natural::ROW_GAP_FLOOR
}

/// The air between two rows of a column of `count` set at `row_px` in a box
/// `height` tall, in pixels.
///
/// Bounded rather than shared out. Giving the rows whatever the type ceiling left
/// over put a hundred and eighty pixels between two readings, which reads as four
/// unrelated cells rather than one table; a bounded gap leaves the block with air
/// around it instead of inside it, which is what air is for.
///
/// The slack is measured against the line boxes the rows occupy and not against
/// their type size, which is the bug this function exists to have fixed: a column of
/// ten measured on the size alone left a fifth of itself unaccounted for, spread
/// that height into the gaps as well, and drew its last reading past the bottom of
/// the cell.
///
/// A pinned column takes the floor and nothing else. Spreading is how a box gets
/// filled and a pinned column is not filling one — what is under its last row is
/// paper a ticket declined to spend.
fn row_gap(row_px: f32, count: usize, height: f32, pinned: bool) -> f32 {
    let floor = row_px * natural::ROW_GAP_FLOOR;
    if pinned {
        return floor;
    }
    let slack = height - row_px * natural::LINE_BOX * count as f32;
    (slack / (count.max(2) - 1) as f32).clamp(floor, row_px * ROW_GAP_CEILING)
}

/// A ruled line with nothing written on it, at a real line's height.
///
/// Nothing inside it, so unlike a line of type it cannot overflow the height it is
/// given — which is why this one may be handed a fixed height and a reading may not.
fn blank_row_node(height: f32, width: f32, greys: Greys) -> Node {
    Node::container(Vec::new()).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::width(Length::Px(width)))
            .with(StyleDeclaration::height(Length::Px(height)))
            .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0))))
            .with(StyleDeclaration::border_bottom_width(rule_width(1.0)))
            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_bottom_color(rule(greys))),
    )
}

/// The size at which the widest row would occupy [`COLUMN_TARGET`] of `width`.
///
/// Measured at a reference size and scaled, because a row's advance is proportional
/// to its type size: one measurement is exact rather than a search. The reference is
/// large enough that a glyph's fixed margins do not dominate the ratio.
///
/// This is what stops a column of four short readings from sitting in the middle of
/// a 950 pixel cell with a third of the panel blank on either side of it. Filling
/// the space is not vanity: on a wall panel every pixel not spent on a reading is a
/// pixel spent making the reading smaller than it could have been.
fn width_driven_size(fonts: &Fonts, rows: &[Line], width: f32, greys: Greys) -> f32 {
    const REFERENCE: f32 = 100.0;
    let widest = rows
        .iter()
        .map(|line| intrinsic_width(fonts, row_runs(line, REFERENCE, greys)))
        .fold(0.0, f32::max);
    if widest <= 0.0 {
        return REFERENCE;
    }
    REFERENCE * (width * COLUMN_TARGET) / widest
}

/// The one size every row is set at, shaved so the widest of them fits.
///
/// One size for every row rather than one per row, and that is the point of a
/// list: rows sized individually read as unrelated readings that happen to be
/// stacked, where a shared size reads as a table. So the widest row decides for
/// all of them.
///
/// A safety net rather than the sizing rule: [`rows_node`] has already chosen a
/// design size the widest row should fit at, and this shaves it if the measurement
/// disagrees. Without it, a reading long enough to overflow was drawn overflowing —
/// `Office 21.3 °C` at 74px in a 322px cell overprinted its neighbours.
fn rows_size(fonts: &Fonts, rows: &[Line], width: f32, design: f32, greys: Greys) -> f32 {
    let widest = rows
        .iter()
        .map(|line| intrinsic_width(fonts, row_runs(line, design, greys)))
        .fold(0.0, f32::max);
    fit_size(widest, width, design)
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
fn figure_node(
    text: &str,
    unit: Option<&str>,
    ink: Ink,
    size: f32,
    style: &crate::config::Style,
    greys: Greys,
) -> Node {
    let mut children = vec![text_node(
        text,
        one_line(
            text_style(size, 700.0, NUMERIC_FAMILY)
                .with(StyleDeclaration::color(ink.colour(greys))),
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
                    text_style(size * style.unit_scale, 400.0, UI_FAMILY)
                        .with(StyleDeclaration::color(ink.colour(greys))),
                ),
            )])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::padding_bottom(Length::Px(
                        size * (1.0 - style.unit_scale) * UNIT_BASELINE_LIFT,
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
///
/// `condition` is `None` when the cell asked for the glyph alone, and then the glyph
/// *is* the block: it is returned bare rather than wrapped in a one-child column, so
/// [`fitted_box`] fits the picture to the whole box instead of to a box with a
/// caption's worth of height and a row gap still reserved inside it.
fn sky_node(svg: &str, condition: Option<&str>, ink: Ink, size: f32, greys: Greys) -> Node {
    let glyph = icon_node(
        &Icon::Svg {
            markup: svg.to_owned(),
            ink: None,
        },
        size,
        ink,
        greys,
    );
    let Some(condition) = condition else {
        return glyph;
    };
    let caption_px = size * SKY_CAPTION_SCALE;
    Node::container(vec![
        glyph,
        text_node(
            condition,
            one_line(
                text_style(caption_px, 400.0, UI_FAMILY)
                    .with(StyleDeclaration::color(ink.colour(greys))),
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
///
/// Applies only where there is a caption. A cell with `state_text = false` has no
/// words to size, and its glyph takes the box whole.
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
fn icon_node(icon: &Icon, size: f32, ink: Ink, greys: Greys) -> Node {
    let data = match icon {
        Icon::Svg { markup, ink: own } => {
            let colour = match (ink, own) {
                (Ink::Current, Some(grey)) => grey_ink(*grey),
                _ => ink.colour(greys),
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

/// One line of a multi-reading widget: label on the left, value on the right,
/// stretched across the box it is laid out in.
///
/// With `row_rule` on it carries a hairline beneath it, drawn in the rule grey and
/// not in the reading's own: a muted sensor mutes its figure, and a table whose
/// rules faded with it would read as a broken table rather than as a stale reading.
fn row_node(
    line: &Line,
    size: f32,
    width: f32,
    style: &crate::config::Style,
    greys: Greys,
) -> Node {
    let mut declarations = Style::default()
        .with(StyleDeclaration::display(Display::Flex))
        .with(StyleDeclaration::flex_direction(FlexDirection::Row))
        .with(StyleDeclaration::justify_content(
            JustifyContent::SpaceBetween,
        ))
        .with(StyleDeclaration::align_items(AlignItems::Center))
        .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
            size * 0.5,
        ))))
        .with(StyleDeclaration::width(Length::Px(width)));
    if style.row_rule {
        declarations = declarations
            .with(StyleDeclaration::border_bottom_width(rule_width(1.0)))
            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_bottom_color(rule(greys)));
    }
    Node::container(row_runs_children(line, size, greys)).with_style(declarations)
}

/// The same row, sized to its own runs instead of to its box.
///
/// This is the shape that can be *measured*, and the reason it exists: a box that
/// is `width: 100%` measures as the measuring viewport rather than as the row, so
/// the fit that decides a column's type size has to be taken on a content-sized
/// copy. The gap between label and value is kept, because it is width the row
/// genuinely needs.
fn row_runs(line: &Line, size: f32, greys: Greys) -> Node {
    Node::container(row_runs_children(line, size, greys)).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                size * 0.5,
            )))),
    )
}

/// A row's runs: what it is, then what it reads.
///
/// What it is may be a glyph, words, or both — a glyph for a quantity, words for a
/// place, and both when a room's thermometer wants saying twice. The value carries
/// its unit rather than setting it apart, because a row is one line and `21.3 °C`
/// is one reading: the figure-and-unit treatment a whole cell gets has no room to
/// work at this size.
///
/// Both runs are one-line, and therefore elided rather than wrapped when a column
/// cannot be fitted even at [`MIN_TYPE_PX`] — a publisher sending
/// `21.299999237060547` into a cell a few characters wide. That case is what
/// [`one_line`]'s ellipsis is for: a run that wrapped inside a one-line box printed
/// over the reading beneath it.
fn row_runs_children(line: &Line, size: f32, greys: Greys) -> Vec<Node> {
    let row = &line.row;
    let ink = line.ink;
    let label = row.label.clone().or_else(|| row.id.clone());
    let mut value = row.value.as_ref().map(value_text).unwrap_or_default();
    if let Some(unit) = &row.unit {
        value.push(' ');
        value.push_str(unit);
    }

    // The glyph and the words that follow it are one thing — what this row is — so
    // they share a box and the value is what the row's `space-between` pushes away
    // from. Left as two siblings, a row with both would have spread its glyph, its
    // label and its value evenly across the line.
    let mut naming = Vec::new();
    if let Some(icon) = &line.icon {
        // A shade larger than the type it stands beside. A glyph at the same
        // nominal size as a run of digits reads smaller than they do, because the
        // digits fill their line box and a silhouette sits inside its own margins.
        naming.push(icon_node(icon, size * 1.15, ink, greys));
    }
    if let Some(label) = &label {
        naming.push(text_node(
            label,
            one_line(
                text_style(size, 400.0, UI_FAMILY).with(StyleDeclaration::color(muted(greys))),
            ),
        ));
    }

    vec![
        Node::container(naming).with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                .with(StyleDeclaration::align_items(AlignItems::Center))
                .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                    size * 0.35,
                )))),
        ),
        text_node(
            &value,
            one_line(
                text_style(size, 700.0, NUMERIC_FAMILY)
                    .with(StyleDeclaration::color(ink.colour(greys))),
            ),
        ),
    ]
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
///
/// Elided rather than merely bounded, because the two are not the same failure. A
/// run this module could not shrink to fit — every fit here floors at
/// [`MIN_TYPE_PX`], so one exists — is a publisher or config problem, and it should
/// look like one: an ellipsis says "there is more of this than fits", where a run
/// cut off mid-glyph reads as a value. That is the worst failure a panel has,
/// because nobody looking at it knows the number is wrong.
fn one_line(style: Style) -> Style {
    style
        .with(StyleDeclaration::max_lines(Some(1)))
        .with(StyleDeclaration::text_overflow(TextOverflow::Ellipsis))
}

fn family(name: &str) -> FontFamily {
    FontFamily::from_names([name.to_owned()])
}

/// A rule at a configured width.
///
/// Takes its width because a cell's rule is configuration: the same function draws
/// the hairline a dashboard gets by default and the heavier frame an author asked
/// for. A width of zero is never passed here — a frameless cell writes no border
/// declarations at all.
fn rule_width(px: f32) -> LineWidth {
    LineWidth::Length(Length::Px(px))
}

fn radius(diameter: f32) -> SpacePair<Length> {
    SpacePair::from_single(Length::Px(diameter / 2.0))
}

fn paper() -> ColorInput {
    ColorInput::Value(Color([255, 255, 255, 255]))
}

/// The three greys a frame is drawn in, lifted out of a [`crate::config::Style`].
///
/// A small copy rather than a borrow of the whole style, because these travel with
/// an [`Ink`] into functions that have no business knowing a cell's type scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Greys {
    ink: u8,
    muted: u8,
    rule: u8,
}

impl Greys {
    fn of(style: &crate::config::Style) -> Self {
        Self {
            ink: style.ink,
            muted: style.muted,
            rule: style.rule,
        }
    }
}

fn ink(greys: Greys) -> ColorInput {
    grey_ink(greys.ink)
}

/// Secondary text. Mid grey reads as secondary on a 16-level panel and still
/// resolves to something legible when quantised to fewer levels.
fn muted(greys: Greys) -> ColorInput {
    grey_ink(greys.muted)
}

/// Cell rules, light enough not to compete with the content.
fn rule(greys: Greys) -> ColorInput {
    grey_ink(greys.rule)
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
    fn colour(self, greys: Greys) -> ColorInput {
        match self {
            Self::Current => ink(greys),
            Self::Held => muted(greys),
        }
    }
}

/// One line of a multi-reading body, with how far its own value can be trusted.
///
/// The ink is per line rather than per cell, and that is the whole reason the type
/// exists: a `list` is `n` independent sensors sharing a box, so one unreachable
/// sensor mutes its own line and the lines around it stay black. A single cell-wide
/// ink would throw away `n - 1` good readings in order to report one bad one.
#[derive(Debug, Clone, PartialEq)]
struct Line {
    row: Row,
    /// Drawn where the label would go. Resolved here rather than looked up while
    /// building nodes, so that a row's layout takes no map with it.
    icon: Option<Icon>,
    ink: Ink,
}

#[derive(Debug, Clone, PartialEq)]
enum Body {
    /// A large figure with an optional unit.
    Figure { text: String, unit: Option<String> },
    /// A weather condition: its glyph, the condition in words when the cell asked
    /// for words, and any readings hung off the same entity.
    Sky {
        svg: &'static str,
        condition: Option<String>,
        rows: Vec<Line>,
    },
    /// A two-state indicator: the icon configured for the state it is in, or a dot
    /// when that state named none, captioned `ON`/`OFF` unless the cell asked for
    /// the indicator alone.
    Beacon {
        on: bool,
        icon: Option<Icon>,
        text: bool,
    },
    /// Free text, wrapped to the cell.
    Prose(String),
    /// A small group of related readings, each trusted on its own.
    Rows(Vec<Line>),
    /// A sub-grid of widgets, and a marker only: a group's children are cells in
    /// their own right, resolved and drawn by [`cell_node`], so there is nothing
    /// here for [`body_nodes`] to build.
    Group,
    /// Nothing has ever been pushed, or nothing has ever been read.
    ///
    /// Distinct from a held value, and the distinction is the point: "no data"
    /// says a publisher has never spoken, which is a wiring problem, whereas a
    /// muted value with a mark says the source is known but currently unreachable.
    Absent(&'static str),
}

/// Every kind is named rather than swept into a wildcard, so adding one is a
/// compile error here instead of a cell that silently waits for a push that will
/// never come.
fn resolve(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    match widget.kind {
        // A group holds no reading of its own; its children are resolved
        // individually, which is what `cell_node` does when it draws them.
        WidgetKind::Group => Cell {
            body: Body::Group,
            ink: Ink::Current,
        },
        // A list whose readings the author declared reads Home Assistant; one that
        // declares none is fed by push, which is how a publisher sends a shopping
        // list nobody could have written into a config file. Told apart by
        // `Widget::fed_by_push`, which the layout reads too — a cell drawn from a
        // push and charged a track as though it read Home Assistant is a list drawn
        // over its neighbour.
        WidgetKind::List if widget.fed_by_push() => resolve_pushed(widget, inputs),
        WidgetKind::HaEntity | WidgetKind::Weather | WidgetKind::List => resolve_ha(widget, inputs),
        WidgetKind::Value | WidgetKind::Beacon | WidgetKind::Text => resolve_pushed(widget, inputs),
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
    //
    // Every row shares the record's trust, unlike a `list`'s. One push either
    // arrived in time or did not: a pushed row names no source of its own, so there
    // is nothing for it to be separately unreachable from.
    if let Some(rows) = &record.rows {
        return Cell {
            body: Body::Rows(
                rows.iter()
                    .map(|row| Line {
                        row: rounded_row(row, widget.precision),
                        // A pushed row names no icon: the push protocol carries
                        // values, and a publisher that could choose glyphs would be
                        // choosing the dashboard's appearance from outside it.
                        icon: None,
                        ink,
                    })
                    .collect(),
            ),
            ink,
        };
    }

    let body = match widget.kind {
        WidgetKind::Value => Body::Figure {
            text: format_reading(&value_text(&record.value), widget.precision),
            unit: record.unit.clone().or_else(|| widget.unit.clone()),
        },
        WidgetKind::Beacon => {
            let on = beacon_is_on(record, &widget.on_values);
            Body::Beacon {
                on,
                icon: beacon_icon(widget, on, inputs),
                text: widget.state_text,
            }
        }
        WidgetKind::Text => Body::Prose(value_text(&record.value)),
        // A pushed list whose record carried a scalar rather than rows: the
        // publisher sent something, and it was not a list. Named rather than drawn
        // as a one-row table, because a cell that quietly showed the scalar would
        // leave an author reloading the page wondering where the other rows went.
        WidgetKind::List => Body::Absent("no rows"),
        WidgetKind::HaEntity | WidgetKind::Weather => {
            unreachable!("handled by resolve_ha")
        }
        WidgetKind::Group => unreachable!("handled by resolve"),
    };
    Cell { body, ink }
}

/// A pushed row with its value rounded to the widget's precision.
///
/// A publisher's row is a numeric reading like any other, so it goes through the one
/// formatter rather than around it. With no precision configured — the default —
/// [`format_reading`] hands the text back untouched, so a publisher's own digits
/// survive unless somebody actually asked for rounding.
fn rounded_row(row: &Row, precision: Option<u8>) -> Row {
    Row {
        // A row with no value at all keeps none, rather than becoming an empty
        // string that would render as a label with a blank beside it.
        value: row
            .value
            .as_ref()
            .map(|value| serde_json::Value::String(format_reading(&value_text(value), precision))),
        ..row.clone()
    }
}

/// A beacon's indicator icon for the state it is currently in.
///
/// `None` when that state names no icon, and equally when it names one the icon
/// store could not resolve. Both fall back to the dot, and that is what makes
/// configuring `icon_on` alone legal: the on state gets its picture and the off
/// state stays the hollow dot it has always been, rather than the cell losing its
/// indicator entirely.
fn beacon_icon(widget: &Widget, on: bool, inputs: &RenderInputs<'_>) -> Option<Icon> {
    let spec = match on {
        true => widget.icon_on.as_ref(),
        false => widget.icon_off.as_ref(),
    }?;
    inputs.icons.get(spec).cloned()
}

/// A cell read from Home Assistant. A fetch failure degrades this cell only: the
/// frame still renders, because one unreachable integration must not blank the
/// dashboard.
fn resolve_ha(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    // A list has no reading of its own — its body *is* its readings, each naming its
    // own entity — so it resolves ahead of the entity check below, which it would
    // otherwise fail for want of a widget-level entity it never needed.
    if widget.kind == WidgetKind::List {
        return resolve_list(widget, inputs);
    }

    let Some(entity) = &widget.entity else {
        // Config validation rejects this, so it cannot happen from a config file.
        return Cell {
            body: Body::Absent("no entity"),
            ink: Ink::Current,
        };
    };
    let reading = ha_reading(entity, widget.attribute.as_deref());

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

    // Empty for every kind but `weather`, and cheap when it is: an empty `Vec` does
    // not allocate.
    let rows = lines(&widget.readings, inputs);
    // A weather cell's condition may be current while a reading beside it is held,
    // so the cell's mark is decided over both. Per-line muting says which line is
    // stale; the mark is what a viewer scanning the whole dashboard sees first.
    let ink = held_over(ink, &rows);

    let body = match widget.kind {
        WidgetKind::Weather => {
            let (svg, label) = match icon::Condition::parse(value) {
                Some(condition) => (condition.svg(), condition.label().to_owned()),
                // An unrecognised condition still shows what Home Assistant said,
                // because a new condition slug is a thing to notice rather than hide.
                None => (icon::UNKNOWN_SKY, value.to_owned()),
            };
            Body::Sky {
                svg,
                condition: widget.state_text.then_some(label),
                rows,
            }
        }
        _ => Body::Figure {
            text: format_reading(value, widget.precision),
            unit: widget.unit.clone(),
        },
    };
    Cell { body, ink }
}

/// A cell whose body is its configured readings, one line each.
///
/// Every line is trusted on its own, which is the feature: a reading the last
/// request could not confirm is muted where it stands, one that was never read shows
/// an em dash, and the lines around either of them stay black. Muting the whole cell
/// because one sensor is unreachable would throw away the readings the panel does
/// still have.
///
/// Reached only by a list that declared readings: one that declared none is a pushed
/// cell, which [`resolve`] routes to [`resolve_pushed`] before this is called.
fn resolve_list(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    let rows = lines(&widget.readings, inputs);
    let ink = held_over(Ink::Current, &rows);
    Cell {
        body: Body::Rows(rows),
        ink,
    }
}

/// Every configured reading of a cell, resolved to lines in the order written.
fn lines(readings: &[crate::config::Reading], inputs: &RenderInputs<'_>) -> Vec<Line> {
    readings
        .iter()
        .map(|reading| reading_line(reading, inputs))
        .collect()
}

/// One configured reading as a line, carrying how far its own value can be trusted.
///
/// A reading nothing was ever read for keeps its label and its place with a null
/// value, which renders as an em dash: the cell still lists what it is meant to
/// have, and says of that one line that it does not know. Dropping the line instead
/// would silently shorten the list, and a short list reads as configuration rather
/// than as a failure.
fn reading_line(reading: &crate::config::Reading, inputs: &RenderInputs<'_>) -> Line {
    let key = ha_reading(&reading.entity, reading.attribute.as_deref());
    let (value, unit, ink) = match inputs.ha_states.get(&key) {
        Some(Reported::Fresh(text)) => (
            serde_json::Value::String(format_reading(text, reading.precision)),
            reading.unit.clone(),
            Ink::Current,
        ),
        Some(Reported::Held(text)) => (
            serde_json::Value::String(format_reading(text, reading.precision)),
            reading.unit.clone(),
            Ink::Held,
        ),
        // The unit goes with the value it qualified. `— °C` claims a reading in
        // degrees that nobody has.
        Some(Reported::Lost) | None => (serde_json::Value::Null, None, Ink::Held),
    };
    Line {
        row: Row {
            id: None,
            label: reading.label.clone(),
            value: Some(value),
            unit,
            state: None,
        },
        icon: reading
            .icon
            .as_ref()
            .and_then(|spec| inputs.icons.get(spec))
            .cloned(),
        ink,
    }
}

/// The Home Assistant reading an entity and an optional attribute name.
///
/// One function so that a widget's own reading, a `list` row's and a weather cell's
/// extra readings are all looked up under the same key. A second spelling of this
/// arithmetic is a cell reading `no data` because it asked the map a question the
/// fetcher never answered.
fn ha_reading(entity: &str, attribute: Option<&str>) -> Reading {
    match attribute {
        Some(attribute) => Reading::attribute(entity, attribute),
        None => Reading::state(entity),
    }
}

/// A cell's ink, given its own reading's and its lines'.
///
/// [`Ink::Held`] wins. The corner mark means "something in this cell is not
/// confirmed current", and that is true the moment one line is holding or missing a
/// value, however black the rest of the cell is.
fn held_over(own: Ink, lines: &[Line]) -> Ink {
    match own == Ink::Held || lines.iter().any(|line| line.ink == Ink::Held) {
        true => Ink::Held,
        false => Ink::Current,
    }
}

/// A reading's text at a configured number of decimal places.
///
/// The one place rounding happens, so that a widget's `precision`, a device's
/// default and a single reading's override cannot come to mean three slightly
/// different things in three cells.
///
/// `None` hands the text back untouched, and so does anything that does not parse as
/// a number. That fallthrough is the whole subtlety: a Home Assistant state is a
/// string that merely happens to be numeric most of the time, so `unavailable`,
/// `partlycloudy` and a publisher's `23.4 °C` all arrive here and all have to
/// survive verbatim. Refusing to render them would turn a cosmetic setting into a
/// blank cell, and coercing them to `0.0` would be worse still — a number on the
/// glass that no sensor ever reported, and one that looks exactly like a reading.
fn format_reading(text: &str, precision: Option<u8>) -> String {
    let Some(places) = precision else {
        return text.to_owned();
    };
    let Ok(number) = text.trim().parse::<f64>() else {
        return text.to_owned();
    };
    let places = places as usize;
    format!("{number:.places$}")
}

/// Whether a pushed record is older than its widget's `stale_after`.
fn is_stale(widget: &Widget, record: &ContentRecord, now: OffsetDateTime) -> bool {
    widget.stale_after > 0 && is_stale_after(widget.stale_after, record, now)
}

/// Whether a pushed record is older than `stale_after` seconds.
///
/// Computed at render time rather than stamped at push time, so raising or
/// lowering the window takes effect on the next frame. A record stamped in the
/// future is never stale: that is a clock disagreement, not freshness
/// information, and treating it as stale would mute a cell that is fine.
///
/// Shared with the status bar's alerts, which apply the same age to the opposite
/// decision — a stale cell mutes its reading, a stale alert withdraws itself — so
/// the arithmetic is written once and the policy lives at each call site.
fn is_stale_after(stale_after: u64, record: &ContentRecord, now: OffsetDateTime) -> bool {
    let age = now - record.received_at;
    age.whole_seconds() >= 0 && age.unsigned_abs().as_secs() > stale_after
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
    use crate::config::{Chrome, Grid, Group, Palette};
    use std::sync::LazyLock;
    use time::Duration;

    /// One font collection for the whole test module: registration is the
    /// expensive part and it is immutable once built.
    static FONTS: LazyLock<Fonts> = LazyLock::new(|| fonts().expect("embedded fonts must load"));

    /// The shipped look, which is what almost every test here is about: the point of
    /// the style table is that a configuration saying nothing renders what it always
    /// did, so the fixtures assert against the defaults.
    static STYLE: crate::config::Style = crate::config::Style::SHIPPED;

    const GREYS: Greys = Greys {
        ink: 0,
        muted: 102,
        rule: 170,
    };

    /// A dashboard nobody has pushed to, for the geometry tests: a rect, a gutter
    /// and a rule are what a configuration says they are, and none of them moves
    /// because a publisher spoke.
    fn nothing_pushed() -> HashMap<String, ContentRecord> {
        HashMap::new()
    }

    fn device(widgets: Vec<Widget>) -> Device {
        const GRID: Grid = Grid {
            cols: 2,
            rows: 2,
            fit: Fit::Stretch,
        };
        Device {
            id: "kindle".to_owned(),
            width: 400,
            height: 300,
            palette: Palette::Gray16,
            dither: Dither::Atkinson,
            refresh_rate: 300,
            render_interval: 300,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            grid: GRID,
            style: crate::config::Style::default(),
            chrome: Chrome::derived(400, 300, GRID),
            status_bar: None,
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
            precision: None,
            state_text: true,
            stale_after: 0,
            entity: None,
            attribute: None,
            on_values: vec!["on".to_owned(), "true".to_owned(), "alert".to_owned()],
            icon: None,
            icon_on: None,
            icon_off: None,
            readings: Vec::new(),
            group: None,
            fill: false,
            style: crate::config::Style::default(),
            tap: None,
        }
    }

    /// A panel exercising every part of the style table at once: a bar, a figure with
    /// a unit, a weather cell with its glyph and its readings, and a list.
    ///
    /// One fixture rather than one per key, because the property being asserted is
    /// about the table as a whole — that no key on it is decorative.
    fn styled_panel(
        style: crate::config::Style,
    ) -> (
        Device,
        HashMap<String, ContentRecord>,
        HashMap<Reading, Reported>,
    ) {
        const GRID: Grid = Grid {
            cols: 3,
            rows: 1,
            fit: Fit::Stretch,
        };
        let mut figure = widget("figure", WidgetKind::Value, 0, 0);
        figure.unit = Some("\u{b0}C".to_owned());
        figure.style = style;

        let mut sky = ha_widget("sky", "weather.home", WidgetKind::Weather);
        sky.col = 1;
        sky.readings = vec![reading("Temp", "weather.home", Some("\u{b0}C"))];
        sky.readings[0].attribute = Some("temperature".to_owned());
        sky.style = style;

        let mut list = widget("rooms", WidgetKind::List, 2, 0);
        list.readings = ["Office", "Hall"]
            .iter()
            .map(|label| reading(label, "sensor.room", Some("\u{b0}C")))
            .collect();
        list.style = style;

        let device = Device {
            grid: GRID,
            chrome: Chrome::derived(400, 300, GRID),
            style,
            status_bar: Some(crate::config::StatusBar {
                edge: Edge::Bottom,
                thickness: 40,
                fields: vec![
                    crate::config::StatusField::Date,
                    crate::config::StatusField::Battery,
                ],
                alerts: Vec::new(),
                timezone: crate::config::Timezone::utc(),
            }),
            ..device(vec![figure, sky, list])
        };
        let states = HashMap::from([
            (
                Reading::state("weather.home"),
                Reported::Fresh("partlycloudy".to_owned()),
            ),
            (
                Reading::attribute("weather.home", "temperature"),
                Reported::Fresh("19.5".to_owned()),
            ),
            (
                Reading::state("sensor.room"),
                Reported::Fresh("21.4".to_owned()),
            ),
        ]);
        // The figure cell needs a figure: with nothing pushed it renders `no data`,
        // and an absence has no unit for `unit_scale` to move.
        let content = HashMap::from([(
            "figure".to_owned(),
            record(serde_json::json!("21.4"), now()),
        )]);
        (device, content, states)
    }

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
                telemetry: &Telemetry::default(),
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
                telemetry: &Telemetry::default(),
            },
        )
    }

    /// The cell one widget resolves to, given pushed content and resolved icons.
    fn resolved_push(
        widget: &Widget,
        content: &HashMap<String, ContentRecord>,
        icons: &HashMap<String, Icon>,
    ) -> Cell {
        let device = device(vec![widget.clone()]);
        resolve(
            widget,
            &RenderInputs {
                device: &device,
                content,
                ha_states: &HashMap::new(),
                icons,
                now: now(),
                telemetry: &Telemetry::default(),
            },
        )
    }

    /// One configured reading, as a `list` or a `weather` cell hangs off it.
    fn reading(label: &str, entity: &str, attribute: Option<&str>) -> crate::config::Reading {
        crate::config::Reading {
            label: Some(label.to_owned()),
            icon: None,
            entity: entity.to_owned(),
            attribute: attribute.map(str::to_owned),
            unit: Some("\u{b0}C".to_owned()),
            precision: Some(1),
        }
    }

    /// One resolved line, spelt the way `resolve` builds it.
    ///
    /// Not called `line`: the module already has a `line` for grid placement, and a
    /// test helper shadowing it is a trap for whoever writes the next placement test.
    fn resolved_line(label: &str, value: serde_json::Value, unit: Option<&str>, ink: Ink) -> Line {
        Line {
            row: Row {
                id: None,
                label: Some(label.to_owned()),
                value: Some(value),
                unit: unit.map(str::to_owned),
                state: None,
            },
            icon: None,
            ink,
        }
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
                telemetry: &Telemetry::default(),
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
                    condition: Some("Partly cloudy".to_owned()),
                    rows: Vec::new(),
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
                    condition: Some("meteor-shower".to_owned()),
                    rows: Vec::new(),
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
            telemetry: &Telemetry::default(),
        };
        let raster = rasterise(
            &FONTS,
            dashboard_node(&FONTS, &inputs),
            device.width,
            device.height,
        )
        .expect("should rasterise");

        let (x, y, w, h) = Layout::for_device(device, &nothing_pushed()).rect(widget);
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
    ///
    /// `chrome` is re-derived rather than inherited from [`device`]: it is scaled to
    /// the cell, so a device that overrides the dimensions and the grid but keeps the
    /// 400x300 spacing is testing a dashboard no configuration can produce.
    fn panel(widgets: Vec<Widget>) -> Device {
        const GRID: Grid = Grid {
            cols: 4,
            rows: 3,
            fit: Fit::Stretch,
        };
        Device {
            width: 1448,
            height: 1072,
            grid: GRID,
            chrome: Chrome::derived(1448, 1072, GRID),
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
            let node = figure_node("88", Some("88"), Ink::Current, size, &STYLE, GREYS);
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
        let painted = paint_svg(fetched, muted(GREYS));
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
            GREYS,
        );
        let raster = rasterise(&FONTS, node, 48, 48).expect("should rasterise");
        let inked = raster
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|&&[.., a]| a > 128)
            .count();
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
        let intrinsic = intrinsic_width(
            &FONTS,
            figure_node(text, unit, Ink::Current, DESIGN, &STYLE, GREYS),
        );
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
                    telemetry: &Telemetry::default(),
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
    fn truncates_an_over_long_device_id() {
        assert_eq!(truncate("short", 64), "short");
        assert_eq!(truncate("abcdef", 3), "abc\u{2026}");
    }

    /// The palette's top level: paper, on a 16-level greyscale panel.
    const PAPER: u8 = 15;

    /// The frame's palette levels, one per pixel, read back out of the encoded PNG.
    ///
    /// Read off the frame the panel is actually handed rather than tapped out of the
    /// rasteriser, and exact rather than approximate: a 16-level greyscale panel
    /// rendered with [`Dither::None`] maps paper onto the top level and a cell's rule
    /// onto the level nearest its grey, so a probe distinguishes the two with no
    /// tolerance to tune.
    fn greys(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().expect("should be a PNG");
        assert_eq!(
            reader.info().bit_depth,
            png::BitDepth::Four,
            "these probes read a 16-level greyscale frame"
        );
        let mut packed = vec![0; reader.output_buffer_size().expect("a bounded frame")];
        let info = reader.next_frame(&mut packed).expect("frame should decode");

        // Two pixels per byte, leftmost in the high nibble, every row padded to a byte
        // boundary — so the row stride is read from the decoder rather than computed
        // from the width.
        let mut levels = Vec::with_capacity((info.width * info.height) as usize);
        for y in 0..info.height as usize {
            let row = &packed[y * info.line_size..][..info.line_size];
            for x in 0..info.width as usize {
                levels.push(if x % 2 == 0 {
                    row[x / 2] >> 4
                } else {
                    row[x / 2] & 0x0F
                });
            }
        }
        (info.width, info.height, levels)
    }

    /// Whether anything inside a rect is darker than paper.
    fn inked(levels: &[u8], width: u32, rect: (f32, f32, f32, f32)) -> bool {
        let (x, y, w, h) = rect;
        let (x0, y0) = (x.max(0.0) as u32, y.max(0.0) as u32);
        let (x1, y1) = ((x + w).ceil() as u32, (y + h).ceil() as u32);
        (y0..y1).any(|y| {
            (x0..x1).any(|x| {
                let index = (y * width + x) as usize;
                index < levels.len() && levels[index] < PAPER
            })
        })
    }

    /// How many separate runs of ink cross the one-pixel band at `y`, between `x0`
    /// and `x1`.
    ///
    /// Aimed at a row of cells' top rules, where it counts the cells themselves
    /// rather than standing in for them: a cell's content sits a padding inside its
    /// own rule, so that row holds the rules and nothing else. The band covers every
    /// row the rule can touch, which is two of them when it falls at a fractional
    /// offset and is antialiased across both.
    fn rule_runs(levels: &[u8], width: u32, y: f32, x0: f32, x1: f32) -> usize {
        let band = y.floor() as u32..(y + 1.0).ceil() as u32;
        let mut runs = 0;
        let mut inside = false;
        for x in x0.floor() as u32..x1.ceil() as u32 {
            let ink = band
                .clone()
                .any(|y| levels[(y * width + x) as usize] < PAPER);
            runs += usize::from(ink && !inside);
            inside = ink;
        }
        runs
    }

    /// A 2x1 group filling the panel's top-left slot, its two children each holding
    /// a pushed reading.
    ///
    /// `title` is the *group's* own label. The children always carry theirs, because
    /// a child of a group is an ordinary cell and draws its own header.
    fn grouped_panel(title: Option<&str>) -> (Device, HashMap<String, ContentRecord>) {
        let mut group = widget("indoors", WidgetKind::Group, 0, 0);
        group.label = title.map(str::to_owned);
        group.group = Some(Group {
            grid: Grid {
                cols: 2,
                rows: 1,
                fit: Fit::Stretch,
            },
            widgets: vec![
                widget("left", WidgetKind::Value, 0, 0),
                widget("right", WidgetKind::Value, 1, 0),
            ],
        });
        let device = Device {
            // Both dithering modes texture flat tone, which is exactly what these
            // probes measure. Without dithering paper stays paper and a rule stays a
            // rule, everywhere.
            dither: Dither::None,
            ..panel(vec![group])
        };
        let content = HashMap::from([
            ("left".to_owned(), record(serde_json::json!("21.4"), now())),
            ("right".to_owned(), record(serde_json::json!("48"), now())),
        ]);
        (device, content)
    }

    #[test]
    fn a_group_draws_one_cell_per_child_where_sub_layout_puts_it() {
        // Asserted against `sub_layout` because the renderer and the tap hit test read
        // it as one answer: if a child's cell were drawn anywhere other than the rect
        // `sub_layout` reports, a finger over one reading would fire another's action.
        let (device, content) = grouped_panel(None);
        let (width, _, levels) = greys(&render(&device, &content));

        let group = &device.widgets[0];
        let sub = Layout::for_device(&device, &nothing_pushed())
            .sub_layout(group)
            .expect("a group has a sub-layout");
        let children = &group.group.as_ref().expect("the group is set").widgets;

        // Each child's own content box, not merely its rect: a rect holds the child's
        // rule whether or not anything was drawn inside it, so probing the rect would
        // pass on a row of empty boxes.
        let inset = sub.padding() + sub.border();
        for child in children {
            let (x, y, w, h) = sub.rect(child);
            let content = (x + inset, y + inset, w - inset * 2.0, h - inset * 2.0);
            assert!(
                inked(&levels, width, content),
                "child `{}` drew nothing inside {content:?}",
                child.id
            );
        }

        // Two cells side by side rather than one box holding two readings: the gutter
        // between them is untouched, and the row of their top rules crosses ink
        // exactly twice.
        let (x, y, w, h) = sub.rect(&children[0]);
        let gutter = sub.gutter();
        assert!(
            !inked(&levels, width, (x + w + 2.0, y, gutter - 4.0, h)),
            "the gutter between two children must stay paper"
        );
        assert_eq!(
            rule_runs(&levels, width, y, x, x + w + gutter + w),
            2,
            "a 2x1 group must draw two child cells, edge to edge across its content box"
        );
    }

    #[test]
    fn a_titled_group_draws_its_own_header_across_its_children() {
        // A group is a cell, so it can be titled like one. The gutter between its
        // children is what proves the header is the group's own and not a child's: the
        // title spans the whole content box and crosses that gutter, which nothing
        // inside a child can reach.
        let (untitled, content) = grouped_panel(None);
        let (titled, _) = grouped_panel(Some("INDOORS AND OUT"));

        let group = &untitled.widgets[0];
        let sub = Layout::for_device(&untitled, &nothing_pushed())
            .sub_layout(group)
            .expect("a group has a sub-layout");
        let children = &group.group.as_ref().expect("the group is set").widgets;
        let (x, y, w, h) = sub.rect(&children[0]);
        // The gutter between the two children, clear of both their rules. Nothing
        // inside a child can reach it, so ink here is the group's own.
        let between = (x + w + 2.0, y, sub.gutter() - 4.0, h);

        let (width, _, plain) = greys(&render(&untitled, &content));
        assert!(
            !inked(&plain, width, between),
            "an untitled group draws no header, so its gutter is paper"
        );

        let (_, _, headed) = greys(&render(&titled, &content));
        assert!(
            inked(&headed, width, between),
            "a titled group must draw its own header, spanning its children"
        );
    }

    #[test]
    fn a_zero_border_draws_no_rules() {
        // A frameless dashboard has to be the *absence* of the border declarations. A
        // zero-width solid edge still costs the layout engine a pass on each of the
        // four sides, and `border = 0` is an author asking for bare readings rather
        // than for a rule nobody can see.
        let framed = Device {
            dither: Dither::None,
            ..device(vec![widget("a", WidgetKind::Value, 0, 0)])
        };
        let frameless = Device {
            chrome: Chrome {
                border: 0.0,
                ..framed.chrome
            },
            ..framed.clone()
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("21.4"), now()))]);

        // The cell's top edge, which a rule traces corner to corner and nothing else
        // reaches: a cell's content sits a padding inside it.
        let (x, y, w, h) = Layout::for_device(&framed, &nothing_pushed()).rect(&framed.widgets[0]);
        let edge = (x + 2.0, y, w - 4.0, 1.0);

        let (width, _, ruled) = greys(&render(&framed, &content));
        assert!(
            inked(&ruled, width, edge),
            "a cell's rule must trace its top edge"
        );

        let (width, _, bare) = greys(&render(&frameless, &content));
        assert!(
            !inked(&bare, width, edge),
            "a zero border must draw nothing at all"
        );
        // And the cell still has its label and its reading in it, so the assertion
        // above cannot be passing on a blank frame.
        assert!(
            inked(&bare, width, (x, y, w, h)),
            "a frameless cell must still draw its contents"
        );
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
    fn precision_rounds_a_number_and_leaves_anything_else_alone() {
        // One formatter for every reading on the dashboard, so the same `precision`
        // cannot come to mean two things in two cells.
        assert_eq!(format_reading("21.456", None), "21.456");
        assert_eq!(format_reading("21.456", Some(1)), "21.5");
        assert_eq!(format_reading("21.456", Some(0)), "21");
        // Padded when the source carries fewer digits than were asked for, so a
        // value walking between 21 and 21.05 does not change width under the eye.
        assert_eq!(format_reading("21", Some(2)), "21.00");
        assert_eq!(format_reading("-3.75", Some(1)), "-3.8");
        assert_eq!(format_reading(" 4.2 ", Some(1)), "4.2");

        // The fallthrough that matters: a Home Assistant state is a string that
        // merely happens to be numeric most of the time, and every one of these
        // reaches the formatter on a cell whose author set a precision.
        assert_eq!(format_reading("partlycloudy", Some(1)), "partlycloudy");
        assert_eq!(format_reading("unavailable", Some(2)), "unavailable");
        assert_eq!(
            format_reading("23.4 \u{b0}C", Some(1)),
            "23.4 \u{b0}C",
            "a value carrying its own unit is not a number"
        );
        assert_eq!(format_reading("", Some(2)), "");
    }

    #[test]
    fn a_configured_precision_reaches_both_reading_paths() {
        // Pushed and read are two functions, and a formatter applied in only one of
        // them is a dashboard where `precision` works on some cells and not others.
        let pushed = Widget {
            precision: Some(1),
            ..widget("a", WidgetKind::Value, 0, 0)
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("21.456"), now()))]);
        assert_eq!(
            resolved_push(&pushed, &content, &HashMap::new()).body,
            Body::Figure {
                text: "21.5".to_owned(),
                unit: None,
            }
        );

        let read = Widget {
            precision: Some(1),
            ..ha_widget("temp", "sensor.office", WidgetKind::HaEntity)
        };
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("21.456".to_owned()),
        )]);
        assert_eq!(
            resolved(&read, &ha).body,
            Body::Figure {
                text: "21.5".to_owned(),
                unit: None,
            }
        );
    }

    #[test]
    fn a_beacon_without_state_text_keeps_the_indicator_and_drops_the_word() {
        let captioned = widget("a", WidgetKind::Beacon, 0, 0);
        let bare = Widget {
            state_text: false,
            ..captioned.clone()
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("on"), now()))]);

        assert_eq!(
            resolved_push(&captioned, &content, &HashMap::new()).body,
            Body::Beacon {
                on: true,
                icon: None,
                text: true,
            }
        );
        assert_eq!(
            resolved_push(&bare, &content, &HashMap::new()).body,
            Body::Beacon {
                on: true,
                icon: None,
                text: false,
            }
        );
    }

    #[test]
    fn an_uncaptioned_beacon_takes_the_cell_the_caption_would_have_had() {
        // Measured in pixels because the resolved body cannot say it: dropping the
        // word alone would leave the indicator at the size it had while it was
        // sharing the width, and a third of the cell blank beside it.
        let captioned = widget("a", WidgetKind::Beacon, 0, 0);
        let bare = Widget {
            state_text: false,
            ..captioned.clone()
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("on"), now()))]);
        let with_word = panel(vec![captioned]);
        let without = panel(vec![bare]);

        let (_, shorter) = cell_fill(&with_word, "a", &content, &HashMap::new());
        let (_, taller) = cell_fill(&without, "a", &content, &HashMap::new());
        assert!(
            taller > shorter * 1.1,
            "the uncaptioned indicator must grow into the space the word had: \
             {taller:.2} of the height against {shorter:.2}"
        );
    }

    #[test]
    fn a_weather_cell_can_stand_without_its_condition_in_words() {
        let captioned = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        let bare = Widget {
            state_text: false,
            ..captioned.clone()
        };
        let ha = HashMap::from([(
            Reading::state("weather.braga"),
            Reported::Fresh("partlycloudy".to_owned()),
        )]);

        assert_eq!(
            resolved(&captioned, &ha).body,
            Body::Sky {
                svg: icon::Condition::PartlyCloudy.svg(),
                condition: Some("Partly cloudy".to_owned()),
                rows: Vec::new(),
            }
        );
        assert_eq!(
            resolved(&bare, &ha).body,
            Body::Sky {
                svg: icon::Condition::PartlyCloudy.svg(),
                condition: None,
                rows: Vec::new(),
            },
            "the glyph is the reading, so it stands on its own"
        );
    }

    #[test]
    fn a_beacon_draws_its_state_icon_and_falls_back_to_the_dot_without_one() {
        // Configuring `icon_on` alone is legal, which is the point of the fallback:
        // the on state gets its picture and the off state stays the hollow dot,
        // rather than the cell losing its indicator half the time.
        let w = Widget {
            icon_on: Some("mdi-lightbulb-on".to_owned()),
            ..widget("a", WidgetKind::Beacon, 0, 0)
        };
        let bulb = Icon::Svg {
            markup: r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="9"/></svg>"#
                .to_owned(),
            ink: None,
        };
        let icons = HashMap::from([("mdi-lightbulb-on".to_owned(), bulb.clone())]);
        let on = HashMap::from([("a".to_owned(), record(serde_json::json!("on"), now()))]);
        let off = HashMap::from([("a".to_owned(), record(serde_json::json!("off"), now()))]);

        assert_eq!(
            resolved_push(&w, &on, &icons).body,
            Body::Beacon {
                on: true,
                icon: Some(bulb),
                text: true,
            }
        );
        assert_eq!(
            resolved_push(&w, &off, &icons).body,
            Body::Beacon {
                on: false,
                icon: None,
                text: true,
            },
            "the off state configured no icon, so it stays a dot"
        );
        // A spec the icon store could not resolve falls back the same way: an
        // unreachable icon host must not cost the cell its indicator.
        assert_eq!(
            resolved_push(&w, &on, &HashMap::new()).body,
            Body::Beacon {
                on: true,
                icon: None,
                text: true,
            }
        );
    }

    #[test]
    fn a_weather_cell_carries_its_own_readings_beside_the_condition() {
        // The attributes a `weather.*` entity's state does not carry: the condition is
        // a word, and the temperature and humidity hang off the same entity.
        let w = Widget {
            readings: vec![
                reading("Temp", "weather.braga", Some("temperature")),
                crate::config::Reading {
                    unit: Some("%".to_owned()),
                    precision: Some(0),
                    ..reading("Humidity", "weather.braga", Some("humidity"))
                },
            ],
            ..ha_widget("sky", "weather.braga", WidgetKind::Weather)
        };
        let ha = HashMap::from([
            (
                Reading::state("weather.braga"),
                Reported::Fresh("sunny".to_owned()),
            ),
            (
                Reading::attribute("weather.braga", "temperature"),
                Reported::Fresh("23.456".to_owned()),
            ),
            (
                Reading::attribute("weather.braga", "humidity"),
                Reported::Fresh("61.8".to_owned()),
            ),
        ]);

        assert_eq!(
            resolved(&w, &ha),
            Cell {
                body: Body::Sky {
                    svg: icon::Condition::Sunny.svg(),
                    condition: Some("Sunny".to_owned()),
                    rows: vec![
                        resolved_line(
                            "Temp",
                            serde_json::json!("23.5"),
                            Some("\u{b0}C"),
                            Ink::Current
                        ),
                        resolved_line("Humidity", serde_json::json!("62"), Some("%"), Ink::Current),
                    ],
                },
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_list_mutes_only_the_reading_it_could_not_get() {
        // The whole point of a per-line ink: one unreachable sensor must not take the
        // two working readings beside it off the glass.
        let w = Widget {
            readings: vec![
                reading("Office", "sensor.office", None),
                reading("Hall", "sensor.hall", None),
                reading("Shed", "sensor.shed", None),
            ],
            ..widget("rooms", WidgetKind::List, 0, 0)
        };
        let ha = HashMap::from([
            (
                Reading::state("sensor.office"),
                Reported::Fresh("21.44".to_owned()),
            ),
            (
                Reading::state("sensor.hall"),
                Reported::Fresh("19.0".to_owned()),
            ),
            (Reading::state("sensor.shed"), Reported::Lost),
        ]);

        assert_eq!(
            resolved(&w, &ha),
            Cell {
                body: Body::Rows(vec![
                    resolved_line(
                        "Office",
                        serde_json::json!("21.4"),
                        Some("\u{b0}C"),
                        Ink::Current
                    ),
                    resolved_line(
                        "Hall",
                        serde_json::json!("19.0"),
                        Some("\u{b0}C"),
                        Ink::Current
                    ),
                    // The lost reading keeps its label and its place. Its value is
                    // null, which renders as an em dash, and its unit goes with the
                    // value it qualified — `— °C` would claim a reading in degrees
                    // that nobody has.
                    resolved_line("Shed", serde_json::Value::Null, None, Ink::Held),
                ]),
                // Marked, because one line in this cell is not confirmed current.
                ink: Ink::Held,
            }
        );
    }

    #[test]
    fn a_list_renders_a_frame_with_a_reading_it_could_not_get() {
        // The resolved cell says the right thing; this says the em-dash path actually
        // rasterises, which is the half a `Cell` assertion cannot reach.
        let w = Widget {
            readings: vec![
                reading("Office", "sensor.office", None),
                reading("Shed", "sensor.shed", None),
            ],
            ..widget("rooms", WidgetKind::List, 0, 0)
        };
        let device = device(vec![w]);
        let partial = HashMap::from([
            (
                Reading::state("sensor.office"),
                Reported::Fresh("21.4".to_owned()),
            ),
            (Reading::state("sensor.shed"), Reported::Lost),
        ]);
        let whole = HashMap::from([
            (
                Reading::state("sensor.office"),
                Reported::Fresh("21.4".to_owned()),
            ),
            (
                Reading::state("sensor.shed"),
                Reported::Fresh("8.2".to_owned()),
            ),
        ]);

        let degraded = render_with(&device, &HashMap::new(), &partial, &HashMap::new());
        assert_eq!(dimensions(&degraded), (400, 300));
        assert_ne!(
            degraded,
            render_with(&device, &HashMap::new(), &whole, &HashMap::new()),
            "a reading that could not be got must be visibly distinct from one that could"
        );
    }

    /// A list that declares no reading is fed by push, which is the only way a
    /// shopping list gets onto a ticket: nobody writes one into a config file.
    #[test]
    fn a_list_with_no_reading_draws_the_rows_it_was_pushed() {
        let list = widget("items", WidgetKind::List, 0, 0);
        let sent = HashMap::from([("items".to_owned(), rows_record(2))]);

        assert_eq!(
            resolved_push(&list, &sent, &HashMap::new()),
            Cell {
                body: Body::Rows(vec![
                    resolved_line("item 0", serde_json::json!("0"), None, Ink::Current),
                    resolved_line("item 1", serde_json::json!("1"), None, Ink::Current),
                ]),
                ink: Ink::Current,
            },
            "a pushed list's rows are its body"
        );

        // Nothing pushed yet, and a push that was not a list, are both named rather
        // than drawn as an empty table: a cell that quietly showed the scalar would
        // leave an author wondering where the other rows went.
        assert_eq!(
            resolved_push(&list, &HashMap::new(), &HashMap::new()).body,
            Body::Absent("no data")
        );
        let scalar =
            HashMap::from([("items".to_owned(), record(serde_json::json!("21.4"), now()))]);
        assert_eq!(
            resolved_push(&list, &scalar, &HashMap::new()).body,
            Body::Absent("no rows")
        );
    }

    #[test]
    fn a_group_resolves_to_a_marker_with_no_body_of_its_own() {
        // The body is deliberately empty: `cell_node` draws a group's children, and a
        // body node here would take a share of the box they are laid out in.
        let w = widget("box", WidgetKind::Group, 0, 0);
        assert_eq!(
            resolved(&w, &HashMap::new()),
            Cell {
                body: Body::Group,
                ink: Ink::Current,
            }
        );
        assert!(
            body_nodes(
                &FONTS,
                &Body::Group,
                Ink::Current,
                Space {
                    width: 200.0,
                    height: 200.0,
                    label_px: 14.0,
                    style: &STYLE,
                },
            )
            .is_empty(),
            "a group's cell has no body of its own to draw"
        );
    }

    /// A band-counting probe: how many separate horizontal strips of ink lie inside
    /// a rect.
    ///
    /// One band per line of text, so it counts *lines* — which is what tells a
    /// column of three readings apart from three readings where one of them wrapped
    /// onto a second line and printed over its neighbour.
    fn ink_bands(levels: &[u8], width: u32, rect: (f32, f32, f32, f32)) -> usize {
        let (x, y, w, h) = rect;
        let (x0, x1) = (x.max(0.0) as u32, (x + w).ceil() as u32);
        let mut bands = 0;
        let mut inside = false;
        for y in y.max(0.0) as u32..(y + h).ceil() as u32 {
            let ink = (x0..x1).any(|x| {
                let index = (y * width + x) as usize;
                index < levels.len() && levels[index] < PAPER
            });
            bands += usize::from(ink && !inside);
            inside = ink;
        }
        bands
    }

    /// A panel whose only cell is a `list` of three readings, on a grid dense enough
    /// that the readings have to be fitted to the width they are given.
    ///
    /// Sized as the second panel in service is — 800x600 on a 4x3 grid, so a cell is
    /// about 184 pixels of content wide. That is the width a reading with no
    /// `precision` set overflows even at [`MIN_TYPE_PX`], which is the case worth
    /// having a fixture for.
    fn listed_panel(values: [&str; 3]) -> (Device, HashMap<Reading, Reported>) {
        const GRID: Grid = Grid {
            cols: 4,
            rows: 3,
            fit: Fit::Stretch,
        };
        let mut list = widget("climate", WidgetKind::List, 0, 0);
        list.label = Some("Climate".to_owned());
        // No `precision`, deliberately: rounding is what keeps a reading short, and
        // the case worth a fixture is the author who has not configured any.
        list.readings = ["Office", "Bedroom", "Hall"]
            .iter()
            .zip(["sensor.office", "sensor.bedroom", "sensor.hall"])
            .map(|(label, entity)| crate::config::Reading {
                precision: None,
                ..reading(label, entity, None)
            })
            .collect();
        let device = Device {
            dither: Dither::None,
            width: 800,
            height: 600,
            grid: GRID,
            chrome: Chrome::derived(800, 600, GRID),
            ..panel(vec![list])
        };
        let states = HashMap::from([
            (
                Reading::state("sensor.office"),
                Reported::Fresh(values[0].to_owned()),
            ),
            (
                Reading::state("sensor.bedroom"),
                Reported::Fresh(values[1].to_owned()),
            ),
            (
                Reading::state("sensor.hall"),
                Reported::Fresh(values[2].to_owned()),
            ),
        ]);
        (device, states)
    }

    #[test]
    fn a_column_of_readings_is_fitted_to_its_width_and_not_only_its_height() {
        // Sized by height alone — which is all this did — three readings in a 322x302
        // cell were set at 74px, and `Office 21.3 °C` at 74px is half again as wide as
        // the cell it was drawn in. It overprinted the readings under it.
        let rows = [
            resolved_line(
                "Office",
                serde_json::json!("21.3"),
                Some("\u{b0}C"),
                Ink::Current,
            ),
            resolved_line(
                "Bedroom",
                serde_json::json!("19.1"),
                Some("\u{b0}C"),
                Ink::Current,
            ),
            resolved_line("Hall", serde_json::json!("48"), Some("%"), Ink::Current),
        ];

        let tall = 302.0;
        let by_height = tall / block(3);
        let wide = rows_size(&FONTS, &rows, 900.0, by_height, GREYS);
        let narrow = rows_size(&FONTS, &rows, 322.0, by_height, GREYS);

        assert!(
            (wide - by_height).abs() < 1.0,
            "given width to spare the height decides: {wide} should be about {by_height}"
        );
        assert!(
            narrow < by_height * 0.8,
            "in a 322px box the widest row has to shrink the column: {narrow} vs {by_height}"
        );
        assert!(
            narrow >= MIN_TYPE_PX,
            "the fit still floors at the readable size: {narrow}"
        );
    }

    #[test]
    fn every_reading_in_a_list_keeps_its_own_line() {
        // Three readings, three lines of ink — including the case the fit cannot
        // satisfy. A value too long for the box even at [`MIN_TYPE_PX`] used to wrap,
        // and a wrapped line inside a one-line box prints over the reading beneath it,
        // so the count is what catches it.
        for values in [
            ["21.3", "19.1", "48"],
            // What a publisher sends when nobody configured `precision`.
            ["21.299999237060547", "19.050000190734863", "47.8231"],
        ] {
            let (device, states) = listed_panel(values);
            let png = render_with(&device, &HashMap::new(), &states, &HashMap::new());
            let (width, _, levels) = greys(&png);

            let layout = Layout::for_device(&device, &nothing_pushed());
            let (x, y, w, h) = layout.rect(&device.widgets[0]);
            let inset = layout.inset() / 2.0;
            // Below the header, so the label above the readings is not counted as one.
            let body = (
                x + inset,
                y + inset + h * 0.25,
                w - inset * 2.0,
                h * 0.75 - inset * 2.0,
            );

            assert_eq!(
                ink_bands(&levels, width, body),
                3,
                "three readings must draw three lines, whatever their values ({values:?})"
            );
        }
    }

    #[test]
    fn a_nested_cell_with_nothing_to_say_still_renders() {
        // The regression this pins is a panic, not a misdraw. A run that inherits its
        // width from its container measures one width and breaks lines at another once
        // that container is a grid item of a grid item, and the text engine asserts on
        // the disagreement — so a group holding a `text` cell, or any cell with no data
        // yet, killed the whole frame. Both bodies are bare runs; every other body
        // wraps its text in a box and was never affected.
        let mut group = widget("utility", WidgetKind::Group, 0, 0);
        group.group = Some(Group {
            grid: Grid {
                cols: 2,
                rows: 2,
                fit: Fit::Stretch,
            },
            widgets: vec![
                widget("prose", WidgetKind::Text, 0, 0),
                widget("absent", WidgetKind::Value, 1, 0),
                widget("figure", WidgetKind::Value, 0, 1),
            ],
        });
        let device = panel(vec![group]);
        let content = HashMap::from([
            (
                "prose".to_owned(),
                record(
                    serde_json::json!("Bin day Tuesday, and the boiler is booked"),
                    now(),
                ),
            ),
            (
                "figure".to_owned(),
                record(serde_json::json!("21.4"), now()),
            ),
        ]);

        let png = render(&device, &content);
        assert_eq!(dimensions(&png), (1448, 1072));

        // And the absence really was drawn, rather than skipped along with the panic.
        let (width, _, levels) = greys(&render(
            &Device {
                dither: Dither::None,
                ..device.clone()
            },
            &content,
        ));
        let sub = Layout::for_device(&device, &nothing_pushed())
            .sub_layout(&device.widgets[0])
            .expect("a group has a sub-layout");
        let children = &device.widgets[0].group.as_ref().expect("set").widgets;
        for child in children {
            assert!(
                inked(&levels, width, sub.rect(child)),
                "nested cell `{}` drew nothing at all",
                child.id
            );
        }
    }

    /// A panel whose bar carries one alert, rendered with `pushed` in the store.
    fn alerting_frame(pushed: Option<&str>) -> Vec<u8> {
        let text = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "none"
refresh_rate = 300
grid = { cols = 2, rows = 1 }

[device.status_bar]
edge = "bottom"
fields = ["date"]

[[device.status_bar.alert]]
id = "post"
label = "MAIL"

[[device.widget]]
id = "a"
kind = "value"
col = 0
row = 0

[[device.widget]]
id = "b"
kind = "value"
col = 1
row = 0
"#;
        let device = &crate::config::parse(text)
            .expect("the fixture must be valid")
            .devices[0];
        let mut content =
            HashMap::from([("a".to_owned(), record(serde_json::json!("21.4"), now()))]);
        if let Some(state) = pushed {
            content.insert(
                "post".to_owned(),
                ContentRecord {
                    value: serde_json::Value::String(state.to_owned()),
                    state: Some(state.to_owned()),
                    unit: None,
                    rows: None,
                    received_at: now(),
                },
            );
        }
        render(device, &content)
    }

    #[test]
    fn a_column_grows_to_fill_a_cell_wider_than_its_readings() {
        // The complaint this pins: four short readings in a 950px cell were set at
        // what the height and the ceiling allowed, came out 480px wide, and were
        // centred — leaving a third of the panel blank on either side. Every pixel
        // not spent on a reading is a pixel spent making the reading smaller than it
        // could have been.
        let rows: Vec<Line> = ["21.9", "22.3", "26.1", "53"]
            .iter()
            .map(|v| resolved_line("", serde_json::json!(*v), Some("\u{b0}C"), Ink::Current))
            .collect();

        for width in [400.0_f32, 950.0] {
            let size = width_driven_size(&FONTS, &rows, width, GREYS);
            let widest = rows
                .iter()
                .map(|line| intrinsic_width(&FONTS, row_runs(line, size, GREYS)))
                .fold(0.0_f32, f32::max);
            let filled = widest / width;
            assert!(
                (filled - COLUMN_TARGET).abs() < 0.06,
                "at {width}px the widest row should occupy about {COLUMN_TARGET} of the \
                 cell, got {filled:.2}"
            );
        }
    }

    #[test]
    fn a_column_of_readings_never_outgrows_the_box_it_was_given() {
        // Two bugs in one arrangement, and both of them cost a reading.
        //
        // Sharing out whatever the ceiling left over put 180px between two readings,
        // which reads as four unrelated cells rather than one table. And measuring
        // the slack against the type size rather than the line box left a fifth of
        // the column unaccounted for, spread that into the gaps as well, and drew the
        // last reading past the bottom of the cell — on a ten-item shopping list, off
        // the end of the ticket.
        let row_px = 60.0_f32;
        for (count, height) in [(4, 2000.0_f32), (10, 425.0), (3, 302.0), (1, 500.0)] {
            let gap = row_gap(row_px, count, height, false);
            assert!(
                gap <= row_px * ROW_GAP_CEILING + 0.01,
                "the gap must stay bounded: {count} rows in {height}px got {gap}"
            );
            assert!(
                gap >= row_px * natural::ROW_GAP_FLOOR - 0.01,
                "and never go under the floor: {count} rows in {height}px got {gap}"
            );
        }

        // The column the sizing rule actually produces, in the box that produced it,
        // fits inside it: `block` is what the type is fitted by, so a column set at
        // that size and spaced by `row_gap` cannot come out taller than the box.
        for (count, height) in [(10, 425.0_f32), (4, 200.0), (2, 90.0), (1, 40.0)] {
            let size = height / block(count);
            let column = size * natural::LINE_BOX * count as f32
                + row_gap(size, count, height, false) * (count - 1) as f32;
            assert!(
                column <= height + 0.01,
                "{count} rows fitted to a {height}px box came out {column}px tall"
            );
        }

        // A pinned column is spaced by the floor whatever the box, because the box is
        // not a thing it is trying to fill.
        assert!(
            (row_gap(20.0, 10, 4000.0, true) - 20.0 * natural::ROW_GAP_FLOOR).abs() < 0.01,
            "a pinned column takes the floor and nothing more"
        );
    }

    /// A pushed record of `count` labelled rows: what a publisher sends for a list
    /// nobody could have written into a config file.
    fn rows_record(count: usize) -> ContentRecord {
        ContentRecord {
            rows: Some(
                (0..count)
                    .map(|index| Row {
                        id: None,
                        label: Some(format!("item {index}")),
                        value: Some(serde_json::json!(index)),
                        unit: None,
                        state: None,
                    })
                    .collect(),
            ),
            ..record(serde_json::Value::Null, now())
        }
    }

    /// A ticket: a heading and a pushed list of `count` rows on a printhead-wide
    /// frame `height` long, with the rows pinned to 20px and set edge to edge.
    ///
    /// The palette is the one these probes read rather than the `mono` a printer
    /// takes, because the two differ in tone and not in geometry.
    fn ticket(count: usize, height: u32) -> (Device, HashMap<String, ContentRecord>) {
        const GRID: Grid = Grid {
            cols: 1,
            rows: 2,
            fit: Fit::Content,
        };
        let mut head = widget("head", WidgetKind::Value, 0, 0);
        head.label = Some("Ticket".to_owned());
        let mut items = widget("items", WidgetKind::List, 0, 1);
        // A list that declares no reading is fed by push, and a leaf cell's header is
        // the mark's home rather than a title, so this one has no label to draw.
        items.label = None;
        items.style = crate::config::Style {
            row_type: 20.0,
            row_width: crate::config::RowWidth::Full,
            ..crate::config::Style::SHIPPED
        };
        let device = Device {
            width: 384,
            height,
            grid: GRID,
            chrome: Chrome::derived(384, height, GRID),
            widgets: vec![head, items],
            ..device(Vec::new())
        };
        let content = HashMap::from([
            (
                "head".to_owned(),
                record(serde_json::json!("Groceries"), now()),
            ),
            ("items".to_owned(), rows_record(count)),
        ]);
        (device, content)
    }

    /// How much paper a frame costs: the row after its last inked one, which is
    /// exactly what the sink's trailing-blank trim leaves on the roll.
    fn paper(png: &[u8]) -> u32 {
        let (width, height, levels) = greys(png);
        (0..height)
            .rev()
            .find(|&y| inked(&levels, width, (0.0, y as f32, width as f32, 1.0)))
            .map_or(0, |y| y + 1)
    }

    #[test]
    fn a_pinned_pushed_list_costs_the_paper_its_rows_cost_and_no_more() {
        // The complaint, measured on the device: one cell stretched to the frame, the
        // type sized to fill it, and a one-item list costing the same roll as a
        // ten-item one. Three things have to be true for a ticket to be as long as
        // its content, and each is asserted here: the rows are all drawn, the frame
        // ends after the last of them, and the roll's length does not decide either.
        let (device, content) = ticket(10, 768);
        let png = render(&device, &content);
        let (width, _, levels) = greys(&png);

        let layout = Layout::for_device(&device, &content);
        let (x, y, w, h) = layout.rect(&device.widgets[1]);
        let inset = layout.inset() / 2.0;
        assert_eq!(
            ink_bands(
                &levels,
                width,
                (x + inset, y + inset, w - inset * 2.0, h - inset * 2.0)
            ),
            10,
            "ten pushed rows must draw ten lines: fewer means the column was clipped \
             by its track, more means two of them overprinted"
        );

        // Seven more rows cost seven rows of paper. Bounded rather than pinned to a
        // number, because the engine's own line box is what falls between the two:
        // each row costs at least that box and at most a full pitch, and paper is
        // measured in whole rows, so a pixel of rounding is allowed at each end.
        let (device, content) = ticket(3, 768);
        let short = paper(&render(&device, &content));
        let long = paper(&png);
        let per_row = 20.0 * natural::LINE_BOX - 1.0
            ..=20.0 * (natural::LINE_BOX + natural::ROW_GAP_FLOOR) + 1.0;
        let grown = (long - short) as f32 / 7.0;
        assert!(
            per_row.contains(&grown),
            "seven more items must cost seven rows of paper: {short}px against \
             {long}px is {grown:.1}px each, outside {:.1}..={:.1}",
            per_row.start(),
            per_row.end()
        );

        // And the roll is not what decides it. This is the whole complaint: the frame
        // height set both the type size and the paper cost, so the same list on a
        // longer roll printed a longer ticket.
        let (longer_roll, content) = ticket(10, 1200);
        assert_eq!(
            paper(&render(&longer_roll, &content)),
            long,
            "the same list on a longer roll must cost the same paper"
        );
        assert!(
            long < 768,
            "and the ticket must end before the frame does, or there is nothing for \
             the sink to trim: got {long}px of 768"
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
    fn chrome_is_sized_to_the_panel_and_not_to_the_cell() {
        // A two-cell grid on the panel in service gives cells 1008 pixels tall. Sized
        // off the cell, a label came out at 110px — the same size as the reading it
        // was introducing, which is not chrome. Both grids here are the same panel,
        // so both must set their labels the same.
        let dense = panel(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let sparse = Device {
            grid: Grid {
                cols: 2,
                rows: 1,
                fit: Fit::Stretch,
            },
            chrome: Chrome::derived(
                1448,
                1072,
                Grid {
                    cols: 2,
                    rows: 1,
                    fit: Fit::Stretch,
                },
            ),
            ..panel(vec![widget("a", WidgetKind::Value, 0, 0)])
        };

        let label_px = |device: &Device| {
            let layout = Layout::for_device(device, &nothing_pushed());
            (device.width.min(device.height) as f32 * STYLE.chrome_scale * STYLE.type_scale)
                .min(layout.cell().1 * 0.11)
                .max(STYLE.min_type)
        };

        assert_eq!(
            label_px(&dense),
            label_px(&sparse),
            "one panel, one chrome size, whatever the grid"
        );
        assert!(
            label_px(&sparse) < 40.0,
            "chrome must stay chrome: got {}",
            label_px(&sparse)
        );
    }

    #[test]
    fn a_column_of_readings_is_capped_below_what_a_tall_cell_would_allow() {
        // The ceiling, in the shape the complaint arrived in: four readings in a cell
        // half the panel high were set at what the height afforded and shouted.
        let rows: Vec<Line> = ["21.9", "22.3", "26.1", "53"]
            .iter()
            .map(|v| resolved_line("Room", serde_json::json!(*v), Some("\u{b0}C"), Ink::Current))
            .collect();

        let label_px = 32.0;
        let tall = rows_size(
            &FONTS,
            &rows,
            691.0,
            {
                let by_height = 991.0_f32 / block(4);
                by_height
                    .min(label_px * STYLE.reading_ceiling)
                    .max(STYLE.min_type)
            },
            GREYS,
        );
        assert!(
            tall <= label_px * STYLE.reading_ceiling + 0.5,
            "a tall cell must not set its rows past the ceiling: got {tall}"
        );
        assert!(
            tall > label_px,
            "the reading still has to outweigh the label naming it: got {tall}"
        );
    }
}
