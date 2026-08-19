//! Widget icons, spelt the way [gethomepage](https://gethomepage.dev) spells one.
//!
//! Adopting an existing grammar rather than inventing one is the whole point:
//! anyone who has written a `homepage` `services.yaml` already knows that
//! `mdi-thermometer` means Material Design Icons and that `plex.png` means the
//! Dashboard Icons collection, and a config line can be copied between the two
//! projects unchanged.
//!
//! The one structural difference is where resolution happens. gethomepage
//! resolves an icon in the browser, so the CDN URL is all it needs. A panel has
//! no browser — the Kindle fetches one PNG and nothing else — so paneld must
//! resolve, fetch and rasterise server-side. That forces two properties:
//!
//! - **Fetching is not part of rendering.** [`Store::fetch`] runs alongside the
//!   Home Assistant read, before the pure render. A frame is therefore
//!   reproducible and a dashboard stays drawable while the internet is down.
//! - **The cache is on disk and keyed by the URL.** Every entry is
//!   content-addressed by what was asked for, so losing the directory costs one
//!   round trip per icon and nothing else.
//!
//! A failure never propagates: an icon that cannot be resolved leaves that one
//! cell without an icon, exactly as a Home Assistant outage leaves one cell
//! without a reading.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

/// How long a single icon fetch may take.
///
/// Shorter than the Home Assistant timeout on purpose: an icon is decoration,
/// and the render loop is a single task, so a slow CDN must not be able to hold
/// up every device's frame for as long as a missing sensor reading would.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest icon accepted, in bytes.
///
/// A guard on an attacker-supplied URL rather than a judgement about art: an
/// icon is a glyph, and anything at this size is a photograph or a mistake.
pub const MAX_BYTES: usize = 1_048_576;

/// Material Design Icons, as gethomepage fetches them.
const MDI_BASE: &str = "https://cdn.jsdelivr.net/npm/@mdi/svg@latest/svg/";

/// Simple Icons, as gethomepage fetches them.
const SI_BASE: &str = "https://cdn.jsdelivr.net/npm/simple-icons@latest/icons/";

/// selfh.st icons. Pinned to `@main`, as gethomepage pins it.
const SELFHST_BASE: &str = "https://cdn.jsdelivr.net/gh/selfhst/icons@main/";

/// The Dashboard Icons collection.
///
/// Pinned to `@main` where gethomepage leaves the ref off entirely. Unpinned,
/// jsDelivr resolves to whatever the default branch holds today, which would
/// make a cache entry and a fresh fetch disagree about what an icon looks like.
/// A panel that repaints because an upstream logo was retouched is a worse
/// outcome than being one commit behind.
const DASHBOARD_BASE: &str = "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/";

/// Where an icon's bytes come from.
///
/// Resolved from the spec string alone, so it is a pure function of config and
/// can be decided at validation time rather than discovered on the first render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Fetched over HTTP and cached on disk.
    Url(String),
    /// Read from this machine's filesystem on every render prep.
    ///
    /// Not cached: a local file is the one source an operator expects to be able
    /// to change and see the result, and re-reading it costs nothing.
    File(PathBuf),
}

/// A resolved icon reference: where the bytes come from, and how they are drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub source: Source,
    /// The ink to draw a single-colour icon in, as a grey level.
    ///
    /// `Some` only for the `mdi-` and `si-` families, whose icons are one
    /// silhouette with no colour of their own — gethomepage renders them as a
    /// CSS mask filled with the theme colour, and this is the same decision on
    /// a paper-white panel. `None` leaves a multi-colour icon's own colours
    /// alone for the quantiser to reduce.
    pub ink: Option<u8>,
}

/// Icon bytes, in whichever form the rasteriser can take them.
#[derive(Debug, Clone, PartialEq)]
pub enum Icon {
    /// SVG markup, drawn as vectors at whatever size the cell gives it.
    Svg {
        markup: String,
        /// The grey this icon asked to be drawn in, from an `mdi-`/`si-` colour
        /// suffix. `None` means "whatever the cell is using", which is the usual
        /// case and the one that lets an icon mute along with a held value.
        ///
        /// Advisory rather than binding: the renderer overrides it on a held cell,
        /// because the muting is what the corner mark means and an icon that
        /// stayed black would undercut it.
        ink: Option<u8>,
    },
    /// Decoded straight-alpha RGBA pixels.
    ///
    /// Decoded here rather than handed to the layout engine encoded, because
    /// takumi's bitmap decoders are not linked into this build: paneld draws no
    /// photographs, and the `png` crate is already present for the encode side.
    Raster {
        data: Vec<u8>,
        width: u32,
        height: u32,
    },
}

/// Rejects an icon spec that could never resolve, naming what is wrong.
///
/// Called from config validation so a typo is a startup error rather than a
/// blank corner of a cell nobody notices.
pub fn validate(spec: &str) -> Result<()> {
    resolve(spec).map(|_| ())
}

/// Resolves a spec to where its bytes come from.
///
/// The branch order is gethomepage's, exactly, including the consequence that
/// the prefix test wins over the extension test: a Dashboard Icon that happens
/// to be named `si-something.png` is unreachable in gethomepage too, and
/// diverging here would mean a config that works in one and not the other.
pub fn resolve(spec: &str) -> Result<Ref> {
    let spec = spec.trim();
    ensure!(!spec.is_empty(), "an icon must not be empty");
    ensure!(
        !spec.contains(char::is_whitespace),
        "icon `{spec}` contains whitespace; an icon reference is a single token"
    );
    ensure!(
        !spec.to_ascii_lowercase().ends_with(".webp"),
        "icon `{spec}` is a webp, which this build cannot decode. \
         Use the `.svg` spelling of the same icon, or `.png`"
    );

    // Absolute URLs and rooted paths are taken as written, which is what makes
    // an icon this machine already has, or one behind a private host, reachable
    // at all.
    if spec.starts_with("http://") || spec.starts_with("https://") {
        return Ok(Ref {
            source: Source::Url(spec.to_owned()),
            ink: None,
        });
    }
    if spec.starts_with('/') {
        return Ok(Ref {
            source: Source::File(PathBuf::from(spec)),
            ink: None,
        });
    }

    let family = spec.split('-').next().unwrap_or(spec);
    match family {
        "mdi" | "si" => {
            let (name, ink) = single_colour(spec, family);
            ensure!(
                !name.is_empty(),
                "icon `{spec}` names the {family} collection but no icon in it"
            );
            let base = if family == "mdi" { MDI_BASE } else { SI_BASE };
            Ok(Ref {
                source: Source::Url(format!("{base}{name}.svg")),
                ink: Some(ink),
            })
        }
        "sh" => {
            let (name, extension) = selfhst(spec);
            ensure!(
                !name.is_empty(),
                "icon `{spec}` names the selfh.st collection but no icon in it"
            );
            Ok(Ref {
                source: Source::Url(format!("{SELFHST_BASE}{extension}/{name}.{extension}")),
                ink: None,
            })
        }
        _ => {
            let (name, extension) = match spec.strip_suffix(".svg") {
                Some(name) => (name, "svg"),
                None => (spec.strip_suffix(".png").unwrap_or(spec), "png"),
            };
            ensure!(
                !name.is_empty(),
                "icon `{spec}` has an extension but no name"
            );
            Ok(Ref {
                source: Source::Url(format!("{DASHBOARD_BASE}{extension}/{name}.{extension}")),
                ink: None,
            })
        }
    }
}

/// Splits an `mdi-`/`si-` spec into its icon name and the grey to draw it in.
///
/// The colour suffix is gethomepage's: a literal `-#` followed by exactly six
/// hex digits, anchored to the end. Three-digit shorthand and eight-digit alpha
/// are not accepted, because they are not accepted there either.
///
/// On a greyscale panel the colour cannot be honoured as written, so it is
/// reduced to its luminance. That keeps a copied gethomepage config working and
/// keeps its *intent* — "this one should be lighter" — while being honest that
/// hue is not a thing this panel has.
fn single_colour(spec: &str, family: &str) -> (String, u8) {
    let body = spec
        .strip_prefix(family)
        .and_then(|rest| rest.strip_prefix('-'))
        .unwrap_or("");
    let body = body.strip_suffix(".svg").unwrap_or(body);

    match body.rsplit_once("-#") {
        Some((name, hex)) if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) => {
            let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0);
            (
                name.to_owned(),
                luminance(channel(0), channel(2), channel(4)),
            )
        }
        // No suffix: black, which is the most contrast a paper-white panel has
        // to offer and the analogue of gethomepage's theme colour.
        _ => (body.to_owned(), 0),
    }
}

/// Rec. 709 luma, which is how a colour becomes a grey level everywhere else in
/// this program.
fn luminance(red: u8, green: u8, blue: u8) -> u8 {
    let luma = 0.2126 * red as f32 + 0.7152 * green as f32 + 0.0722 * blue as f32;
    luma.round().clamp(0.0, 255.0) as u8
}

/// Splits an `sh-` spec into its icon name and the subdirectory it lives in.
///
/// The extension defaults to `png` because that is selfh.st's default in
/// gethomepage, even though `svg` is the better choice on this panel. Diverging
/// would mean the same config line resolving to different artwork in the two
/// projects, which is worse than a default that is merely suboptimal — and
/// `sh-name.svg` says so explicitly.
fn selfhst(spec: &str) -> (String, &'static str) {
    let body = spec.strip_prefix("sh-").unwrap_or("");
    match body.strip_suffix(".svg") {
        Some(name) => (name.to_owned(), "svg"),
        None => (body.strip_suffix(".png").unwrap_or(body).to_owned(), "png"),
    }
}

/// Fetches and caches icons.
#[derive(Debug)]
pub struct Store {
    /// Where fetched bytes are kept between runs.
    dir: PathBuf,
    /// Built once and reused, so a dashboard with a dozen icons opens one
    /// connection pool rather than a dozen.
    http: reqwest::Client,
}

impl Store {
    /// Builds the store, creating the cache directory.
    ///
    /// A directory that cannot be created is not fatal: icons then miss on every
    /// render prep and are re-fetched, which is slower and still correct. The
    /// error is returned so the caller can say so once rather than on every
    /// frame.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating the icon cache directory {}", dir.display()))?;
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .context("building the icon HTTP client")?;
        Ok(Self { dir, http })
    }

    /// Resolves every spec, keyed by the spec itself.
    ///
    /// Cannot fail as a whole. A spec that does not resolve is simply absent from
    /// the result, having been logged, because one unreachable CDN must not blank
    /// a dashboard.
    pub async fn fetch(&self, specs: &[String]) -> HashMap<String, Icon> {
        let mut icons = HashMap::new();
        for spec in specs {
            if icons.contains_key(spec) {
                continue;
            }
            match self.one(spec).await {
                Ok(icon) => {
                    icons.insert(spec.clone(), icon);
                }
                Err(error) => tracing::warn!(
                    icon = spec.as_str(),
                    error = format!("{error:#}"),
                    "could not resolve this icon; its cell renders without one"
                ),
            }
        }
        icons
    }

    async fn one(&self, spec: &str) -> Result<Icon> {
        let reference = resolve(spec)?;
        let bytes = match &reference.source {
            Source::File(path) => {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("reading the icon file {}", path.display()))?;
                ensure!(
                    bytes.len() <= MAX_BYTES,
                    "the icon file {} is {} bytes, over the {MAX_BYTES}-byte ceiling",
                    path.display(),
                    bytes.len()
                );
                bytes
            }
            Source::Url(url) => self.bytes(url).await?,
        };
        decode(&bytes, reference.ink)
            .with_context(|| format!("decoding icon `{spec}` from {:?}", reference.source))
    }

    /// The bytes for a URL, from the cache if it holds them.
    async fn bytes(&self, url: &str) -> Result<Vec<u8>> {
        let path = self.cache_path(url);
        if let Ok(cached) = std::fs::read(&path) {
            return Ok(cached);
        }

        let response = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("reading the response body of GET {url}"))?;
        ensure!(
            status.is_success(),
            "GET {url} returned HTTP {status}; the icon name is probably not in that collection"
        );
        ensure!(
            bytes.len() <= MAX_BYTES,
            "GET {url} returned {} bytes, over the {MAX_BYTES}-byte ceiling",
            bytes.len()
        );

        // Written best-effort: a cache that cannot be written costs a round trip
        // per render, which is not worth failing a frame over.
        if let Err(error) = std::fs::write(&path, &bytes) {
            tracing::debug!(
                path = %path.display(),
                error = %error,
                "could not write the icon cache entry; it will be fetched again"
            );
        }
        Ok(bytes.to_vec())
    }

    /// Where a URL's bytes live.
    ///
    /// Named by a hash of the URL rather than by the icon's own name: a name is
    /// attacker-supplied through the config and could otherwise escape the cache
    /// directory or collide across collections.
    fn cache_path(&self, url: &str) -> PathBuf {
        use std::fmt::Write;
        let mut name = String::with_capacity(64);
        for byte in Sha256::digest(url.as_bytes()) {
            let _ = write!(name, "{byte:02x}");
        }
        self.dir.join(name)
    }
}

/// Turns fetched bytes into something the rasteriser can draw.
///
/// The two families are told apart by their bytes rather than by the URL's
/// extension, so a collection that serves an SVG from a `.png` path still works.
fn decode(bytes: &[u8], ink: Option<u8>) -> Result<Icon> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return decode_png(bytes);
    }

    let markup = std::str::from_utf8(bytes).context("an icon is neither a PNG nor UTF-8 text")?;
    ensure!(
        markup.contains("<svg"),
        "an icon's bytes are neither a PNG nor SVG markup"
    );
    Ok(Icon::Svg {
        markup: match ink {
            Some(_) => follow_current_colour(markup),
            None => markup.to_owned(),
        },
        ink,
    })
}

/// Makes a single-colour icon take its ink from whatever is drawing it.
///
/// `fill` is set as a presentation attribute on the root element, so it cascades
/// into every path that does not set its own — which is every path in the `mdi`
/// and `si` collections, whose icons are one silhouette. A multi-colour icon never
/// reaches here, so nothing legitimate is being overridden.
///
/// `currentColor` rather than a literal grey, because the renderer supplies the
/// grey by injecting `color` on this same element. Setting a literal here as well
/// produced two `color` attributes on one element, which is malformed XML: usvg
/// rejected the whole document and every icon silently drew nothing.
fn follow_current_colour(markup: &str) -> String {
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
        " fill=\"currentColor\"",
        &markup[at..]
    )
}

/// Decodes a PNG to straight-alpha RGBA.
///
/// Palette, greyscale and 16-bit inputs are all normalised up front, because the
/// collections serve all three and the layout engine takes exactly one shape.
fn decode_png(bytes: &[u8]) -> Result<Icon> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("reading the PNG header")?;
    let mut buffer = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buffer)
        .context("decoding the PNG pixels")?;
    buffer.truncate(info.buffer_size());

    let (width, height) = (info.width, info.height);
    let pixels = (width as usize) * (height as usize);
    let data = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => widen(&buffer, 3, pixels, |px, out| {
            out.extend_from_slice(px);
            out.push(255);
        }),
        png::ColorType::GrayscaleAlpha => widen(&buffer, 2, pixels, |px, out| {
            out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
        }),
        png::ColorType::Grayscale => widen(&buffer, 1, pixels, |px, out| {
            out.extend_from_slice(&[px[0], px[0], px[0], 255]);
        }),
        other => bail!("a PNG icon is {other:?}, which normalisation should have removed"),
    };
    ensure!(
        data.len() == pixels * 4,
        "a PNG icon decoded to {} bytes, not the {} its {width}x{height} needs",
        data.len(),
        pixels * 4
    );
    Ok(Icon::Raster {
        data,
        width,
        height,
    })
}

/// Expands `stride`-byte pixels to RGBA.
fn widen(
    buffer: &[u8],
    stride: usize,
    pixels: usize,
    mut expand: impl FnMut(&[u8], &mut Vec<u8>),
) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels * 4);
    for pixel in buffer.chunks_exact(stride) {
        expand(pixel, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(spec: &str) -> String {
        match resolve(spec).expect("should resolve").source {
            Source::Url(url) => url,
            other => panic!("{spec} resolved to {other:?}, not a URL"),
        }
    }

    #[test]
    fn every_gethomepage_form_resolves_to_its_collection() {
        for (spec, expected) in [
            (
                "mdi-thermometer",
                "https://cdn.jsdelivr.net/npm/@mdi/svg@latest/svg/thermometer.svg",
            ),
            (
                "mdi-thermometer.svg",
                "https://cdn.jsdelivr.net/npm/@mdi/svg@latest/svg/thermometer.svg",
            ),
            (
                "si-homeassistant",
                "https://cdn.jsdelivr.net/npm/simple-icons@latest/icons/homeassistant.svg",
            ),
            (
                "sh-plex",
                "https://cdn.jsdelivr.net/gh/selfhst/icons@main/png/plex.png",
            ),
            (
                "sh-plex.svg",
                "https://cdn.jsdelivr.net/gh/selfhst/icons@main/svg/plex.svg",
            ),
            (
                "sh-plex.png",
                "https://cdn.jsdelivr.net/gh/selfhst/icons@main/png/plex.png",
            ),
            (
                "plex",
                "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/png/plex.png",
            ),
            (
                "plex.png",
                "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/png/plex.png",
            ),
            (
                "plex.svg",
                "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/svg/plex.svg",
            ),
            ("https://example.test/a.svg", "https://example.test/a.svg"),
            ("http://example.test/a.png", "http://example.test/a.png"),
        ] {
            assert_eq!(url(spec), expected, "resolving `{spec}`");
        }
    }

    #[test]
    fn a_rooted_path_is_a_local_file() {
        assert_eq!(
            resolve("/var/lib/paneld/mine.svg").unwrap().source,
            Source::File(PathBuf::from("/var/lib/paneld/mine.svg"))
        );
    }

    #[test]
    fn only_the_single_colour_collections_carry_an_ink() {
        // gethomepage renders mdi and si as a mask filled with the theme colour
        // and leaves every other collection its own artwork. Same split here.
        assert_eq!(resolve("mdi-home").unwrap().ink, Some(0));
        assert_eq!(resolve("si-plex").unwrap().ink, Some(0));
        assert_eq!(resolve("sh-plex").unwrap().ink, None);
        assert_eq!(resolve("plex.svg").unwrap().ink, None);
        assert_eq!(resolve("/tmp/a.svg").unwrap().ink, None);
    }

    #[test]
    fn a_colour_suffix_becomes_a_grey_level() {
        // The suffix is honoured as luminance, so a config copied from
        // gethomepage keeps working and keeps its "lighter than the rest" intent.
        let white = resolve("mdi-home-#ffffff").unwrap();
        assert_eq!(white.ink, Some(255));
        assert_eq!(
            url("mdi-home-#ffffff"),
            "https://cdn.jsdelivr.net/npm/@mdi/svg@latest/svg/home.svg",
            "the suffix must not leak into the icon name"
        );
        assert_eq!(resolve("mdi-home-#000000").unwrap().ink, Some(0));
        // Rec. 709: green dominates, blue barely registers.
        assert_eq!(resolve("mdi-home-#00ff00").unwrap().ink, Some(182));
        assert_eq!(resolve("mdi-home-#0000ff").unwrap().ink, Some(18));
    }

    #[test]
    fn a_malformed_colour_suffix_stays_part_of_the_name() {
        // gethomepage anchors the match to exactly six hex digits. Anything else
        // is part of the icon's name, so it must reach the CDN intact rather than
        // being silently trimmed into a different icon.
        for spec in ["mdi-home-#fff", "mdi-home-#ffffffff", "mdi-home-#nothex"] {
            let name = spec.strip_prefix("mdi-").unwrap();
            assert_eq!(
                url(spec),
                format!("{MDI_BASE}{name}.svg"),
                "resolving `{spec}`"
            );
            assert_eq!(resolve(spec).unwrap().ink, Some(0));
        }
    }

    #[test]
    fn the_prefix_test_wins_over_the_extension_test() {
        // gethomepage's `icon.split("-")[0]` makes a dashboard icon named
        // `si-anything.png` unreachable. Diverging would mean a config line that
        // works in one project and not the other.
        assert_eq!(
            url("si-anything.png"),
            "https://cdn.jsdelivr.net/npm/simple-icons@latest/icons/anything.png.svg"
        );
    }

    #[test]
    fn an_unusable_spec_is_rejected_with_a_reason() {
        for (spec, expected) in [
            ("", "must not be empty"),
            ("   ", "must not be empty"),
            ("two words", "whitespace"),
            ("plex.webp", "webp"),
            ("sh-plex.webp", "webp"),
            ("mdi-", "no icon in it"),
            ("sh-", "no icon in it"),
        ] {
            let error = validate(spec).expect_err(&format!("`{spec}` must be rejected"));
            let message = format!("{error:#}");
            assert!(
                message.contains(expected),
                "rejecting `{spec}`: {message} should mention {expected}"
            );
        }
    }

    #[test]
    fn a_spec_is_trimmed_before_it_is_read() {
        assert_eq!(url("  mdi-home  "), format!("{MDI_BASE}home.svg"));
    }

    #[test]
    fn a_cache_entry_is_named_by_a_hash_of_its_url() {
        // The icon's own name comes from config and must never reach the
        // filesystem, or `../` in a name would escape the cache directory.
        let store = Store::new(std::env::temp_dir().join("paneld-icon-test")).unwrap();
        let path = store.cache_path("https://example.test/../../etc/passwd");
        assert_eq!(path.parent(), Some(store.dir.as_path()));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 64);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_single_colour_icon_is_told_to_follow_the_host_colour() {
        let icon = decode(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0h1v1z"/></svg>"#,
            Some(0x66),
        )
        .unwrap();
        let Icon::Svg { markup, ink } = icon else {
            panic!("SVG bytes must decode to SVG");
        };
        assert_eq!(ink, Some(0x66), "the requested grey travels with the icon");
        assert!(markup.starts_with(r#"<svg fill="currentColor" xmlns="#));
        // The regression this shape exists to prevent: painting a literal colour
        // here *as well as* at the renderer produced two `color` attributes on one
        // element. That is malformed XML, usvg rejected the whole document, and
        // every icon on the dashboard silently drew nothing at all. So this stage
        // contributes none: the renderer is the single owner of the grey, and
        // `render::paint_svg` is where the composed result is asserted.
        assert_eq!(
            markup.matches("color=").count(),
            0,
            "the fetch stage must leave the colour to the renderer: {markup}"
        );
    }

    #[test]
    fn a_multi_colour_icon_keeps_its_own_artwork() {
        let source = r##"<svg xmlns="http://www.w3.org/2000/svg"><path fill="#e5a00d" d="M0 0h1v1z"/></svg>"##;
        assert_eq!(
            decode(source.as_bytes(), None).unwrap(),
            Icon::Svg {
                markup: source.to_owned(),
                ink: None
            }
        );
    }

    #[test]
    fn bytes_that_are_no_kind_of_icon_are_rejected() {
        for bytes in [b"not markup".as_slice(), &[0xff, 0xd8, 0xff, 0xe0]] {
            assert!(decode(bytes, None).is_err());
        }
    }

    #[test]
    fn a_png_icon_decodes_to_rgba() {
        // Every colour type the collections serve widens to the one shape the
        // layout engine takes, so a greyscale logo is not a blank cell.
        for (colour, depth) in [
            (png::ColorType::Rgba, png::BitDepth::Eight),
            (png::ColorType::Rgb, png::BitDepth::Eight),
            (png::ColorType::Grayscale, png::BitDepth::Eight),
            (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight),
        ] {
            let bytes = encode_png(4, 3, colour, depth);
            let icon = decode(&bytes, None).unwrap_or_else(|e| panic!("{colour:?}: {e:#}"));
            assert_eq!(
                icon,
                Icon::Raster {
                    data: expected_rgba(4, 3, colour),
                    width: 4,
                    height: 3,
                },
                "decoding a {colour:?} PNG"
            );
        }
    }

    /// A solid mid-grey image in the given colour type, so the widening is
    /// checked against known bytes rather than against itself.
    fn encode_png(
        width: u32,
        height: u32,
        colour: png::ColorType,
        depth: png::BitDepth,
    ) -> Vec<u8> {
        let channels = match colour {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            other => panic!("unsupported {other:?}"),
        };
        let mut raw = Vec::new();
        for _ in 0..(width * height) {
            for channel in 0..channels {
                let alpha = matches!(
                    (colour, channel),
                    (png::ColorType::GrayscaleAlpha, 1) | (png::ColorType::Rgba, 3)
                );
                raw.push(if alpha { 0x80 } else { 0x40 });
            }
        }

        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(std::io::Cursor::new(&mut out), width, height);
        encoder.set_color(colour);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&raw).unwrap();
        writer.finish().unwrap();
        out
    }

    fn expected_rgba(width: u32, height: u32, colour: png::ColorType) -> Vec<u8> {
        let pixel = match colour {
            png::ColorType::Grayscale => [0x40, 0x40, 0x40, 0xff],
            png::ColorType::GrayscaleAlpha => [0x40, 0x40, 0x40, 0x80],
            png::ColorType::Rgb => [0x40, 0x40, 0x40, 0xff],
            png::ColorType::Rgba => [0x40, 0x40, 0x40, 0x80],
            other => panic!("unsupported {other:?}"),
        };
        pixel.repeat((width * height) as usize)
    }
}
