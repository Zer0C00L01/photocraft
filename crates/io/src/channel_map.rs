//! Alpha / spot channel options <-> PSD `DisplayInfo` image resources (1077, and the older 1007).
//!
//! Layout (Adobe PSD spec, "DisplayInfo", and a Photoshop 27 layered TIFF): resource 1077
//! starts with a 4-byte version (1), then one record per extra channel: colour space (u16),
//! four u16 colour components, opacity (u16, 0..100) and kind (u8: 0 = colour indicates
//! selected areas, 1 = masked ("protected") areas, 2 = spot channel). Photoshop writes that
//! 13-byte record with no pad. Older files pad each record to 14 bytes; both are read.
//! Resource 1007 has the same records without the version and without the spot kind.

use photocraft_color::{Color, ColorMode};
use photocraft_doc::{AlphaChannel, ColorIndicates};

pub(crate) const DISPLAY_INFO: u16 = 1077;
pub(crate) const DISPLAY_INFO_OLD: u16 = 1007;
/// Quick Mask information: channel id (u16) and an "initially empty" flag (u8).
pub(crate) const QUICK_MASK_INFO: u16 = 1022;
/// Alpha channel identifiers (Photoshop 6): one big-endian u32 per extra channel, no count prefix.
pub(crate) const ALPHA_IDENTIFIERS: u16 = 1053;

const SPACE_RGB: u16 = 0;
const SPACE_CMYK: u16 = 2;
const SPACE_LAB: u16 = 7;
const SPACE_GRAY: u16 = 8;

/// Resource 1053: channel ids `1..=count`, one big-endian u32 each, with no length prefix.
/// Photoshop, ag-psd and psd-tools read u32s until the resource ends.
pub(crate) fn alpha_identifiers(count: usize) -> Vec<u8> {
    alpha_identifiers_from(1, count)
}

/// Resource 1053 starting at `first` (a layered TIFF uses the sample's index in the IFD:
/// 3 for the spot after RGB, not 1).
pub(crate) fn alpha_identifiers_from(first: u32, count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count.saturating_mul(4));
    for i in 0..count {
        let id = first.saturating_add(u32::try_from(i).unwrap_or(u32::MAX));
        out.extend_from_slice(&id.to_be_bytes());
    }
    out
}

/// Resource 1077 for `channels` (in file order).
pub(crate) fn display_info(channels: &[&AlphaChannel]) -> Vec<u8> {
    let mut out = 1u32.to_be_bytes().to_vec();
    for c in channels {
        let (color, opacity, kind) = match c.spot {
            Some((ink, solidity)) => (ink, solidity, 2u8),
            None => (c.color, c.opacity, if c.indicates == ColorIndicates::SelectedAreas { 0 } else { 1 }),
        };
        let rgb = color.to_rgb();
        out.extend_from_slice(&SPACE_RGB.to_be_bytes());
        for v in [rgb[0], rgb[1], rgb[2], 0.0] {
            out.extend_from_slice(&((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_be_bytes());
        }
        out.extend_from_slice(&((opacity.clamp(0.0, 1.0) * 100.0).round() as u16).to_be_bytes());
        out.push(kind);
    }
    out
}

/// 13 when every record is Photoshop's unpadded form, otherwise the 14-byte padded form.
///
/// Lengths divisible by both (14, 28, … unpadded records, or 13, 26, … padded ones) take
/// the unpadded stride for resource 1077, which is what Photoshop 27 writes, and the padded
/// stride for the older 1007 resource, whose record includes the pad byte.
fn display_stride(body: &[u8], versioned: bool) -> usize {
    let unpadded = !body.is_empty() && body.len().is_multiple_of(13);
    let padded = !body.is_empty() && body.len().is_multiple_of(14);
    if unpadded && (!padded || versioned) { 13 } else { 14 }
}

/// Applies a DisplayInfo resource (`versioned` = 1077) to the imported channels in order.
pub(crate) fn apply_display_info(data: &[u8], versioned: bool, channels: &mut [AlphaChannel]) {
    let body = if versioned { data.get(4..).unwrap_or(&[]) } else { data };
    let stride = display_stride(body, versioned);
    for (c, rec) in channels.iter_mut().zip(body.chunks(stride)) {
        let Some(rec) = rec.get(..13) else { break };
        let u = |i: usize| u16::from_be_bytes([rec[i], rec[i + 1]]);
        let comps = [u(2), u(4), u(6), u(8)];
        let color = decode_color(u(0), comps);
        let opacity = f32::from(u(10).min(100)) / 100.0;
        match rec[12] {
            2 => c.spot = Some((color, opacity)),
            k => {
                c.color = color;
                c.opacity = opacity;
                c.indicates = if k == 0 { ColorIndicates::SelectedAreas } else { ColorIndicates::MaskedAreas };
            }
        }
    }
}

pub(crate) fn decode_color(space: u16, c: [u16; 4]) -> Color {
    let n = |v: u16| f32::from(v) / 65535.0;
    match space {
        // Photoshop colour structures store CMYK inverted (65535 = no ink).
        SPACE_CMYK => {
            let cmyk = Color { mode: ColorMode::Cmyk, c: [1.0 - n(c[0]), 1.0 - n(c[1]), 1.0 - n(c[2]), 1.0 - n(c[3])], alpha: 1.0 };
            let rgb = cmyk.to_rgb();
            Color::rgb(rgb[0], rgb[1], rgb[2])
        }
        SPACE_LAB => {
            let l = f32::from(c[0].min(10000)) / 10000.0;
            let a = f32::from(c[1] as i16) / 100.0;
            let b = f32::from(c[2] as i16) / 100.0;
            let lab = Color { mode: ColorMode::Lab, c: [l, (a + 128.0) / 255.0, (b + 128.0) / 255.0, 0.0], alpha: 1.0 };
            let rgb = lab.to_rgb();
            Color::rgb(rgb[0], rgb[1], rgb[2])
        }
        SPACE_GRAY => {
            let v = 1.0 - f32::from(c[0].min(10000)) / 10000.0;
            Color::rgb(v, v, v)
        }
        _ => Color::rgb(n(c[0]), n(c[1]), n(c[2])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::PixelFormat;
    use photocraft_raster::Surface;

    #[test]
    fn display_info_roundtrip() {
        let mut a = AlphaChannel::new("A", Surface::new(PixelFormat::GRAY8));
        a.color = Color::rgb(0.0, 1.0, 0.0);
        a.opacity = 0.3;
        a.indicates = ColorIndicates::SelectedAreas;
        let mut s = AlphaChannel::new("S", Surface::new(PixelFormat::GRAY8));
        s.spot = Some((Color::rgb(1.0, 0.0, 1.0), 0.8));
        let bytes = display_info(&[&a, &s]);
        assert_eq!(bytes.len(), 4 + 26);
        let mut back = vec![AlphaChannel::new("A", Surface::new(PixelFormat::GRAY8)), AlphaChannel::new("S", Surface::new(PixelFormat::GRAY8))];
        apply_display_info(&bytes, true, &mut back);
        assert_eq!(back[0].color, a.color);
        assert!((back[0].opacity - 0.3).abs() < 1e-6);
        assert_eq!(back[0].indicates, ColorIndicates::SelectedAreas);
        assert_eq!(back[1].spot, s.spot);
        assert_eq!(alpha_identifiers(2), [0, 0, 0, 1, 0, 0, 0, 2]);
        assert!(alpha_identifiers(0).is_empty());
        assert_eq!(alpha_identifiers_from(3, 1), [0, 0, 0, 3]);
        // Photoshop 27 writes a 13-byte record (no pad): red spot, solidity 60.
        let ps = [0, 0, 0, 1, 0, 0, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0x3c, 2];
        let mut spot = vec![AlphaChannel::new("S", Surface::new(PixelFormat::GRAY8))];
        apply_display_info(&ps, true, &mut spot);
        assert_eq!(spot[0].spot.map(|(c, s)| (c.to_rgb(), (s * 100.0).round() as u16)), Some(([1.0, 0.0, 0.0], 60)));
        // Truncated data is ignored rather than misread.
        let mut c = vec![AlphaChannel::new("x", Surface::new(PixelFormat::GRAY8))];
        apply_display_info(&bytes[..10], true, &mut c);
        assert_eq!(c[0].opacity, 0.5);

        // 14 unpadded records are a multiple of 14 as well (14×13 = 13×14). Resource 1077
        // still reads them as 13-byte records; the last spot's solidity must survive.
        let mut many = Vec::new();
        for i in 0..14 {
            let mut ch = AlphaChannel::new(format!("{i}"), Surface::new(PixelFormat::GRAY8));
            ch.spot = Some((Color::rgb(1.0, 0.0, 0.0), (i as f32) / 14.0));
            many.push(ch);
        }
        let refs: Vec<&AlphaChannel> = many.iter().collect();
        let packed = display_info(&refs);
        assert_eq!(packed.len(), 4 + 14 * 13);
        let mut got: Vec<AlphaChannel> = (0..14).map(|i| AlphaChannel::new(format!("{i}"), Surface::new(PixelFormat::GRAY8))).collect();
        apply_display_info(&packed, true, &mut got);
        assert_eq!(got[13].spot.map(|(_, s)| (s * 100.0).round() as u16), many[13].spot.map(|(_, s)| (s * 100.0).round() as u16));
    }

    #[test]
    fn decodes_other_spaces() {
        let g = decode_color(SPACE_GRAY, [10000, 0, 0, 0]);
        assert_eq!(g.to_rgb(), [0.0; 3]);
        let lab = decode_color(SPACE_LAB, [10000, 0, 0, 0]).to_rgb();
        assert!(lab.iter().all(|v| *v > 0.95), "{lab:?}");
    }
}
