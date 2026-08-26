//! Delivery of rendered frames to a thermal printer bridge.
//!
//! A device with a `sink` is still an ordinary device: its frame is rendered,
//! stored and served exactly as a panel's would be, which is what makes it
//! previewable — point any browser, Kindle or the `preview` subcommand at the
//! same device and you are looking at the bytes the printer will get. The sink
//! is one extra consumer bolted onto the end of the pipeline, not a render
//! path.
//!
//! Paper is the one output in this program that is not idempotent — a repainted
//! panel is free, a reprinted receipt is litter. So nothing here runs on its
//! own: paper moves only when a human posts to the print endpoint, after
//! looking at the same frame the printer will get. Two mechanical guards
//! remain: an all-white frame is refused, because an empty dashboard is not
//! worth a blank receipt, and trailing blank rows are trimmed, because paper
//! ends where the content does.
//!
//! The raster the bridge wants is recovered by *decoding the served PNG* rather
//! than by teeing off the encoder. That costs a millisecond of decode and buys
//! an invariant worth far more: what prints is what previews, by construction,
//! because both come from the same encoded bytes.

use anyhow::{Context, Result, ensure};

/// Threshold between "ink" and "paper" when reading the decoded frame.
///
/// The frames a sink consumes are mono-palette, so every sample is already 0 or
/// 255 and any midpoint reads them correctly.
const DARK_BELOW: u8 = 128;

/// What one printed frame looked like, for the caller's reply and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    pub height_px: u32,
    pub bytes: usize,
}

/// Sends a packed raster to a nanoprint bridge.
///
/// `POST {url}/print/raster` with the raw rows as the body, `?density=` when
/// configured. The bridge replies after the printer acknowledges the job, which
/// takes seconds — the connect timeout is short so a powered-off bridge fails
/// fast, but the overall deadline leaves room for a long receipt to feed.
pub async fn deliver(
    client: &reqwest::Client,
    url: &str,
    density: Option<u8>,
    raster: Vec<u8>,
    width: u32,
) -> Result<Delivery> {
    let height_px = (raster.len() / (width as usize / 8)) as u32;
    let bytes = raster.len();
    let target = match density {
        Some(density) => format!("{url}/print/raster?density={density}"),
        None => format!("{url}/print/raster"),
    };
    let response = client
        .post(target)
        // The bridge documents the body as application/octet-stream. reqwest
        // sends no content type for a raw byte body, so it is set explicitly
        // rather than left to whatever the firmware assumes of a typeless POST.
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(raster)
        .send()
        .await
        .with_context(|| format!("posting {bytes} bytes to {url}/print/raster"))?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "the printer bridge answered {status}: {}",
        response.text().await.unwrap_or_default()
    );
    Ok(Delivery { height_px, bytes })
}

/// Decodes a served PNG frame into the packed rows the printhead wants.
///
/// Output is row-major, one bit per pixel, MSB-first leftmost, dark = 1 —
/// `width / 8` bytes per row. Trailing all-white rows are trimmed; an all-white
/// frame decodes to an empty vector, which callers treat as "nothing to print".
pub fn raster_from_png(png_bytes: &[u8], width: u32) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    // Expands sub-byte grey to one byte per sample, so the loop below reads
    // samples rather than re-implementing bit unpacking per bit depth.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .context("reading the frame's PNG header")?;
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .context("frame too large to decode")?
    ];
    let info = reader
        .next_frame(&mut buf)
        .context("decoding the frame PNG")?;

    ensure!(
        info.width == width,
        "frame is {} pixels wide, the sink expects {width}",
        info.width
    );
    let samples_per_px = match info.color_type {
        png::ColorType::Grayscale => 1,
        // Mono frames are encoded as greyscale; anything else means the device
        // was not `palette = "mono"`, which validation should have rejected.
        other => anyhow::bail!("frame decoded as {other:?}, expected a mono greyscale frame"),
    };

    let width_bytes = width as usize / 8;
    let width_px = width as usize;
    let mut packed = Vec::with_capacity(width_bytes * info.height as usize);
    for row in buf[..info.buffer_size()].chunks_exact(width_px * samples_per_px) {
        for byte_index in 0..width_bytes {
            let mut byte = 0u8;
            for bit in 0..8 {
                if row[(byte_index * 8 + bit) * samples_per_px] < DARK_BELOW {
                    byte |= 0x80 >> bit;
                }
            }
            packed.push(byte);
        }
    }

    // Paper ends where the content does.
    let mut end = packed.len();
    while end >= width_bytes && packed[end - width_bytes..end].iter().all(|&b| b == 0) {
        end -= width_bytes;
    }
    packed.truncate(end);
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a 1-bit greyscale PNG the way the frame encoder does, from rows
    /// of `0` (black) and `255` (white) samples.
    fn mono_png(width: u32, rows: &[Vec<u8>]) -> Vec<u8> {
        let mut packed = Vec::new();
        for row in rows {
            assert_eq!(row.len(), width as usize);
            for chunk in row.chunks(8) {
                let mut byte = 0u8;
                for (bit, &sample) in chunk.iter().enumerate() {
                    if sample >= 128 {
                        byte |= 0x80 >> bit; // PNG grey: 1 = white
                    }
                }
                packed.push(byte);
            }
        }
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, rows.len() as u32);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::One);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&packed).unwrap();
        }
        out
    }

    #[test]
    fn dark_is_one_msb_first() {
        // Leftmost pixel black, rest white -> printer byte 0x80.
        let mut row = vec![255u8; 16];
        row[0] = 0;
        let png = mono_png(16, &[row]);
        assert_eq!(raster_from_png(&png, 16).unwrap(), vec![0x80, 0x00]);
    }

    #[test]
    fn trailing_blank_rows_are_trimmed() {
        let ink = vec![0u8; 16];
        let paper = vec![255u8; 16];
        let png = mono_png(16, &[ink, paper.clone(), paper]);
        assert_eq!(raster_from_png(&png, 16).unwrap(), vec![0xFF, 0xFF]);
    }

    #[test]
    fn an_all_white_frame_is_nothing_to_print() {
        let paper = vec![255u8; 16];
        let png = mono_png(16, &[paper.clone(), paper]);
        assert!(raster_from_png(&png, 16).unwrap().is_empty());
    }

    #[test]
    fn a_width_mismatch_is_an_error() {
        let png = mono_png(16, &[vec![0u8; 16]]);
        assert!(raster_from_png(&png, 384).is_err());
    }
}
