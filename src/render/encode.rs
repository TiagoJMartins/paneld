//! The tail of the render pipeline: quantise an RGBA raster to the panel's
//! palette, pack it at the narrowest bit depth that palette allows, and encode a
//! PNG.
//!
//! Three decisions here are load-bearing and easy to get wrong.
//!
//! **Quantisation happens in linear light.** Error diffusion is an averaging
//! process: a 50% grey rendered as a mix of darker and lighter pixels only
//! *reads* as 50% grey if the arithmetic that chose those pixels worked in the
//! space where light adds. Diffusing on gamma-encoded bytes makes midtones come
//! out wrong. `dithr` applies no transfer function of its own —
//! `Sample::to_unit_f32` is a plain divide — so we hand it `f32` samples we have
//! linearised ourselves.
//!
//! **The quantisation targets are the panel's levels, not a uniform linear
//! grid.** A 16-level greyscale panel's levels are uniform in *gamma* space: PNG
//! grey code `k` at bit depth 4 means `k/15` of full scale. So the palette we
//! quantise against is those levels *expressed in linear light*, which is
//! unevenly spaced. That combination — linear-light arithmetic, panel-spaced
//! targets — is what makes both the midtones and the endpoints correct. It also
//! collapses every palette class onto one code path: after quantisation each
//! pixel is a palette index, which is the grey level for a greyscale panel and
//! the PLTE index for a colour one.
//!
//! **Ordered dithering is implemented here rather than taken from `dithr`**, for
//! the reason documented on [`ordered_dither`].

use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use dithr::{Palette32F, QuantizeMode, rgb_32f};
use png::{BitDepth, ColorType, DeflateCompression, Encoder, Filter};
use sha2::{Digest, Sha256};

use crate::config::{Dither, Palette};

/// Hard ceiling on the encoded frame, in bytes.
///
/// A limit of the non-PSRAM device boards: over it, the fetch fails outright.
pub const MAX_FRAME_BYTES: usize = 90_000;

/// Quantises a straight-alpha RGBA raster to `palette` and encodes it as a PNG.
///
/// `rgba` is row-major, four bytes per pixel, as produced by the rasteriser.
pub fn quantise_and_encode(
    rgba: &[u8],
    width: u32,
    height: u32,
    palette: Palette,
    dither: Dither,
) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    ensure!(w > 0 && h > 0, "cannot encode a {width}x{height} frame");
    ensure!(
        rgba.len() == w * h * 4,
        "raster is {} bytes, expected {} for a {width}x{height} RGBA frame",
        rgba.len(),
        w * h * 4
    );

    let spec = PaletteSpec::of(palette);
    let targets = Palette32F::new(spec.linear_levels())
        .map_err(|e| anyhow::anyhow!("building the {palette:?} quantisation palette: {e}"))?;

    let mut linear = to_linear_over_white(rgba, spec.grayscale);
    match dither {
        // Nothing to do: the nearest-level lookup below *is* undithered
        // quantisation.
        Dither::None => {}
        Dither::Bayer => ordered_dither(&mut linear, w, h, &spec, &targets),
        Dither::Atkinson | Dither::FloydSteinberg => {
            diffuse(&mut linear, w, h, dither, &targets)?;
        }
    }

    // Every sample is now a palette colour, so this lookup recovers the index the
    // quantiser chose rather than approximating it.
    let mut indices = Vec::with_capacity(w * h);
    for px in linear.chunks_exact(3) {
        indices.push(targets.nearest_rgb_index([px[0], px[1], px[2]]) as u8);
    }

    let packed = pack_scanlines(&indices, w, h, spec.bit_depth as usize);
    let bytes = encode_png(&packed, width, height, &spec)?;

    if bytes.len() >= MAX_FRAME_BYTES {
        tracing::warn!(
            frame_bytes = bytes.len(),
            limit = MAX_FRAME_BYTES,
            "encoded frame is at or over the device's fetch ceiling; the device will fail to fetch it"
        );
    }
    Ok(bytes)
}

/// The frame's filename stem: SHA-256 over the encoded bytes, truncated to 16
/// bytes and hex encoded.
///
/// Content addressing *is* the cache-invalidation mechanism. The device treats
/// the filename as its cache key — not the URL and not the bytes — so an
/// unchanged filename means it repaints from its own flash without downloading.
/// The hash must therefore cover the final encoded bytes rather than the render
/// inputs.
///
/// 32 hex characters is deliberate: one client folds a long filename to its first
/// 7 plus last 17 characters, which for `<stem>.png` still retains 20 hex
/// characters, so the fold cannot collide.
pub fn frame_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Converts sRGB bytes to linear light, compositing over white.
///
/// The raster arrives with straight alpha and a frame must be opaque, so anything
/// the dashboard left transparent becomes paper. Compositing happens after
/// linearisation because blending, like diffusion, is only correct where light
/// adds.
///
/// For a greyscale panel each pixel is reduced to its Rec. 709 luminance and that
/// value replicated across the three channels. Reducing here rather than letting
/// the nearest-colour search do it matters: a search over grey palette entries
/// minimises Euclidean RGB distance, which resolves to the *mean* of the channels
/// rather than their luminance, so a saturated colour would land on the wrong
/// grey.
fn to_linear_over_white(rgba: &[u8], grayscale: bool) -> Vec<f32> {
    let lut = &*SRGB_TO_LINEAR;
    let mut linear = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        let alpha = f32::from(px[3]) / 255.0;
        let over_white = |channel: u8| lut[channel as usize] * alpha + (1.0 - alpha);
        let (r, g, b) = (over_white(px[0]), over_white(px[1]), over_white(px[2]));

        if grayscale {
            let luminance = 0.212_6 * r + 0.715_2 * g + 0.072_2 * b;
            linear.extend_from_slice(&[luminance, luminance, luminance]);
        } else {
            linear.extend_from_slice(&[r, g, b]);
        }
    }
    linear
}

/// sRGB byte to linear-light lookup. A table because the input is always a `u8`,
/// so the transfer function has 256 possible answers rather than needing a `powf`
/// per channel per pixel.
static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut lut = [0.0f32; 256];
    for (value, slot) in lut.iter_mut().enumerate() {
        *slot = srgb_to_linear(value as f32 / 255.0);
    }
    lut
});

/// The sRGB electro-optical transfer function.
fn srgb_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Runs error diffusion in place over linear-light samples.
///
/// Diffusion needs no threshold tuning — it quantises to the nearest level and
/// carries the residual to later pixels — so `dithr`'s implementations are used
/// directly.
fn diffuse(
    linear: &mut [f32],
    w: usize,
    h: usize,
    dither: Dither,
    targets: &Palette32F,
) -> Result<()> {
    // Stride is counted in samples, not bytes.
    let mut buffer = rgb_32f(linear, w, h, w * 3)
        .map_err(|e| anyhow::anyhow!("wrapping the {w}x{h} raster for dithering: {e}"))?;
    let mode = QuantizeMode::Palette(targets);

    match dither {
        Dither::Atkinson => dithr::diffusion::atkinson_in_place(&mut buffer, mode),
        Dither::FloydSteinberg => dithr::diffusion::floyd_steinberg_in_place(&mut buffer, mode),
        other => unreachable!("{other:?} is not error diffusion"),
    }
    .map_err(|e| anyhow::anyhow!("dithering with {dither:?}: {e}"))
}

/// Ordered (Bayer) dithering, implemented here rather than taken from `dithr`.
///
/// `dithr`'s ordered path biases every pixel by a threshold of at most ±0.25 in
/// unit space regardless of how far apart the palette's levels actually are: its
/// `ordered_threshold_unit` scales only by a `strength` constant of 64/255 that
/// is crate-private, so there is no way to pass a different one. For a two-level
/// palette ±0.25 is a sensible quarter step. For the 16-level greyscale panel
/// this program exists for it is roughly four levels of bias, which textures flat
/// paper and collapses distinct dark tones onto a single level.
///
/// So the threshold is applied against the palette's *actual* spacing. For a
/// greyscale panel that means interpolating between the two levels which bracket
/// the sample, which stays exact even though those levels are unevenly spaced in
/// linear light: the fraction of pixels pushed to the upper level equals the
/// sample's position between the two. For a colour panel, whose palette entries
/// sit at the extremes of each channel, the equivalent is a ±0.5 per-channel
/// bias before the nearest-colour search.
///
/// Unlike diffusion, this is stateless per pixel: a pixel's result depends only
/// on its own value and its coordinates. That is what makes frames stable — a
/// change in one part of the dashboard leaves every other pixel byte-identical,
/// so the frame hash only moves when something visible actually moved.
fn ordered_dither(
    linear: &mut [f32],
    w: usize,
    h: usize,
    spec: &PaletteSpec,
    targets: &Palette32F,
) {
    let grey_levels = spec.grayscale.then(|| spec.grey_ramp());

    for y in 0..h {
        for x in 0..w {
            let threshold = bayer_threshold(x, y);
            let px = &mut linear[(y * w + x) * 3..(y * w + x) * 3 + 3];

            match &grey_levels {
                Some(levels) => {
                    let chosen = pick_bracketing_level(levels, px[0], threshold);
                    px.fill(chosen);
                }
                None => {
                    for channel in px.iter_mut() {
                        *channel = (*channel + threshold - 0.5).clamp(0.0, 1.0);
                    }
                    let snapped = targets.nearest_rgb_color([px[0], px[1], px[2]]);
                    px.copy_from_slice(&snapped);
                }
            }
        }
    }
}

/// The 8x8 Bayer matrix: recursively generated ranks that spread thresholds as
/// evenly as possible over the tile.
const BAYER_8X8: [u8; 64] = [
    0, 32, 8, 40, 2, 34, 10, 42, //
    48, 16, 56, 24, 50, 18, 58, 26, //
    12, 44, 4, 36, 14, 46, 6, 38, //
    60, 28, 52, 20, 62, 30, 54, 22, //
    3, 35, 11, 43, 1, 33, 9, 41, //
    51, 19, 59, 27, 49, 17, 57, 25, //
    15, 47, 7, 39, 13, 45, 5, 37, //
    63, 31, 55, 23, 61, 29, 53, 21,
];

/// This pixel's threshold, strictly between 0 and 1.
///
/// Never exactly 0 or 1, which is what guarantees a sample sitting exactly on a
/// palette level is left on it rather than nudged off — flat paper stays flat.
fn bayer_threshold(x: usize, y: usize) -> f32 {
    let rank = BAYER_8X8[(y % 8) * 8 + (x % 8)];
    (f32::from(rank) + 0.5) / 64.0
}

/// Picks whichever of the two levels bracketing `value` this pixel's threshold
/// selects.
///
/// `levels` must be ascending.
fn pick_bracketing_level(levels: &[f32], value: f32, threshold: f32) -> f32 {
    let upper = levels.partition_point(|&level| level <= value);
    if upper == 0 {
        return levels[0];
    }
    if upper >= levels.len() {
        return levels[levels.len() - 1];
    }

    let (lower, higher) = (levels[upper - 1], levels[upper]);
    let span = higher - lower;
    let fraction = if span > 0.0 {
        (value - lower) / span
    } else {
        0.0
    };
    if fraction > threshold { higher } else { lower }
}

/// Packs one palette index per pixel into PNG scanlines.
///
/// PNG requires the leftmost pixel in the most significant bits and every row
/// independently padded to a byte boundary; `png::Writer::write_image_data` does
/// no packing and rejects a buffer of any other length. The padding bits are
/// ignored by decoders but still compressed, so they are left zero to keep output
/// byte-identical for identical input.
fn pack_scanlines(indices: &[u8], w: usize, h: usize, bits: usize) -> Vec<u8> {
    debug_assert!(matches!(bits, 1 | 2 | 4 | 8));
    let per_byte = 8 / bits;
    let row_bytes = w.div_ceil(per_byte);
    let mut packed = vec![0u8; row_bytes * h];

    for y in 0..h {
        let row = &mut packed[y * row_bytes..(y + 1) * row_bytes];
        for x in 0..w {
            let index = indices[y * w + x];
            let shift = 8 - bits * (x % per_byte + 1);
            row[x / per_byte] |= index << shift;
        }
    }
    packed
}

fn encode_png(packed: &[u8], width: u32, height: u32, spec: &PaletteSpec) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, width, height);
        encoder.set_color(spec.color_type);
        encoder.set_depth(spec.bit_depth);
        if spec.color_type == ColorType::Indexed {
            encoder.set_palette(spec.plte());
        }
        // Pinned rather than left to the defaults: `Compression` and
        // `DeflateCompression` are `#[non_exhaustive]` with defaults upstream may
        // retune, and the filename-stability behaviour rests on the encoder being
        // deterministic.
        encoder.set_deflate_compression(DeflateCompression::Level(6));
        encoder.set_filter(Filter::Adaptive);

        let mut writer = encoder.write_header().context("writing the PNG header")?;
        writer
            .write_image_data(packed)
            .context("writing PNG image data")?;
        // Explicit rather than left to `Drop`, which swallows the error.
        writer.finish().context("finishing the PNG")?;
    }
    Ok(out)
}

/// How one palette maps onto PNG output.
struct PaletteSpec {
    /// The panel's output levels as sRGB triples, in palette-index order.
    levels: Vec<[u8; 3]>,
    /// Whether those levels form a grey ramp, in which case the raster is
    /// reduced to luminance and the index is the PNG grey sample.
    grayscale: bool,
    color_type: ColorType,
    bit_depth: BitDepth,
}

impl PaletteSpec {
    fn of(palette: Palette) -> Self {
        match palette {
            // For a greyscale panel the palette index *is* the PNG grey sample:
            // at bit depth 4 a sample of `k` already means `k/15` of full scale,
            // which is exactly the level we quantised to. No PLTE needed.
            Palette::Gray16 => Self::grayscale(16, BitDepth::Four),
            Palette::Gray4 => Self::grayscale(4, BitDepth::Two),
            Palette::Mono => Self::grayscale(2, BitDepth::One),
            // Four entries need two bits.
            Palette::Bwry => Self::indexed(vec![BLACK, WHITE, RED, YELLOW], BitDepth::Two),
            // Six entries do not fit in two bits.
            Palette::Spectra6 => {
                Self::indexed(vec![BLACK, WHITE, YELLOW, RED, BLUE, GREEN], BitDepth::Four)
            }
        }
    }

    fn grayscale(levels: u8, bit_depth: BitDepth) -> Self {
        let max = u32::from(levels - 1);
        let levels = (0..u32::from(levels))
            .map(|k| {
                // Spread the levels evenly across 0..=255, matching how a decoder
                // scales an n-bit grey sample to 8 bits.
                let v = (k * 255 / max) as u8;
                [v, v, v]
            })
            .collect();
        Self {
            levels,
            grayscale: true,
            color_type: ColorType::Grayscale,
            bit_depth,
        }
    }

    fn indexed(levels: Vec<[u8; 3]>, bit_depth: BitDepth) -> Self {
        Self {
            levels,
            grayscale: false,
            color_type: ColorType::Indexed,
            bit_depth,
        }
    }

    /// The quantisation targets, in linear light.
    fn linear_levels(&self) -> Vec<[f32; 3]> {
        let lut = &*SRGB_TO_LINEAR;
        self.levels
            .iter()
            .map(|&[r, g, b]| [lut[r as usize], lut[g as usize], lut[b as usize]])
            .collect()
    }

    /// The grey ramp in linear light, ascending. Only meaningful when
    /// [`Self::grayscale`].
    fn grey_ramp(&self) -> Vec<f32> {
        let lut = &*SRGB_TO_LINEAR;
        self.levels.iter().map(|&[v, ..]| lut[v as usize]).collect()
    }

    /// Raw PLTE chunk contents: flattened RGB triples.
    fn plte(&self) -> Vec<u8> {
        self.levels.iter().flatten().copied().collect()
    }
}

const BLACK: [u8; 3] = [0, 0, 0];
const WHITE: [u8; 3] = [255, 255, 255];
const RED: [u8; 3] = [255, 0, 0];
const YELLOW: [u8; 3] = [255, 255, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const GREEN: [u8; 3] = [0, 255, 0];

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_DITHER: [Dither; 4] = [
        Dither::Atkinson,
        Dither::FloydSteinberg,
        Dither::Bayer,
        Dither::None,
    ];

    /// A gradient over most of the frame plus a flat band: enough tone for a
    /// dither to have something to do, and deterministic.
    fn ramp(w: u32, h: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = ((x * 255) / w.max(1)) as u8;
                let shade = if y < h / 4 { 0 } else { v };
                rgba.extend_from_slice(&[shade, shade, shade, 255]);
            }
        }
        rgba
    }

    fn flat(w: u32, h: u32, shade: u8) -> Vec<u8> {
        std::iter::repeat_n([shade, shade, shade, 255], (w * h) as usize)
            .flatten()
            .collect()
    }

    /// Decodes a PNG back to (width, height, bit depth, colour type, raw packed
    /// samples).
    fn decode(bytes: &[u8]) -> (u32, u32, BitDepth, ColorType, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("output should decode as a PNG");
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("frame should decode");
        buf.truncate(info.buffer_size());
        (
            info.width,
            info.height,
            reader.info().bit_depth,
            reader.info().color_type,
            buf,
        )
    }

    /// One palette index per pixel, unpacked from a 4-bit image.
    fn levels_4bit(bytes: &[u8]) -> Vec<u8> {
        let (.., samples) = decode(bytes);
        samples
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0F])
            .collect()
    }

    #[test]
    fn output_decodes_at_exactly_the_configured_dimensions() {
        let bytes = quantise_and_encode(&ramp(101, 37), 101, 37, Palette::Gray16, Dither::Atkinson)
            .unwrap();
        let (w, h, ..) = decode(&bytes);
        assert_eq!((w, h), (101, 37), "odd dimensions must survive packing");
    }

    #[test]
    fn gray16_encodes_at_bit_depth_four_with_at_most_sixteen_values() {
        let bytes =
            quantise_and_encode(&ramp(64, 64), 64, 64, Palette::Gray16, Dither::Atkinson).unwrap();
        let (.., depth, color, _) = decode(&bytes);
        assert_eq!(depth, BitDepth::Four);
        assert_eq!(color, ColorType::Grayscale);

        let distinct: std::collections::BTreeSet<u8> = levels_4bit(&bytes).into_iter().collect();
        assert!(
            distinct.len() <= 16,
            "a gray16 panel must not be handed more than 16 levels, got {distinct:?}"
        );
        assert!(
            distinct.len() > 2,
            "a ramp on a 16-level panel should use more than black and white, got {distinct:?}"
        );
    }

    #[test]
    fn mono_encodes_at_bit_depth_one_with_at_most_two_values() {
        let bytes =
            quantise_and_encode(&ramp(64, 64), 64, 64, Palette::Mono, Dither::Atkinson).unwrap();
        let (.., depth, color, samples) = decode(&bytes);
        assert_eq!(depth, BitDepth::One);
        assert_eq!(color, ColorType::Grayscale);

        let distinct: std::collections::BTreeSet<u8> = samples
            .iter()
            .flat_map(|byte| (0..8).map(move |bit| (byte >> (7 - bit)) & 1))
            .collect();
        assert!(distinct.len() <= 2, "mono must be 1 bit, got {distinct:?}");
    }

    #[test]
    fn gray4_encodes_at_bit_depth_two() {
        let bytes =
            quantise_and_encode(&ramp(64, 64), 64, 64, Palette::Gray4, Dither::Atkinson).unwrap();
        let (.., depth, color, _) = decode(&bytes);
        assert_eq!(depth, BitDepth::Two);
        assert_eq!(color, ColorType::Grayscale);
    }

    #[test]
    fn colour_palettes_encode_as_indexed_with_a_matching_plte() {
        for (palette, depth, entries) in [
            (Palette::Bwry, BitDepth::Two, 4),
            (Palette::Spectra6, BitDepth::Four, 6),
        ] {
            let bytes =
                quantise_and_encode(&ramp(32, 32), 32, 32, palette, Dither::Atkinson).unwrap();
            let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
            let reader = decoder.read_info().unwrap();
            let info = reader.info();
            assert_eq!(info.bit_depth, depth, "{palette:?}");
            assert_eq!(info.color_type, ColorType::Indexed, "{palette:?}");
            assert_eq!(
                info.palette
                    .as_ref()
                    .expect("indexed output needs a PLTE")
                    .len(),
                entries * 3,
                "{palette:?} PLTE is flattened RGB triples"
            );
        }
    }

    #[test]
    fn rendering_the_same_input_twice_is_byte_identical() {
        // Load-bearing rather than hygiene: filename stability, and therefore the
        // whole e-ink refresh story, is only correct if the encoder is
        // deterministic.
        let raster = ramp(97, 51);
        for dither in EVERY_DITHER {
            let first = quantise_and_encode(&raster, 97, 51, Palette::Gray16, dither).unwrap();
            let second = quantise_and_encode(&raster, 97, 51, Palette::Gray16, dither).unwrap();
            assert_eq!(first, second, "{dither:?} must be deterministic");
            assert_eq!(frame_hash(&first), frame_hash(&second));
        }
    }

    #[test]
    fn a_changed_region_changes_the_hash_under_every_dither() {
        let base = ramp(64, 64);
        let mut changed = base.clone();
        // A block rather than a single pixel: one pixel can legitimately quantise
        // to the same level it already had.
        for y in 20..30 {
            for x in 20..30 {
                let i = (y * 64 + x) * 4;
                changed[i..i + 3].copy_from_slice(&[200, 200, 200]);
            }
        }

        for dither in EVERY_DITHER {
            let before = quantise_and_encode(&base, 64, 64, Palette::Gray16, dither).unwrap();
            let after = quantise_and_encode(&changed, 64, 64, Palette::Gray16, dither).unwrap();
            assert_ne!(
                frame_hash(&before),
                frame_hash(&after),
                "{dither:?} must notice a changed region"
            );
        }
    }

    #[test]
    fn ordered_dithering_is_stable_outside_the_region_that_changed() {
        // The operational reason `bayer` is offered at all. Error diffusion carries
        // its residual forward, so touching one pixel perturbs everything after it
        // and the frame hash moves even where nothing visibly changed. Ordered
        // dithering is stateless per pixel, so only the changed pixels move.
        let base = ramp(64, 64);
        let mut changed = base.clone();
        let target = (40 * 64 + 40) * 4;
        changed[target..target + 3].copy_from_slice(&[200, 200, 200]);

        let differing = |dither: Dither| {
            let before =
                levels_4bit(&quantise_and_encode(&base, 64, 64, Palette::Gray16, dither).unwrap());
            let after = levels_4bit(
                &quantise_and_encode(&changed, 64, 64, Palette::Gray16, dither).unwrap(),
            );
            before.iter().zip(&after).filter(|(a, b)| a != b).count()
        };

        let ordered = differing(Dither::Bayer);
        let diffused = differing(Dither::Atkinson);
        assert_eq!(
            ordered, 1,
            "ordered dithering must change only the pixel that changed"
        );
        assert!(
            diffused > ordered,
            "error diffusion is expected to spread the change ({diffused} pixels) \
             further than ordered dithering ({ordered})"
        );
    }

    #[test]
    fn every_dither_and_palette_combination_encodes() {
        let raster = ramp(48, 24);
        for palette in [
            Palette::Gray16,
            Palette::Gray4,
            Palette::Mono,
            Palette::Bwry,
            Palette::Spectra6,
        ] {
            for dither in EVERY_DITHER {
                let bytes = quantise_and_encode(&raster, 48, 24, palette, dither)
                    .unwrap_or_else(|e| panic!("{palette:?}/{dither:?}: {e:#}"));
                let (w, h, ..) = decode(&bytes);
                assert_eq!((w, h), (48, 24), "{palette:?}/{dither:?}");
            }
        }
    }

    #[test]
    fn a_kindle_sized_frame_stays_under_the_fetch_ceiling() {
        let (w, h) = (1024, 758);
        let bytes = quantise_and_encode(&ramp(w, h), w, h, Palette::Gray16, Dither::Bayer).unwrap();
        assert!(
            bytes.len() < MAX_FRAME_BYTES,
            "a 1024x758 gray16 frame encoded to {} bytes, over the {MAX_FRAME_BYTES} ceiling",
            bytes.len()
        );
    }

    #[test]
    fn flat_paper_and_flat_ink_survive_every_dither_untextured() {
        // Endpoint fidelity. A dither that biases pixels sitting exactly on a
        // palette level puts a visible texture across the whole background, which
        // is what `dithr`'s own ordered path does at our level count.
        for dither in EVERY_DITHER {
            let white =
                quantise_and_encode(&flat(32, 32, 255), 32, 32, Palette::Gray16, dither).unwrap();
            assert!(
                levels_4bit(&white).iter().all(|&level| level == 15),
                "{dither:?} should leave flat white on level 15"
            );

            let black =
                quantise_and_encode(&flat(32, 32, 0), 32, 32, Palette::Gray16, dither).unwrap();
            assert!(
                levels_4bit(&black).iter().all(|&level| level == 0),
                "{dither:?} should leave flat black on level 0"
            );
        }
    }

    #[test]
    fn every_palette_level_is_reproduced_exactly() {
        // A frame painted in the panel's own levels must come back unchanged
        // rather than being re-dithered into a neighbour.
        for k in 0u32..16 {
            let shade = (k * 255 / 15) as u8;
            let bytes =
                quantise_and_encode(&flat(16, 16, shade), 16, 16, Palette::Gray16, Dither::Bayer)
                    .unwrap();
            assert!(
                levels_4bit(&bytes)
                    .iter()
                    .all(|&level| u32::from(level) == k),
                "level {k} (sRGB {shade}) should round-trip exactly"
            );
        }
    }

    #[test]
    fn transparency_composites_over_white_paper() {
        // A fully transparent raster is paper, not ink.
        let raster = vec![0u8; 32 * 32 * 4];
        let bytes = quantise_and_encode(&raster, 32, 32, Palette::Gray16, Dither::None).unwrap();
        assert!(levels_4bit(&bytes).iter().all(|&level| level == 15));
    }

    #[test]
    fn a_mid_grey_diffuses_to_roughly_the_right_mean_luminance() {
        // The point of working in linear light. A 50%-luminance patch dithered over
        // 16 levels should average back to 50% of full linear scale; doing the
        // arithmetic on gamma-encoded bytes lands nearer 21%.
        let target = 0.5f32;
        let encoded = (linear_to_srgb(target) * 255.0).round() as u8;
        let lut = &*SRGB_TO_LINEAR;

        for dither in [Dither::Atkinson, Dither::FloydSteinberg, Dither::Bayer] {
            let bytes =
                quantise_and_encode(&flat(64, 64, encoded), 64, 64, Palette::Gray16, dither)
                    .unwrap();
            let levels = levels_4bit(&bytes);
            let mean: f32 = levels
                .iter()
                .map(|&k| lut[(u32::from(k) * 255 / 15) as usize])
                .sum::<f32>()
                / levels.len() as f32;
            assert!(
                (mean - target).abs() < 0.02,
                "{dither:?}: mean linear luminance {mean} should be close to {target}"
            );
        }
    }

    fn linear_to_srgb(linear: f32) -> f32 {
        if linear <= 0.003_130_8 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        }
    }

    #[test]
    fn a_saturated_colour_maps_to_its_luminance_on_a_greyscale_panel() {
        // Pure green is far brighter than pure blue. Reducing by Euclidean RGB
        // distance to the grey ramp would rank them identically, so this is what
        // catches a missing luminance step.
        let grey_of = |rgba: Vec<u8>| {
            let bytes = quantise_and_encode(&rgba, 16, 16, Palette::Gray16, Dither::None).unwrap();
            levels_4bit(&bytes)[0]
        };
        let green = grey_of(
            std::iter::repeat_n([0u8, 255, 0, 255], 256)
                .flatten()
                .collect(),
        );
        let blue = grey_of(
            std::iter::repeat_n([0u8, 0, 255, 255], 256)
                .flatten()
                .collect(),
        );
        assert!(
            green > blue,
            "green (level {green}) should read lighter than blue (level {blue})"
        );
    }

    #[test]
    fn frame_hash_is_thirty_two_lowercase_hex_characters() {
        let hash = frame_hash(b"anything");
        assert_eq!(hash.len(), 32);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_ne!(hash, frame_hash(b"anything else"));
    }

    #[test]
    fn rejects_a_raster_that_does_not_match_the_dimensions() {
        let error = quantise_and_encode(&[0; 16], 64, 64, Palette::Gray16, Dither::None)
            .expect_err("a short raster must be rejected");
        assert!(format!("{error:#}").contains("expected"), "{error:#}");
    }

    #[test]
    fn packs_leftmost_pixel_into_the_most_significant_bits() {
        // PNG orders sub-byte samples big-endian within a byte, and pads each row
        // independently. Both are the caller's job.
        assert_eq!(pack_scanlines(&[1, 0], 2, 1, 4), vec![0x10]);
        assert_eq!(pack_scanlines(&[0xF, 0xF], 2, 1, 4), vec![0xFF]);
        assert_eq!(
            pack_scanlines(&[1, 0, 0, 0, 0, 0, 0, 0], 8, 1, 1),
            vec![0x80]
        );
        assert_eq!(
            pack_scanlines(&[1, 1, 1], 3, 1, 1),
            vec![0b1110_0000],
            "padding bits are zero so identical input encodes identically"
        );
        assert_eq!(
            pack_scanlines(&[1, 0, 0, 1], 2, 2, 4),
            vec![0x10, 0x01],
            "rows never share a byte"
        );
    }

    #[test]
    fn the_bayer_threshold_never_reaches_its_endpoints() {
        // What keeps a sample already sitting on a palette level exactly there.
        for y in 0..8 {
            for x in 0..8 {
                let threshold = bayer_threshold(x, y);
                assert!(threshold > 0.0 && threshold < 1.0, "at {x},{y}");
            }
        }
        let mut ranks: Vec<u8> = BAYER_8X8.to_vec();
        ranks.sort_unstable();
        assert_eq!(
            ranks,
            (0..64).collect::<Vec<u8>>(),
            "the matrix must be a permutation of 0..64"
        );
    }

    #[test]
    fn bracketing_picks_the_level_the_threshold_selects() {
        let levels = [0.0, 0.25, 1.0];
        // Exactly on a level: stays there whatever the threshold.
        assert_eq!(pick_bracketing_level(&levels, 0.25, 0.01), 0.25);
        assert_eq!(pick_bracketing_level(&levels, 0.25, 0.99), 0.25);
        // A quarter of the way up the 0.25..1.0 span.
        assert_eq!(pick_bracketing_level(&levels, 0.437_5, 0.1), 1.0);
        assert_eq!(pick_bracketing_level(&levels, 0.437_5, 0.9), 0.25);
        // Outside the ramp clamps to its ends.
        assert_eq!(pick_bracketing_level(&levels, -1.0, 0.5), 0.0);
        assert_eq!(pick_bracketing_level(&levels, 2.0, 0.5), 1.0);
    }
}
