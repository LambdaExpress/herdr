//! SIXEL host output for terminals that render SIXEL but no Kitty graphics.
//!
//! Windows Terminal reports no cell pixel geometry and only understands SIXEL,
//! so a client hosted by it reports the fixed VT340 virtual cell grid instead
//! and encodes the graphics scene itself.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use super::{image_signature, ClippedPlacement, HostCellSize, HostPlacement, ImageSignature};
use crate::ghostty::KittyImageFormat;

/// Virtual cell Windows Terminal uses when scaling SIXEL rasters onto its grid.
pub(crate) const SIXEL_CELL_SIZE: HostCellSize = HostCellSize {
    width_px: 10,
    height_px: 20,
};

/// Upper bound for one host write.
///
/// ConPTY delays the whole write while it drains a large SIXEL payload, which
/// stalls scrolling in panes that show several images.
const SIXEL_WRITE_CHUNK_BYTES: usize = 16 * 1024;

/// Graphics protocol the hosting terminal accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum HostGraphicsProtocol {
    /// Do not emit terminal graphics payloads.
    #[default]
    Disabled,
    /// Emit Kitty graphics protocol payloads.
    Kitty,
    /// Emit SIXEL payloads.
    Sixel,
}

impl HostGraphicsProtocol {
    pub(crate) fn is_sixel(self) -> bool {
        self == Self::Sixel
    }

    /// Cell geometry to request from the endpoint for this protocol.
    ///
    /// SIXEL hosts are steered onto the fixed virtual grid because they cannot
    /// report real geometry.
    pub(crate) fn cell_size(self, reported: HostCellSize) -> HostCellSize {
        if self.is_sixel() {
            SIXEL_CELL_SIZE
        } else {
            reported
        }
    }
}

pub(crate) fn host_graphics_protocol(enabled: bool) -> HostGraphicsProtocol {
    host_graphics_protocol_for_env(enabled, |name| std::env::var_os(name))
}

fn host_graphics_protocol_for_env(
    enabled: bool,
    mut env: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> HostGraphicsProtocol {
    if !enabled {
        return HostGraphicsProtocol::Disabled;
    }

    let term_program = env("TERM_PROGRAM")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_default();
    let known_kitty_host = env("KITTY_WINDOW_ID").is_some()
        || env("GHOSTTY_RESOURCES_DIR").is_some()
        || env("WEZTERM_PANE").is_some()
        || matches!(
            term_program.to_ascii_lowercase().as_str(),
            "kitty" | "ghostty" | "wezterm"
        );
    if known_kitty_host {
        HostGraphicsProtocol::Kitty
    } else if env("WT_SESSION").is_some() {
        HostGraphicsProtocol::Sixel
    } else {
        HostGraphicsProtocol::Kitty
    }
}

/// Writes host graphics bytes, bounding each write so ConPTY keeps draining.
pub(crate) fn write_host_output(
    writer: &mut impl Write,
    protocol: HostGraphicsProtocol,
    bytes: &[u8],
) -> io::Result<()> {
    if protocol.is_sixel() {
        for chunk in bytes.chunks(SIXEL_WRITE_CHUNK_BYTES) {
            writer.write_all(chunk)?;
        }
        Ok(())
    } else {
        writer.write_all(bytes)
    }
}

/// Decoded RGBA pixels of one asset.
#[derive(Debug)]
struct DecodedRgba<'a> {
    width: u32,
    height: u32,
    data: Cow<'a, [u8]>,
}

/// What one encoded SIXEL payload depends on: the source pixels, the crop, and
/// the placement's pixel geometry. Screen position is deliberately excluded.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct SixelPayloadSignature {
    image: ImageSignature,
    cols: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x_offset: u32,
    y_offset: u32,
}

/// Per-client SIXEL state: decoded source pixels plus encoded payloads.
#[derive(Debug, Default)]
pub(crate) struct SixelGraphicsCache {
    images: HashMap<ImageSignature, DecodedRgba<'static>>,
    payloads: HashMap<SixelPayloadSignature, Vec<u8>>,
}

impl SixelGraphicsCache {
    pub(crate) fn clear(&mut self) {
        self.images.clear();
        self.payloads.clear();
    }
}

/// Encodes every visible placement as positioned SIXEL payloads.
///
/// `placements` is consumed for its pixel data so a decoded image can outlive
/// the frame it arrived in. `safe_bottom` is the last host row an image may
/// paint; leaving the host terminal's bottom row unused keeps it from scrolling.
pub(super) fn encode_sixel_placements(
    placements: &mut [HostPlacement],
    safe_bottom: Option<u16>,
    cache: &mut SixelGraphicsCache,
) -> Vec<u8> {
    // A placeholder row carries a crop of the same full image. Take and decode
    // that image once, before sorting can put a data-free row ahead of its owner.
    for placement in placements.iter_mut() {
        if placement.placement.data.is_empty() {
            continue;
        }
        let signature = image_signature(
            placement,
            super::kitty_format_code(placement.placement.format),
        );
        if let Entry::Vacant(entry) = cache.images.entry(signature) {
            if let Some(decoded) = decode_placement_rgba(placement) {
                entry.insert(decoded);
            }
        }
    }

    let mut bytes = Vec::new();
    let mut order = (0..placements.len()).collect::<Vec<_>>();
    // Paint taller placements first so a shorter image is never covered by the
    // trailing band of a taller one.
    order.sort_by_key(|index| {
        std::cmp::Reverse((
            placements[*index].area.height,
            placements[*index].area.width,
        ))
    });
    let mut live = HashSet::new();
    for index in order {
        let placement = &mut placements[index];
        if let Some(signature) =
            append_cached_sixel_placement(&mut bytes, placement, safe_bottom, cache)
        {
            live.insert(signature);
        }
    }
    cache
        .payloads
        .retain(|signature, _| live.contains(signature));
    cache
        .images
        .retain(|image, _| live.iter().any(|signature| signature.image == *image));
    bytes
}

/// Clips a placement for SIXEL output, honouring the host's safe bottom row.
fn clipped_sixel_placement(
    placement: &HostPlacement,
    safe_bottom: Option<u16>,
) -> Option<(ClippedPlacement, u32)> {
    super::clipped_placement_with_bottom_limit(placement, safe_bottom)
}

fn append_cached_sixel_placement(
    encoded: &mut Vec<u8>,
    placement: &mut HostPlacement,
    safe_bottom: Option<u16>,
    cache: &mut SixelGraphicsCache,
) -> Option<SixelPayloadSignature> {
    let (clipped, format_code) = clipped_sixel_placement(placement, safe_bottom)?;
    let signature = SixelPayloadSignature {
        image: image_signature(placement, format_code),
        cols: clipped.cols,
        rows: clipped.rows,
        source_x: clipped.source_x,
        source_y: clipped.source_y,
        source_width: clipped.source_width,
        source_height: clipped.source_height,
        x_offset: clipped.x_offset,
        y_offset: clipped.y_offset,
    };
    // Synthetic placeholder ids include screen coordinates. Encoded pixels only
    // depend on the source and crop, so moving them must not invalidate the cache.
    let payload = match cache.payloads.entry(signature) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let source = match cache.images.entry(signature.image) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(decode_placement_rgba(placement)?),
            };
            entry.insert(encode_sixel_payload(placement, clipped, source)?)
        }
    };
    encoded.reserve(payload.len() + 24);
    append_sixel_cursor(encoded, clipped);
    encoded.extend_from_slice(payload);
    Some(signature)
}

fn append_sixel_cursor(encoded: &mut Vec<u8>, clipped: ClippedPlacement) {
    let _ = write!(encoded, "\x1b[{};{}H", clipped.y + 1, clipped.x + 1);
}

fn encode_sixel_payload(
    placement: &HostPlacement,
    clipped: ClippedPlacement,
    source: &DecodedRgba<'_>,
) -> Option<Vec<u8>> {
    let source_right = clipped.source_x.checked_add(clipped.source_width)?;
    let source_bottom = clipped.source_y.checked_add(clipped.source_height)?;
    if source_right > source.width || source_bottom > source.height {
        return None;
    }

    let raw_width = clipped.cols.checked_mul(placement.cell_size.width_px)?;
    let raw_height = clipped.rows.checked_mul(placement.cell_size.height_px)?;
    if raw_width == 0 || raw_height == 0 {
        return None;
    }
    // The encoder pads the final partial sixel band with transparent pixels.
    // Rounding down to a multiple of six would leave an unpainted horizontal
    // gap in every terminal row.
    let target_width = raw_width;
    let target_height = raw_height;
    let x_offset = clipped.x_offset.min(target_width);
    let y_offset = clipped.y_offset.min(target_height);
    let rgba = crop_scale_rgba(
        source,
        clipped,
        target_width,
        target_height,
        x_offset,
        y_offset,
    )?;
    let sixel = icy_sixel::sixel_encode(
        &rgba,
        target_width as usize,
        target_height as usize,
        &icy_sixel::EncodeOptions::default(),
    )
    .ok()?;

    Some(sixel.into_bytes())
}

/// Takes the placement's pixel buffer so the decoded image can be cached.
fn decode_placement_rgba(placement: &mut HostPlacement) -> Option<DecodedRgba<'static>> {
    let width = placement.placement.image_width;
    let height = placement.placement.image_height;
    let pixel_count = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    match placement.placement.format {
        KittyImageFormat::Rgba => {
            let expected = pixel_count.checked_mul(4)?;
            (placement.placement.data.len() == expected).then(|| DecodedRgba {
                width,
                height,
                data: Cow::Owned(std::mem::take(&mut placement.placement.data)),
            })
        }
        KittyImageFormat::Rgb => {
            let expected = pixel_count.checked_mul(3)?;
            if placement.placement.data.len() != expected {
                return None;
            }
            let rgb = std::mem::take(&mut placement.placement.data);
            let mut rgba = Vec::with_capacity(pixel_count.checked_mul(4)?);
            for pixel in rgb.chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            Some(DecodedRgba {
                width,
                height,
                data: Cow::Owned(rgba),
            })
        }
        KittyImageFormat::Png => {
            let png = std::mem::take(&mut placement.placement.data);
            let decoded = crate::ghostty::decode_png_rgba(&png)?;
            if decoded.width != width || decoded.height != height {
                return None;
            }
            Some(DecodedRgba {
                width,
                height,
                data: Cow::Owned(decoded.data),
            })
        }
    }
}

/// Crops and scales the source region into a `target_width × target_height`
/// RGBA buffer, offset by the placement's sub-cell padding.
///
/// The downscale path integrates each destination pixel's source footprint and
/// weights color by alpha, so thin strokes and transparent edges survive.
fn crop_scale_rgba(
    source: &DecodedRgba<'_>,
    clipped: ClippedPlacement,
    target_width: u32,
    target_height: u32,
    x_offset: u32,
    y_offset: u32,
) -> Option<Vec<u8>> {
    let target_pixels = usize::try_from(target_width)
        .ok()?
        .checked_mul(usize::try_from(target_height).ok()?)?;
    let mut target = vec![0; target_pixels.checked_mul(4)?];
    let content_width = target_width.saturating_sub(x_offset);
    let content_height = target_height.saturating_sub(y_offset);
    if content_width == 0 || content_height == 0 {
        return Some(target);
    }

    let source_width = usize::try_from(source.width).ok()?;
    let downscale = clipped.source_width > content_width || clipped.source_height > content_height;
    let sample_area = f64::from(clipped.source_width) * f64::from(clipped.source_height);
    let mut x_samples = Vec::new();
    let mut x_ranges = Vec::new();
    if downscale {
        // Horizontal coverage is shared by every row; compute it once per payload.
        x_samples.reserve(clipped.source_width as usize + content_width as usize);
        x_ranges.reserve(content_width as usize);
        for dest_x in 0..content_width {
            let begin = u64::from(dest_x) * u64::from(clipped.source_width);
            let end = begin + u64::from(clipped.source_width);
            let start = x_samples.len();
            for sx in begin / u64::from(content_width)..end.div_ceil(u64::from(content_width)) {
                let weight = (end.min((sx + 1) * u64::from(content_width))
                    - begin.max(sx * u64::from(content_width))) as f64;
                x_samples.push(((clipped.source_x as usize + sx as usize) * 4, weight));
            }
            x_ranges.push(start..x_samples.len());
        }
    }
    for dest_y in 0..content_height {
        let y_begin = u64::from(dest_y) * u64::from(clipped.source_height);
        let y_end = y_begin + u64::from(clipped.source_height);
        let source_y = clipped.source_y + (y_begin / u64::from(content_height)) as u32;
        for dest_x in 0..content_width {
            let target_index = usize::try_from(dest_y + y_offset)
                .ok()?
                .checked_mul(usize::try_from(target_width).ok()?)?
                .checked_add(usize::try_from(dest_x + x_offset).ok()?)?
                .checked_mul(4)?;
            if downscale {
                // Integrate the source pixel footprint instead of dropping thin strokes.
                // Integer boundaries retain fractional coverage at non-integral ratios.
                let mut sum = [0.0; 4];
                for sy in
                    y_begin / u64::from(content_height)..y_end.div_ceil(u64::from(content_height))
                {
                    let y_weight = (y_end.min((sy + 1) * u64::from(content_height))
                        - y_begin.max(sy * u64::from(content_height)))
                        as f64;
                    let row_start = (clipped.source_y as usize + sy as usize) * source_width * 4;
                    for &(offset, x_weight) in &x_samples[x_ranges[dest_x as usize].clone()] {
                        let index = row_start + offset;
                        let pixel = &source.data[index..index + 4];
                        let alpha_weight = x_weight * y_weight * f64::from(pixel[3]);
                        for channel in 0..3 {
                            sum[channel] += f64::from(pixel[channel]) * alpha_weight;
                        }
                        sum[3] += alpha_weight;
                    }
                }
                if sum[3] > 0.0 {
                    for channel in 0..3 {
                        target[target_index + channel] = (sum[channel] / sum[3]).round() as u8;
                    }
                    target[target_index + 3] = (sum[3] / sample_area).round() as u8;
                }
                continue;
            }
            let source_x = clipped.source_x
                + (u64::from(dest_x) * u64::from(clipped.source_width) / u64::from(content_width))
                    as u32;
            let source_index = usize::try_from(source_y)
                .ok()?
                .checked_mul(source_width)?
                .checked_add(usize::try_from(source_x).ok()?)?
                .checked_mul(4)?;
            target[target_index..target_index + 4]
                .copy_from_slice(&source.data[source_index..source_index + 4]);
        }
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghostty::KittyPlacementRenderInfo;
    use crate::layout::PaneId;
    use ratatui::layout::Rect;

    fn test_placement(viewport_col: i32, viewport_row: i32) -> HostPlacement {
        HostPlacement {
            pane_id: PaneId::from_raw(1),
            host_image_id: None,
            area: Rect::new(0, 0, 20, 10),
            cell_size: SIXEL_CELL_SIZE,
            source_key: super::super::HostSourceKey::ClientSurface {
                scope: "endpoint-a:boot-1".to_owned(),
                source: crate::protocol::SurfaceGraphicsSource::PaneLayer {
                    pane_id: "w1:p1".to_owned(),
                    layer_id: "primary".to_owned(),
                },
            },
            scrollback_offset: 0,
            placement: crate::ghostty::KittyImagePlacement {
                image_id: 7,
                placement_id: 3,
                z: 0,
                x_offset: 0,
                y_offset: 0,
                image_width: 30,
                image_height: 30,
                format: KittyImageFormat::Rgba,
                data_len: 30 * 30 * 4,
                data_fingerprint: 42,
                data: vec![255; 30 * 30 * 4],
                render: KittyPlacementRenderInfo {
                    pixel_width: 0,
                    pixel_height: 0,
                    grid_cols: 3,
                    grid_rows: 3,
                    viewport_col,
                    viewport_row,
                    source_x: 0,
                    source_y: 0,
                    source_width: 0,
                    source_height: 0,
                },
            },
        }
    }

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        writes: Vec<usize>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes.push(buf.len());
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn windows_terminal_selects_sixel_host_output() {
        let env = |name: &str| match name {
            "WT_SESSION" => Some(std::ffi::OsString::from("session")),
            _ => None,
        };

        assert_eq!(
            host_graphics_protocol_for_env(true, env),
            HostGraphicsProtocol::Sixel
        );
        assert_eq!(
            host_graphics_protocol_for_env(false, |_| None),
            HostGraphicsProtocol::Disabled
        );
    }

    #[test]
    fn kitty_hosts_keep_the_kitty_protocol() {
        for (name, value) in [
            ("KITTY_WINDOW_ID", "1"),
            ("GHOSTTY_RESOURCES_DIR", "/ghostty"),
            ("WEZTERM_PANE", "0"),
            ("TERM_PROGRAM", "WezTerm"),
        ] {
            let env =
                |requested: &str| (requested == name).then(|| std::ffi::OsString::from(value));
            assert_eq!(
                host_graphics_protocol_for_env(true, env),
                HostGraphicsProtocol::Kitty,
                "{name} should stay on Kitty"
            );
        }
    }

    #[test]
    fn sixel_hosts_report_the_virtual_cell_grid() {
        let protocol = HostGraphicsProtocol::Sixel;
        let reported = HostCellSize {
            width_px: 8,
            height_px: 16,
        };

        assert_eq!(protocol.cell_size(reported), SIXEL_CELL_SIZE);
        assert_eq!(HostGraphicsProtocol::Kitty.cell_size(reported), reported);
    }

    #[test]
    fn sixel_output_uses_bounded_terminal_writes() {
        let bytes = vec![b'x'; SIXEL_WRITE_CHUNK_BYTES * 2 + 7];
        let mut writer = RecordingWriter::default();

        write_host_output(&mut writer, HostGraphicsProtocol::Sixel, &bytes).unwrap();

        assert_eq!(writer.bytes, bytes);
        assert_eq!(
            writer.writes,
            vec![SIXEL_WRITE_CHUNK_BYTES, SIXEL_WRITE_CHUNK_BYTES, 7]
        );
    }

    #[test]
    fn sixel_downsampling_preserves_thin_strokes() {
        let source = DecodedRgba {
            width: 8,
            height: 8,
            data: Cow::Owned(
                (0..64)
                    .flat_map(|i| {
                        let value = if (i % 8 + i / 8) % 2 == 0 { 0 } else { 255 };
                        [value, value, value, 255]
                    })
                    .collect(),
            ),
        };
        let mut placement = test_placement(0, 0);
        placement.cell_size = HostCellSize {
            width_px: 10,
            height_px: 10,
        };
        let (mut clipped, _) =
            clipped_sixel_placement(&placement, None).expect("clipped placement");
        clipped.source_width = 8;
        clipped.source_height = 8;

        let rgba = crop_scale_rgba(&source, clipped, 2, 2, 0, 0).unwrap();

        assert_eq!(rgba, [128, 128, 128, 255].repeat(4));
    }

    #[test]
    fn sixel_placement_encodes_cursor_and_visible_pixel_geometry() {
        let mut placement = test_placement(2, 1);
        let mut cache = SixelGraphicsCache::default();

        let encoded =
            encode_sixel_placements(std::slice::from_mut(&mut placement), None, &mut cache);

        let text = String::from_utf8_lossy(&encoded);
        assert!(text.starts_with("\u{1b}[2;3H\u{1b}P"), "{text:?}");
        assert!(text.contains("q"), "{text:?}");
    }

    #[test]
    fn sixel_placement_leaves_the_host_terminal_bottom_row_unused() {
        // A host with rows 0..=9 passes its last usable row as `safe_bottom`.
        // The image starts at row 7, so row 9 must stay unpainted.
        let place_at_bottom = |safe_bottom: Option<u16>| {
            let mut placement = test_placement(0, 0);
            placement.area = Rect::new(0, 7, 20, 10);
            let mut cache = SixelGraphicsCache::default();
            let encoded = encode_sixel_placements(
                std::slice::from_mut(&mut placement),
                safe_bottom,
                &mut cache,
            );
            let (clipped, _) = clipped_sixel_placement(&placement, safe_bottom).expect("clipped");
            (clipped.rows, clipped.y, encoded)
        };

        let (unclipped_rows, _, _) = place_at_bottom(None);
        let (clipped_rows, cursor_row, encoded) = place_at_bottom(Some(9));

        assert_eq!(unclipped_rows, 3);
        assert_eq!(clipped_rows, 2, "the bottom host row stays unused");
        assert_eq!(cursor_row, 7);
        assert!(String::from_utf8_lossy(&encoded).starts_with("\u{1b}[8;1H"));
    }

    #[test]
    fn sixel_cache_reuses_a_payload_when_only_the_position_changes() {
        let mut placement = test_placement(0, 0);
        let mut cache = SixelGraphicsCache::default();
        let first = encode_sixel_placements(std::slice::from_mut(&mut placement), None, &mut cache);
        assert_eq!(cache.payloads.len(), 1);

        // A scrolled row reuses the same crop under a new screen position.
        let mut moved = test_placement(0, 0);
        moved.area = Rect::new(0, 1, 20, 10);
        let second = encode_sixel_placements(std::slice::from_mut(&mut moved), None, &mut cache);

        assert_eq!(cache.payloads.len(), 1);
        assert_ne!(first, second, "cursor row must follow the placement");
        assert_eq!(
            first.strip_prefix(b"\x1b[1;1H"),
            second.strip_prefix(b"\x1b[2;1H")
        );
    }

    #[test]
    fn sixel_cache_reencodes_changed_image_content() {
        let mut placement = test_placement(0, 0);
        let mut cache = SixelGraphicsCache::default();
        let first = encode_sixel_placements(std::slice::from_mut(&mut placement), None, &mut cache);

        let mut changed = test_placement(0, 0);
        changed.placement.data_fingerprint = 43;
        changed.placement.data = vec![0; 30 * 30 * 4];
        let second = encode_sixel_placements(std::slice::from_mut(&mut changed), None, &mut cache);

        assert_ne!(first, second);
        assert_eq!(cache.payloads.len(), 1, "stale payloads are pruned");
    }
}
