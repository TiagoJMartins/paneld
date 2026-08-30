//! The drawing primitives every body reaches for: an icon painted in its cell's
//! ink, a row's label-and-value runs, and the text-node plumbing beneath both.

use takumi::prelude::*;

use crate::icon::Icon;

use super::resolve::value_text;
use super::types::{Greys, Ink, Line, grey_ink, muted, rule};
use super::{NUMERIC_FAMILY, UI_FAMILY};

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
pub(super) fn icon_node(icon: &Icon, size: f32, ink: Ink, greys: Greys) -> Node {
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
pub(super) fn row_node(
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
pub(super) fn row_runs(line: &Line, size: f32, greys: Greys) -> Node {
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
pub(super) fn text_node(text: &str, style: Style) -> Node {
    Node::text(text.to_owned()).with_style(style)
}

/// The base style for a run of text, to be extended with `.with(..)`.
pub(super) fn text_style(size: f32, weight: f32, family_name: &str) -> Style {
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
pub(super) fn one_line(style: Style) -> Style {
    style
        .with(StyleDeclaration::max_lines(Some(1)))
        .with(StyleDeclaration::text_overflow(TextOverflow::Ellipsis))
}

pub(super) fn family(name: &str) -> FontFamily {
    FontFamily::from_names([name.to_owned()])
}

/// A rule at a configured width.
///
/// Takes its width because a cell's rule is configuration: the same function draws
/// the hairline a dashboard gets by default and the heavier frame an author asked
/// for. A width of zero is never passed here — a frameless cell writes no border
/// declarations at all.
pub(super) fn rule_width(px: f32) -> LineWidth {
    LineWidth::Length(Length::Px(px))
}

pub(super) fn radius(diameter: f32) -> SpacePair<Length> {
    SpacePair::from_single(Length::Px(diameter / 2.0))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::{Widget, WidgetKind};
    use crate::ha::{Reading, Reported};

    use super::super::rasterise;

    use super::super::test_support::*;
    use super::*;

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
    fn icon_node_draws_a_raster_icon_and_an_svgs_own_colour() {
        // A decoded raster icon takes the same path to the glass as a vector one:
        // built into an image node and actually drawn, not silently dropped for
        // lacking markup.
        let raster = Icon::Raster {
            data: [200, 0, 0, 255].repeat(4),
            width: 2,
            height: 2,
        };
        let raster_bytes = rasterise(
            &FONTS,
            icon_node(&raster, 48.0, Ink::Current, GREYS),
            48,
            48,
        )
        .expect("should rasterise");
        let raster_inked = raster_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|&&[.., a]| a > 128)
            .count();
        assert!(
            raster_inked > 20,
            "a raster icon must actually draw ink, got {raster_inked}"
        );

        // An SVG carrying its own configured grey is still drawn, whatever the
        // cell's own ink says.
        let coloured = Icon::Svg {
            markup: r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="9"/></svg>"#
                .to_owned(),
            ink: Some(96),
        };
        let coloured_bytes = rasterise(
            &FONTS,
            icon_node(&coloured, 48.0, Ink::Current, GREYS),
            48,
            48,
        )
        .expect("should rasterise");
        let coloured_inked = coloured_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|&&[.., a]| a > 128)
            .count();
        assert!(
            coloured_inked > 20,
            "an icon's own colour must still actually draw, got {coloured_inked}"
        );
    }

    #[test]
    fn paint_svg_leaves_markup_it_cannot_paint_unchanged() {
        // No colour to paint with: whatever `currentColor` resolves to at the
        // rasteriser is what the caller already asked for.
        let markup = r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0h8v8z"/></svg>"#;
        assert_eq!(paint_svg(markup, ColorInput::CurrentColor), markup);

        // No `<svg` tag at all: nothing to attach a colour to.
        let no_tag = "<path d=\"M0 0h8v8z\"/>";
        assert_eq!(paint_svg(no_tag, muted(GREYS)), no_tag);

        // `<svg` as a prefix of a longer tag name is not the element it looks like.
        let odd_tag = "<svgfoo/>";
        assert_eq!(paint_svg(odd_tag, muted(GREYS)), odd_tag);
    }

    #[test]
    fn a_list_rows_own_icon_draws_beside_its_label() {
        // Each `reading` may carry its own icon, resolved the same way a widget's
        // does; nothing before this pinned that a list row actually draws it.
        let mut office = reading("Office", "sensor.office", None);
        office.icon = Some("mdi-home".to_owned());
        let w = Widget {
            readings: vec![office],
            ..widget("rooms", WidgetKind::List, 0, 0)
        };
        let device = device(vec![w]);
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("21.4".to_owned()),
        )]);

        let without_icon = render_with(&device, &HashMap::new(), &ha, &HashMap::new());
        let icons = HashMap::from([(
            "mdi-home".to_owned(),
            Icon::Svg {
                markup: r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="9"/></svg>"#
                    .to_owned(),
                ink: None,
            },
        )]);
        let with_icon = render_with(&device, &HashMap::new(), &ha, &icons);

        assert_ne!(
            without_icon, with_icon,
            "a reading's own icon must actually change what a list row draws"
        );
    }
}
