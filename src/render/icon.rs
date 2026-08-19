//! Weather-condition icons, and the two standalone marks a cell can carry.
//!
//! Every glyph is line art compiled into the binary as a `&'static str`. That is
//! a deliberate choice on three counts:
//!
//! - **Nothing is fetched or read from disk.** An icon that depends on the
//!   network or the filesystem is an icon that can be missing on the one frame
//!   that mattered, and a missing icon on e-ink is indistinguishable from a
//!   broken widget.
//! - **Nothing here is ordered by a hash map or a clock.** The device caches
//!   frames by a content hash of the encoded bytes, so identical inputs must
//!   produce identical output; a `&'static str` picked by an exhaustive `match`
//!   cannot vary between renders.
//! - **The markup is stroked line art, not filled shapes.** Frames are quantised
//!   to the panel's palette and dithered, and Atkinson dithering turns a large
//!   flat fill into visible structure while erasing a hairline entirely. A
//!   1.6-unit stroke on a 24-unit viewBox lands at roughly three device pixels
//!   once a cell draws the glyph at 40-90px, which survives both.
//!
//! Colour is never written into the markup. Every path paints in `currentColor`
//! and no glyph sets `color` on its root, which is what lets the renderer inject
//! one root `color` attribute to draw the same icon black in a live cell and grey
//! in a held one. Setting `color` here would pin it and silently defeat that.

/// The shared root element, so every glyph is optically consistent.
///
/// Wrapping the geometry rather than repeating the root per icon is what
/// guarantees the invariants the panel cannot tolerate being drifted: one 24x24
/// viewBox for all of them (so they interchange at any size), `currentColor`
/// throughout, and a stroke weight that survives dithering. `resvg` silently
/// ignores what it does not implement, so a glyph that opted out of these by
/// accident would render as an empty cell with no error anywhere.
macro_rules! icon {
    ($($body:expr),+ $(,)?) => {
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" "#,
            r#"stroke="currentColor" stroke-width="1.6" stroke-linecap="round" "#,
            r#"stroke-linejoin="round">"#,
            $($body,)+
            "</svg>"
        )
    };
}

/// The cloud every precipitation glyph hangs its weather from, flat-bottomed at
/// `y=14` so there is room for what falls out of it without clipping.
macro_rules! cloud {
    () => {
        r#"<path d="M7 14A3.2 3.2 0 0 1 7.2 7.7A4.6 4.6 0 0 1 16.1 8.2A3 3 0 0 1 17 14Z"/>"#
    };
}

/// The same cloud dropped to sit centred, for the glyphs that are a cloud alone.
macro_rules! cloud_centred {
    () => {
        r#"<path d="M7 17A3.2 3.2 0 0 1 7.2 10.7A4.6 4.6 0 0 1 16.1 11.2A3 3 0 0 1 17 17Z"/>"#
    };
}

/// A weather condition, as Home Assistant reports a `weather.*` entity's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    ClearNight,
    Cloudy,
    Exceptional,
    Fog,
    Hail,
    Lightning,
    LightningRainy,
    PartlyCloudy,
    Pouring,
    Rainy,
    Snowy,
    SnowyRainy,
    Sunny,
    Windy,
    WindyVariant,
}

impl Condition {
    /// Every condition this build draws, in one place.
    ///
    /// [`parse`](Self::parse) walks this rather than matching on the incoming
    /// string, which is what lets the slug spelling live next to the variant it
    /// names instead of in a second table that can fall out of step with it.
    const ALL: [Self; 15] = [
        Self::ClearNight,
        Self::Cloudy,
        Self::Exceptional,
        Self::Fog,
        Self::Hail,
        Self::Lightning,
        Self::LightningRainy,
        Self::PartlyCloudy,
        Self::Pouring,
        Self::Rainy,
        Self::Snowy,
        Self::SnowyRainy,
        Self::Sunny,
        Self::Windy,
        Self::WindyVariant,
    ];

    /// Parses the slug Home Assistant reports. `None` for anything this build
    /// does not know, which the caller draws as an unknown sky rather than
    /// guessing.
    pub fn parse(state: &str) -> Option<Self> {
        let state = state.trim();
        Self::ALL.into_iter().find(|c| slug_eq(c.slug(), state))
    }

    /// Sentence-case display name, e.g. `Partly cloudy`.
    pub fn label(self) -> &'static str {
        match self {
            Self::ClearNight => "Clear night",
            Self::Cloudy => "Cloudy",
            Self::Exceptional => "Exceptional",
            Self::Fog => "Fog",
            Self::Hail => "Hail",
            Self::Lightning => "Lightning",
            Self::LightningRainy => "Thunderstorm",
            Self::PartlyCloudy => "Partly cloudy",
            Self::Pouring => "Heavy rain",
            Self::Rainy => "Rain",
            Self::Snowy => "Snow",
            Self::SnowyRainy => "Sleet",
            Self::Sunny => "Sunny",
            Self::Windy => "Windy",
            Self::WindyVariant => "Windy and cloudy",
        }
    }

    /// Line-art SVG for this condition.
    ///
    /// The glyphs are separated by stroke count and silhouette, never by size:
    /// two icons that differ only in scale are the same icon once the frame has
    /// been dithered to sixteen grey levels and viewed from across a room.
    pub fn svg(self) -> &'static str {
        match self {
            // A crescent: the outer edge of one circle cut by a second offset up
            // and to the right. Drawn as two arcs meeting at the horns rather
            // than as a circle behind a mask, because a mask usvg declined would
            // leave a full moon and report nothing.
            Self::ClearNight => {
                icon!(r#"<path d="M20.16 14.72A8.6 8.6 0 1 1 7.72 4.54A9 9 0 0 0 20.16 14.72Z"/>"#)
            }
            Self::Cloudy => icon!(cloud_centred!()),
            // A warning triangle with the exclamation's stem and dot as strokes.
            // `<text>` is not an option: takumi parses SVG with usvg's text
            // support off, so a glyph built from a character silently loses it.
            Self::Exceptional => icon!(
                r#"<path d="M12 3.2L21.5 20H2.5Z"/>"#,
                r#"<path d="M12 9v5.2"/>"#,
                r#"<path d="M12 17.2h.01"/>"#,
            ),
            Self::Fog => icon!(
                cloud!(),
                r#"<path d="M5.5 17h13"/>"#,
                r#"<path d="M8 19.5h13"/>"#,
                r#"<path d="M4 22h12"/>"#,
            ),
            Self::Hail => icon!(
                cloud!(),
                r#"<circle cx="9" cy="18.5" r="1.3"/>"#,
                r#"<circle cx="13" cy="20.5" r="1.3"/>"#,
                r#"<circle cx="17" cy="18.5" r="1.3"/>"#,
            ),
            // Bolt alone, filling the viewBox. Its rainy sibling shrinks the bolt
            // and puts a cloud above it, so the two never read as the same mark.
            Self::Lightning => {
                icon!(r#"<path d="M13.5 2.5L4 14h8.5l-1 7.5 9-11.5h-8.5l1.5-7.5z"/>"#)
            }
            Self::LightningRainy => icon!(
                cloud!(),
                r#"<path d="M13.6 15.5L10.2 19.5h2.4l-1.2 2.5 4-4h-2.4z"/>"#,
                r#"<path d="M18.5 16.5L16.9 20.5"/>"#,
            ),
            // The sun is an open arc, not a circle: the cloud is drawn over it and
            // an unfilled outline hides nothing, so the part the cloud covers is
            // simply not drawn.
            Self::PartlyCloudy => icon!(
                r#"<path d="M11.41 9.06A3.1 3.1 0 1 0 7.44 10.91"/>"#,
                r#"<path d="M8.5 3.8V2"/>"#,
                r#"<path d="M4.3 8H2.5"/>"#,
                r#"<path d="M5.53 5.03L4.26 3.76"/>"#,
                r#"<path d="M11.47 5.03L12.74 3.76"/>"#,
                r#"<path d="M11 18A2.7 2.7 0 0 1 11.2 12.7A3.9 3.9 0 0 1 18.7 13.1A2.5 2.5 0 0 1 19.5 18Z"/>"#,
            ),
            // Long strokes reaching the baseline, against `rainy`'s short ones.
            Self::Pouring => icon!(
                cloud!(),
                r#"<path d="M10 16L7.6 22.5"/>"#,
                r#"<path d="M13.5 16L11.1 22.5"/>"#,
                r#"<path d="M17 16L14.6 22.5"/>"#,
            ),
            Self::Rainy => icon!(
                cloud!(),
                r#"<path d="M9.5 16.5L8.3 19.5"/>"#,
                r#"<path d="M13 16.5L11.8 19.5"/>"#,
                r#"<path d="M16.5 16.5L15.3 19.5"/>"#,
            ),
            // Arms 60 degrees apart, not 30: three near-vertical strokes 1.6 wide
            // merge into one blob by the time a cell draws the glyph small, and a
            // blob is hail, not snow.
            Self::Snowy => icon!(
                cloud!(),
                r#"<path d="M9.4 16.4v5.2"/>"#,
                r#"<path d="M7.2 17.7L11.6 20.3"/>"#,
                r#"<path d="M7.2 20.3L11.6 17.7"/>"#,
                r#"<path d="M16.4 16.4v5.2"/>"#,
                r#"<path d="M14.2 17.7L18.6 20.3"/>"#,
                r#"<path d="M14.2 20.3L18.6 17.7"/>"#,
            ),
            // One flake and one drop, so sleet is legible as "both" rather than as
            // a smaller version of either.
            Self::SnowyRainy => icon!(
                cloud!(),
                r#"<path d="M10.2 16.4v5.2"/>"#,
                r#"<path d="M8 17.7L12.4 20.3"/>"#,
                r#"<path d="M8 20.3L12.4 17.7"/>"#,
                r#"<path d="M17 16.5L15.4 20.5"/>"#,
            ),
            Self::Sunny => icon!(
                r#"<circle cx="12" cy="12" r="4"/>"#,
                r#"<path d="M12 5.6V3.2"/>"#,
                r#"<path d="M12 18.4V20.8"/>"#,
                r#"<path d="M5.6 12H3.2"/>"#,
                r#"<path d="M18.4 12H20.8"/>"#,
                r#"<path d="M7.47 7.47L5.78 5.78"/>"#,
                r#"<path d="M16.53 7.47L18.22 5.78"/>"#,
                r#"<path d="M7.47 16.53L5.78 18.22"/>"#,
                r#"<path d="M16.53 16.53L18.22 18.22"/>"#,
            ),
            // Three curls spread over the full height, against the variant's two
            // pushed below a cloud.
            Self::Windy => icon!(
                r#"<path d="M3.5 7.5h9.5a2.5 2.5 0 1 0-2.5-2.5"/>"#,
                r#"<path d="M3.5 12h13a2.7 2.7 0 1 1-2.7 2.7"/>"#,
                r#"<path d="M6.5 16.5h7a2.5 2.5 0 1 1-2.5 2.5"/>"#,
            ),
            Self::WindyVariant => icon!(
                cloud!(),
                r#"<path d="M4 17.5h10a2.4 2.4 0 1 0-2.4-2.4"/>"#,
                r#"<path d="M7 20.5h7a2 2 0 1 0-2-2"/>"#,
            ),
        }
    }

    /// The slug Home Assistant reports for this condition.
    ///
    /// Exhaustive on purpose: adding a variant without giving it a slug is a
    /// compile error, which is the only thing that keeps this in step with Home
    /// Assistant's vocabulary.
    fn slug(self) -> &'static str {
        match self {
            Self::ClearNight => "clear-night",
            Self::Cloudy => "cloudy",
            Self::Exceptional => "exceptional",
            Self::Fog => "fog",
            Self::Hail => "hail",
            Self::Lightning => "lightning",
            Self::LightningRainy => "lightning-rainy",
            Self::PartlyCloudy => "partlycloudy",
            Self::Pouring => "pouring",
            Self::Rainy => "rainy",
            Self::Snowy => "snowy",
            Self::SnowyRainy => "snowy-rainy",
            Self::Sunny => "sunny",
            Self::Windy => "windy",
            Self::WindyVariant => "windy-variant",
        }
    }
}

/// Whether `candidate` names the canonical `slug`.
///
/// Case is ignored and `_` is accepted for `-` because the state does not always
/// arrive from Home Assistant's own serialiser: a template that builds the string
/// by hand commonly emits `clear_night`, and rejecting that would draw an unknown
/// sky for a condition we can name perfectly well. Compares in place rather than
/// normalising into a `String`, so a per-widget parse allocates nothing.
///
/// Both sides are compared byte-wise, which is safe because every slug is ASCII
/// lowercase letters and hyphens: a multi-byte character in `candidate` can only
/// ever mismatch, never be split into something that matches.
fn slug_eq(slug: &str, candidate: &str) -> bool {
    slug.len() == candidate.len()
        && slug.bytes().zip(candidate.bytes()).all(|(want, got)| {
            let got = if got == b'_' { b'-' } else { got };
            want == got.to_ascii_lowercase()
        })
}

/// Drawn for a condition string Home Assistant reports that this build does not
/// recognise.
///
/// A cloud with a question mark, rather than a blank cell: an unrecognised
/// condition is a fact worth showing, because the alternative is a gap in the
/// dashboard that reads as a render bug.
pub const UNKNOWN_SKY: &str = icon!(
    cloud!(),
    r#"<path d="M10 17.4A2.1 2.1 0 0 1 14.1 18.1c0 1.4-2.1 2.1-2.1 2.1"/>"#,
    r#"<path d="M12 22.2h.01"/>"#,
);

/// The mark a cell carries when the value on it is the last one that was read
/// rather than one the latest request confirmed.
///
/// A struck-through cloud, deliberately not the warning triangle
/// [`Condition::Exceptional`] uses: this mark sits in a cell's corner beside a
/// value that is still worth reading, so it must not be mistaken for the weather
/// itself. Two strokes, because at the 14-20px it is drawn at anything busier
/// becomes a smudge that only pulls the eye off the reading.
pub const NOT_CONFIRMED: &str = icon!(cloud_centred!(), r#"<path d="M4.5 19.5L19.5 4.5"/>"#,);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use takumi::prelude::*;

    /// Every glyph this module hands out, named so a failure names the offender.
    fn all_markup() -> Vec<(&'static str, &'static str)> {
        let mut markup: Vec<(&'static str, &'static str)> = Condition::ALL
            .into_iter()
            .map(|c| (c.slug(), c.svg()))
            .collect();
        markup.push(("unknown-sky", UNKNOWN_SKY));
        markup.push(("not-confirmed", NOT_CONFIRMED));
        markup
    }

    #[test]
    fn every_condition_parses_from_its_slug() {
        for condition in Condition::ALL {
            assert_eq!(
                Condition::parse(condition.slug()),
                Some(condition),
                "{} should parse",
                condition.slug()
            );
        }
    }

    #[test]
    fn parse_tolerates_the_spellings_templates_emit() {
        for (state, want) in [
            ("clear-night", Some(Condition::ClearNight)),
            ("clear_night", Some(Condition::ClearNight)),
            ("CLEAR_NIGHT", Some(Condition::ClearNight)),
            ("Clear-Night", Some(Condition::ClearNight)),
            ("  partlycloudy  ", Some(Condition::PartlyCloudy)),
            ("\tsnowy_rainy\n", Some(Condition::SnowyRainy)),
            ("Windy-Variant", Some(Condition::WindyVariant)),
            ("lightning_rainy", Some(Condition::LightningRainy)),
            ("SUNNY", Some(Condition::Sunny)),
            ("windy_variant", Some(Condition::WindyVariant)),
            // Not Home Assistant conditions, however plausible they read.
            ("drizzle", None),
            ("partly-cloudy", None),
            ("unavailable", None),
            ("unknown", None),
            ("", None),
            ("-", None),
            ("sunny!", None),
            ("sun", None),
            ("sunnyy", None),
        ] {
            assert_eq!(Condition::parse(state), want, "parsing {state:?}");
        }
    }

    #[test]
    fn slugs_are_distinct_and_cover_every_variant() {
        let slugs: HashSet<&str> = Condition::ALL.iter().map(|c| c.slug()).collect();
        assert_eq!(slugs.len(), 15, "a slug is duplicated or a variant missing");
    }

    #[test]
    fn every_glyph_is_distinct() {
        let markup: HashSet<&str> = all_markup().into_iter().map(|(_, svg)| svg).collect();
        assert_eq!(
            markup.len(),
            17,
            "two glyphs share markup, so two conditions draw the same mark"
        );
    }

    #[test]
    fn every_glyph_obeys_the_panel_markup_rules() {
        for (name, svg) in all_markup() {
            assert!(!svg.is_empty(), "{name} is empty");
            assert!(
                svg.starts_with(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24""#),
                "{name} does not open with the shared root"
            );
            assert!(svg.ends_with("</svg>"), "{name} is not closed");
            assert!(
                svg.contains(r#"viewBox="0 0 24 24""#),
                "{name} is not on the shared viewBox"
            );
            assert!(
                svg.contains("currentColor"),
                "{name} does not paint in currentColor"
            );
            // A root `color`, or any literal colour, would pin the ink and defeat
            // the grey the renderer injects for a held cell.
            assert!(!svg.contains("color=\""), "{name} sets a colour of its own");
            for forbidden in [
                "<text",
                "<style",
                "class=",
                "linearGradient",
                "radialGradient",
                "filter=",
                "mask=",
                "#",
                "rgb(",
            ] {
                assert!(
                    !svg.contains(forbidden),
                    "{name} contains {forbidden}, which resvg silently drops or which \
                     overrides the colour cascade"
                );
            }
        }
    }

    #[test]
    fn every_label_is_a_distinct_sentence_case_name() {
        let labels: HashSet<&str> = Condition::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), 15, "two conditions share a label");
        for condition in Condition::ALL {
            let label = condition.label();
            let first = label.chars().next().expect("a label is never empty");
            assert!(
                first.is_uppercase(),
                "{label:?} does not start with an upper-case letter"
            );
        }
    }

    /// Rasterises one glyph through takumi and counts the pixels it inked.
    ///
    /// This is the only assertion in the module that can catch the failure that
    /// matters: `usvg` drops what it cannot parse without reporting anything, so
    /// a malformed path or an unsupported element renders a blank cell that every
    /// string assertion above would still pass.
    fn ink(markup: &str) -> usize {
        const SIZE: u32 = 48;

        let node = Node::container(vec![Node::image((markup.to_owned(), SIZE, SIZE))]).with_style(
            Style::default()
                .with(StyleDeclaration::width(Length::Px(SIZE as f32)))
                .with(StyleDeclaration::height(Length::Px(SIZE as f32)))
                .with(StyleDeclaration::background_color(ColorInput::Value(
                    Color([255, 255, 255, 255]),
                )))
                .with(StyleDeclaration::color(ColorInput::Value(Color([
                    0, 0, 0, 255,
                ])))),
        );

        // No text is drawn, so an empty collection is the honest input: registering
        // the panel's faces here would only hide a glyph that had smuggled in a
        // `<text>` element.
        let fonts = Fonts::default();
        let options = RenderOptions::builder()
            .viewport(Viewport::new((SIZE, SIZE)))
            .node(node)
            .fonts(&fonts)
            .build();

        let bitmap = takumi::render(options).expect("a glyph should rasterise");
        assert_eq!((bitmap.width(), bitmap.height()), (SIZE, SIZE));

        bitmap
            .into_raw()
            .chunks_exact(4)
            .filter(|px| px[0] < 250 || px[1] < 250 || px[2] < 250)
            .count()
    }

    #[test]
    fn every_glyph_rasterises_with_ink_on_it() {
        let mut report = Vec::new();
        for (name, svg) in all_markup() {
            let count = ink(svg);
            report.push(format!("{name}={count}"));
            assert!(
                count >= 20,
                "{name} rasterised {count} non-white pixels at 48x48; usvg accepted \
                 the markup but drew next to nothing"
            );
        }
        // Printed so a faint glyph is visible under `--nocapture` rather than only
        // once it has fallen under the threshold.
        println!("ink at 48x48: {}", report.join(" "));
    }
}
