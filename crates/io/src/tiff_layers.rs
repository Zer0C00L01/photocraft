//! Layered TIFF: Photoshop layer data carried in the TIFF tags 37724 (`ImageSourceData`) and
//! 34377 (image resources), see [`photocraft_psd::tiff`].
//!
//! Import rebuilds a PSD model from the TIFF's pixels plus those two tags and hands it to the
//! PSD importer, so everything the PSD path understands (groups, masks, effects, type, shapes,
//! smart objects, adjustment and fill layers, alpha channels, paths, guides) opens from a TIFF
//! as well. Export runs the PSD exporter and stores its layer section and resources in the
//! tags, in the byte order the TIFF encoder writes, next to the flattened composite the TIFF
//! itself carries.

use std::sync::Arc;

use photocraft_codecs::{self as codecs, ChannelLayout, Format, Image, SampleType as CSample};
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{AlphaChannel, Document};
use photocraft_geom::Rect;
use photocraft_psd::resources::ids;
use photocraft_psd::tiff::{ByteOrder, ImageSourceData, resources_from_bytes, resources_to_bytes};
use photocraft_psd::{ColorMode as PsdMode, Compression, Header, ImageData, ImageResource, ResolutionInfo};
use photocraft_raster::Surface;

use crate::flat::{csample, encode_image, image_to_document, layout_for, single_layer, try_buffer};
use crate::psd_export::{PsdExportOptions, document_to_psd_with};
use crate::psd_import::psd_to_document;
use crate::{ExportOptions, ExportResult, ImportResult, IoError};

/// `true` when saving `doc` as TIFF writes layer data: anything beyond a single Background
/// layer (one plain, transparency-locked raster layer), which a flat TIFF already represents
/// exactly. A lone unlocked layer keeps its layer data, as in Photoshop, so it reopens as that
/// layer rather than as a Background.
pub fn would_write_layers(doc: &Document) -> bool {
    !(single_layer(doc).is_some() && doc.layers.first().is_some_and(|l| l.locks.transparency))
}

/// Documents a layered TIFF can hold: the TIFF composite must be in the document's model so
/// that the layer channels inside the tag match it (Lab TIFFs are not decoded or encoded by
/// `photocraft-codecs`), and there must be layers to keep (a Multichannel document has none:
/// its channels are the image).
fn layered_mode(doc: &Document) -> bool {
    matches!(doc.pixel_format().mode, ColorMode::Grayscale | ColorMode::Rgb | ColorMode::Cmyk) && doc.mode != ColorMode::Multichannel && !doc.layers.is_empty()
}

/// Replaces (or inserts) resource `id`; removes it when `data` is `None`.
fn set_resource(resources: &mut Vec<ImageResource>, id: u16, data: Option<Vec<u8>>) {
    resources.retain(|r| r.id != id);
    if let Some(d) = data {
        resources.push(ImageResource::new(id, d));
    }
}

/// Opens a TIFF whose `img` carries Photoshop layer data in `layers` (its 37724 tag).
pub(crate) fn import_layered(name: &str, img: &Image, layers: &[u8]) -> Result<ImportResult, IoError> {
    let mut warnings = Vec::new();
    let (isd, w) = match ImageSourceData::from_bytes(layers) {
        Ok(v) => v,
        Err(e) => {
            let mut r = image_to_document(name, img)?;
            r.warnings.push(format!("the PSD layer data could not be read ({e}); opened flattened"));
            return Ok(r);
        }
    };
    warnings.extend(w.into_iter().map(|w| format!("layer data: {w}")));
    let (mut resources, w) = match img.meta.photoshop_resources.as_deref().filter(|r| !r.is_empty()).map(resources_from_bytes) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            warnings.push(format!("the PSD image resources could not be read: {e}"));
            (Vec::new(), Vec::new())
        }
        None => (Vec::new(), Vec::new()),
    };
    warnings.extend(w.into_iter().map(|w| format!("image resources: {w}")));
    // The profile, resolution, XMP and EXIF are the TIFF's own tags; they win over any copy
    // in the resources.
    set_resource(&mut resources, ids::ICC_PROFILE, img.icc.clone());
    if let Some((x, y)) = img.meta.dpi.filter(|d| d.0 > 0.0) {
        // Both axes, so the PSD import notes a vertical resolution the document can't keep.
        let ri = ResolutionInfo { v_res_fixed: ResolutionInfo::from_dpi(f64::from(y)).v_res_fixed, ..ResolutionInfo::from_dpi(f64::from(x)) };
        set_resource(&mut resources, ids::RESOLUTION_INFO, Some(ri.to_bytes()));
    }
    if let Some(xmp) = &img.meta.xmp {
        set_resource(&mut resources, ids::XMP, Some(xmp.as_bytes().to_vec()));
    }
    if let Some(exif) = &img.meta.exif {
        set_resource(&mut resources, ids::EXIF, Some(exif.clone()));
    }

    // The TIFF's pixels become the PSD's merged image: planar, big-endian, CMYK inverted.
    let img = if img.sample_type() == CSample::F16 {
        warnings.push("16-bit float samples are stored as 32-bit float".to_string());
        std::borrow::Cow::Owned(img.convert(img.layout(), CSample::F32))
    } else {
        std::borrow::Cow::Borrowed(img)
    };
    let (mode, color_channels) = match img.layout() {
        ChannelLayout::Gray | ChannelLayout::GrayA => (PsdMode::Grayscale, 1),
        ChannelLayout::Rgb | ChannelLayout::Rgba => (PsdMode::Rgb, 3),
        ChannelLayout::Cmyk | ChannelLayout::CmykA => (PsdMode::Cmyk, 4),
    };
    let depth = match img.sample_type() {
        CSample::U8 => 8,
        CSample::U16 => 16,
        CSample::F16 | CSample::F32 => 32,
    };
    let (w, h) = img.dimensions();
    let mut data = planar_big_endian(&img, color_channels, mode == PsdMode::Cmyk)?;
    let bps = img.sample_type().bytes();
    let plane_len = (w as usize).saturating_mul(h as usize).saturating_mul(bps);
    let mut added = 0usize;
    for e in img.meta.extra_samples.iter().take(codecs::MAX_EXTRA_SAMPLES) {
        match native_plane_to_be(&e.data, bps) {
            Ok(be) if be.len() == plane_len => {
                data.extend(be);
                added += 1;
            }
            _ => warnings.push("an extra TIFF sample did not match the image and was dropped".into()),
        }
    }
    let channels = u16::try_from(img.layout().channels().saturating_add(added)).map_err(|_| IoError::Unsupported("too many channels".into()))?;
    let header = Header::new(isd.version, w, h, channels, depth, mode);
    let image_data = ImageData { compression: Compression::Raw, data };
    let file = isd.into_psd(header, resources, image_data);
    let (mut doc, w) = psd_to_document(&file);
    warnings.extend(w);
    doc.name = name.to_string();
    if doc.icc_profile.is_none() {
        doc.icc_profile = img.icc.clone().map(Arc::new);
    }
    doc.metadata.text = img.meta.text.clone();
    Ok(ImportResult { document: doc, warnings, source_read_only: false, preview_only: false })
}

/// Interleaved native-endian samples → planar big-endian planes (PSD merged-image order),
/// inverting the colour channels of CMYK (PSD stores ink as 255 − value).
fn planar_big_endian(img: &Image, color_channels: usize, invert_color: bool) -> Result<Vec<u8>, IoError> {
    let (w, h) = img.dimensions();
    let n = w as usize * h as usize;
    let ch = img.layout().channels();
    let bps = img.sample_type().bytes();
    let mut out = try_buffer(n * ch, bps)?;
    out.resize(n * ch * bps, 0);
    let data = img.data();
    for (c, plane) in out.chunks_exact_mut(n * bps).enumerate() {
        let invert = invert_color && c < color_channels;
        let pixels = data.chunks_exact(ch * bps).map(|px| &px[c * bps..(c + 1) * bps]);
        for (dst, src) in plane.chunks_exact_mut(bps).zip(pixels) {
            match (bps, src) {
                (1, [v]) => dst[0] = if invert { 255 - *v } else { *v },
                (2, [a, b]) => {
                    let v = u16::from_ne_bytes([*a, *b]);
                    dst.copy_from_slice(&(if invert { 65535 - v } else { v }).to_be_bytes());
                }
                (4, [a, b, c, d]) => {
                    let v = f32::from_ne_bytes([*a, *b, *c, *d]);
                    dst.copy_from_slice(&(if invert { 1.0 - v } else { v }).to_be_bytes());
                }
                _ => return Err(IoError::Unsupported(format!("{bps}-byte samples"))),
            }
        }
    }
    Ok(out)
}

/// Planar big-endian planes (the PSD merged image) → an interleaved native-endian codec image
/// of `channels` channels, un-inverting CMYK colour channels.
fn interleave(planes: &[u8], w: u32, h: u32, channels: usize, color_channels: usize, sample: CSample, invert_color: bool) -> Result<Vec<u8>, IoError> {
    let n = w as usize * h as usize;
    let bps = sample.bytes();
    let plane = n * bps;
    if planes.len() < plane * channels {
        return Err(IoError::Unsupported("the merged image is shorter than its header says".into()));
    }
    let mut out = try_buffer(n * channels, bps)?;
    for i in 0..n {
        for c in 0..channels {
            let at = c * plane + i * bps;
            let invert = invert_color && c < color_channels;
            match planes.get(at..at + bps) {
                Some([v]) => out.push(if invert { 255 - *v } else { *v }),
                Some([a, b]) => {
                    let v = u16::from_be_bytes([*a, *b]);
                    out.extend_from_slice(&(if invert { 65535 - v } else { v }).to_ne_bytes());
                }
                Some([a, b, c, d]) => {
                    let v = f32::from_be_bytes([*a, *b, *c, *d]);
                    out.extend_from_slice(&(if invert { 1.0 - v } else { v }).to_ne_bytes());
                }
                _ => return Err(IoError::Unsupported(format!("{bps}-byte samples"))),
            }
        }
    }
    Ok(out)
}

/// Saves `doc` as a layered TIFF. Documents whose colour mode a TIFF cannot hold in our
/// codec (Lab) are saved flat, with a warning.
pub(crate) fn export_layered(doc: &Document, opts: &ExportOptions) -> Result<ExportResult, IoError> {
    if !layered_mode(doc) {
        let mut r = crate::flat::export_flat(doc, Format::Tiff, opts)?;
        if !doc.layers.is_empty() {
            r.warnings.push(format!("{:?} documents are saved as a flat TIFF: their layers are not kept", doc.pixel_format().mode));
        }
        return Ok(r);
    }
    let mut warnings = Vec::new();
    let big = doc.size.width > 30_000 || doc.size.height > 30_000;
    // The PSD exporter renders the composite we need for the TIFF itself: unmatted (the TIFF
    // keeps straight alpha) and uncompressed (its planes are interleaved below).
    let psd_opts = PsdExportOptions { force_psb: opts.force_psb || big, merged_matte: false, merged_compression: Compression::Raw };
    let (file, w) = document_to_psd_with(doc, &psd_opts);
    warnings.extend(w);
    let order = if codecs::tiff_writes_little_endian() { ByteOrder::Little } else { ByteOrder::Big };
    // Photoshop and print RIPs ignore ExtraSamples values and pair every sample past the colour
    // channels, in order, with alpha names / DisplayInfo. The merged transparency must not occupy
    // that first slot when a spot is also stored, or the spot's name is attached to the alpha
    // plane and the spot pixels themselves are left undescribed. Layer transparency stays in each
    // layer's own channel; only the merged image omits it.
    let mut isd = ImageSourceData::from_psd(&file);
    let spot_extras = file.header.channels as usize > {
        let cc = doc.pixel_format().mode.color_channels();
        let alpha = file.layer_info.as_ref().is_some_and(|l| l.merged_alpha);
        cc + usize::from(alpha)
    };
    if spot_extras && let Some(info) = &mut isd.layer_info {
        info.merged_alpha = false;
    }
    let (layers, w) = isd.to_bytes(order)?;
    warnings.extend(w.into_iter().map(|w| format!("layer data: {w}")));
    // The profile and XMP are written as TIFF tags, not as resources. EXIF stays a resource
    // (1058): the TIFF codec has no EXIF directory, and the PSD importer reads it back from there.
    let mut resources: Vec<ImageResource> = file.resources.iter().filter(|r| !matches!(r.id, ids::ICC_PROFILE | ids::XMP)).cloned().collect();

    let fmt = doc.pixel_format();
    let cc = fmt.mode.color_channels();
    let has_alpha = file.layer_info.as_ref().is_some_and(|l| l.merged_alpha);
    let channels = cc + usize::from(has_alpha);
    let extra_n = usize::from(file.header.channels).saturating_sub(channels);
    let kept = extra_n.min(codecs::MAX_EXTRA_SAMPLES);
    if extra_n > kept {
        warnings.push(format!("{extra_n} extra channels; TIFF keeps the first {kept}"));
    }
    // With spot or alpha channels, the IFD image is colour + those channels. The merged
    // transparency plane stays out of the IFD (see above); a file with no extra channels keeps
    // it, so a plain transparent TIFF still opens with alpha.
    let write_alpha = has_alpha && kept == 0;
    // One identifier per extra sample actually stored, replacing any preserved 1053 whose count
    // described the PSD merged image (which still included transparency).
    if kept > 0 {
        resources.retain(|r| r.id != crate::channel_map::ALPHA_IDENTIFIERS);
        resources.push(ImageResource::new(crate::channel_map::ALPHA_IDENTIFIERS, crate::channel_map::alpha_identifiers(kept)));
    }
    // Resource 1022 names a channel of the PSD merged image, which counts the transparency plane.
    // The IFD below does not, so the id moves back by one.
    if has_alpha
        && kept > 0
        && let Some(r) = resources.iter_mut().find(|r| r.id == crate::channel_map::QUICK_MASK_INFO)
        && r.data.len() >= 2
    {
        let id = u16::from_be_bytes([r.data[0], r.data[1]]).saturating_sub(1);
        let be = id.to_be_bytes();
        r.data[0] = be[0];
        r.data[1] = be[1];
    }
    let resources = resources_to_bytes(&resources)?;
    let ifd_channels = cc + usize::from(write_alpha);
    let sample = csample(fmt.sample);
    let (w, h) = (doc.size.width, doc.size.height);
    let cmyk = fmt.mode == ColorMode::Cmyk;
    let data = interleave(&file.image_data.data, w, h, ifd_channels, cc, sample, cmyk)?;
    let extras = native_extra_planes(&file.image_data.data, w, h, channels, kept, sample)?;
    let layout = layout_for(fmt.mode, write_alpha);
    let mut img = Image::from_raw(w, h, layout, sample, data)?;
    img.icc = doc.icc_profile.as_ref().map(|i| i.to_vec());
    img.meta = codecs::Metadata {
        // EXIF is deliberately not set here: the TIFF codec has no EXIF directory, so the tags
        // above carry it as resource 1058 and the importer reads them back from there. Handing the
        // codec a second copy in `meta.exif` bought nothing and made the export claim it would
        // drop metadata it had in fact kept (#1545).
        xmp: doc.metadata.xmp.clone().filter(|_| opts.xmp == crate::XmpEmbed::All),
        text: if opts.xmp == crate::XmpEmbed::All { doc.metadata.text.clone() } else { Vec::new() },
        dpi: Some((doc.resolution_dpi, doc.resolution_dpi)),
        photoshop_resources: Some(resources),
        photoshop_layers: Some(layers),
        extra_samples: extras,
        ..Default::default()
    };
    encode_image(&img, Format::Tiff, opts, warnings)
}

/// Removes the blocks a document kept verbatim from a little-endian TIFF because their layout
/// is unknown ([`photocraft_psd::TaggedBlock::is_foreign_order`]) before `file` is written as a
/// PSD: their bytes are in the wrong order for a PSD and cannot be converted. Returns a warning
/// per block dropped. A save back to TIFF keeps them (see [`photocraft_psd::tiff`]).
pub fn strip_foreign_order_blocks(file: &mut photocraft_psd::PsdFile) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut strip = |blocks: &mut Vec<photocraft_psd::TaggedBlock>, owner: &str| {
        blocks.retain(|b| {
            let keep = !b.is_foreign_order();
            if !keep {
                warnings.push(format!(
                    "{owner}: {} block ({} bytes) kept from a little-endian TIFF cannot be written to PSD and was dropped",
                    b.key_str(),
                    b.data.len()
                ));
            }
            keep
        });
    };
    if let Some(info) = &mut file.layer_info {
        for l in &mut info.layers {
            let name = l.name();
            strip(&mut l.blocks, &format!("layer \"{name}\""));
        }
    }
    strip(&mut file.global_blocks, "document");
    warnings
}

/// Big-endian planar samples (a PSD merged-image plane) to native endian, or the reverse.
fn swap_plane(src: &[u8], bps: usize, to_be: bool) -> Result<Vec<u8>, IoError> {
    if bps == 1 {
        return Ok(src.to_vec());
    }
    if !matches!(bps, 2 | 4) || !src.len().is_multiple_of(bps) {
        return Err(IoError::Unsupported("an extra channel plane is incomplete".into()));
    }
    let mut out = Vec::with_capacity(src.len());
    for c in src.chunks_exact(bps) {
        match (bps, to_be, c) {
            (2, true, [a, b]) => {
                let v = u16::from_ne_bytes([*a, *b]);
                out.extend_from_slice(&v.to_be_bytes());
            }
            (2, false, [a, b]) => {
                let v = u16::from_be_bytes([*a, *b]);
                out.extend_from_slice(&v.to_ne_bytes());
            }
            (4, true, [a, b, c, d]) => {
                let v = f32::from_ne_bytes([*a, *b, *c, *d]);
                out.extend_from_slice(&v.to_be_bytes());
            }
            (4, false, [a, b, c, d]) => {
                let v = f32::from_be_bytes([*a, *b, *c, *d]);
                out.extend_from_slice(&v.to_ne_bytes());
            }
            _ => return Err(IoError::Unsupported("an extra channel plane is incomplete".into())),
        }
    }
    Ok(out)
}

/// Native-endian extra-sample plane → big-endian, for the PSD merged image a layered TIFF rebuilds.
fn native_plane_to_be(src: &[u8], bps: usize) -> Result<Vec<u8>, IoError> {
    swap_plane(src, bps, true)
}

/// Extra planes of a PSD merged image (`skip` colour-and-alpha planes already consumed), as
/// native-endian TIFF extra samples. Spot and alpha planes are not ink-inverted.
fn native_extra_planes(planes: &[u8], w: u32, h: u32, skip: usize, count: usize, sample: CSample) -> Result<Vec<codecs::ExtraSample>, IoError> {
    let bps = sample.bytes();
    let n = (w as usize).checked_mul(h as usize).ok_or_else(|| IoError::Unsupported("image size overflows".into()))?;
    let plane = n.checked_mul(bps).ok_or_else(|| IoError::Unsupported("image size overflows".into()))?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = skip.checked_add(i).and_then(|k| k.checked_mul(plane)).ok_or_else(|| IoError::Unsupported("image size overflows".into()))?;
        let Some(src) = planes.get(at..at.saturating_add(plane)) else {
            return Err(IoError::Unsupported("the merged image is missing an extra channel".into()));
        };
        if src.len() != plane {
            return Err(IoError::Unsupported("the merged image is missing an extra channel".into()));
        }
        out.push(codecs::ExtraSample { kind: 0, data: swap_plane(src, bps, false)? });
    }
    Ok(out)
}

/// One grayscale channel as native-endian samples in `img`'s sample type.
fn channel_plane(channel: &AlphaChannel, canvas: Rect, sample: CSample) -> Result<Vec<u8>, String> {
    let depth = match sample {
        CSample::U8 => SampleType::U8,
        CSample::U16 => SampleType::U16,
        CSample::F16 | CSample::F32 => SampleType::F32,
    };
    let fmt = PixelFormat::new(ColorMode::Grayscale, depth, false);
    let owned;
    let surface = if channel.surface.format() == fmt {
        &channel.surface
    } else {
        owned = channel.surface.convert(fmt);
        &owned
    };
    let bytes = surface.to_interleaved(canvas);
    let n = (canvas.width() as usize).saturating_mul(canvas.height() as usize);
    let expect = n.saturating_mul(depth.bytes());
    if bytes.len() != expect {
        return Err(format!("channel \"{}\" does not cover the canvas", channel.name));
    }
    if sample == CSample::F16 {
        let mut out = Vec::with_capacity(n.saturating_mul(2));
        for chunk in bytes.chunks_exact(4) {
            let Ok(arr) = <[u8; 4]>::try_from(chunk) else {
                return Err(format!("channel \"{}\" does not cover the canvas", channel.name));
            };
            let v = f32::from_ne_bytes(arr);
            out.extend_from_slice(&codecs::f16::from_f32(v).to_ne_bytes());
        }
        return Ok(out);
    }
    Ok(bytes)
}

/// Drops the straight alpha sample so `img` is colour channels only. Metadata is kept.
/// `false` when there is no alpha or the buffer is not one sample per channel.
fn strip_alpha(img: &mut Image) -> bool {
    if !img.layout().has_alpha() {
        return false;
    }
    let (w, h) = img.dimensions();
    let sample = img.sample_type();
    let dst = img.layout().without_alpha();
    let bps = sample.bytes();
    let keep = dst.channels().saturating_mul(bps);
    let stride = img.layout().channels().saturating_mul(bps);
    if stride == 0 || keep == 0 || !img.data().len().is_multiple_of(stride) {
        return false;
    }
    let mut out = Vec::with_capacity(img.data().len() / stride * keep);
    for px in img.data().chunks_exact(stride) {
        let Some(color) = px.get(..keep) else { return false };
        out.extend_from_slice(color);
    }
    let icc = img.icc.clone();
    let meta = img.meta.clone();
    let warnings = img.warnings.clone();
    let Ok(mut next) = Image::from_raw(w, h, dst, sample, out) else { return false };
    next.icc = icc;
    next.meta = meta;
    next.warnings = warnings;
    *img = next;
    true
}

/// Writes alpha and spot channels (and a Quick Mask) into a flat TIFF: extra samples in the
/// IFD, plus Photoshop resources for the name, ink colour and solidity. At most
/// [`codecs::MAX_EXTRA_SAMPLES`] are kept. A TIFF with no extra channels is left unchanged.
/// When any are written, the composite's own transparency is left out of the IFD: readers pair
/// the channel resources with every sample past the colour channels, in order.
pub(crate) fn stamp_flat_channels(doc: &Document, img: &mut Image) -> Vec<String> {
    let total = doc.channels.len().saturating_add(usize::from(doc.quick_mask.is_some()));
    if total == 0 {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    let (w, h) = img.dimensions();
    if w != doc.size.width || h != doc.size.height {
        warnings.push("extra channels were not written: the image size differs from the document".into());
        return warnings;
    }
    let max = codecs::MAX_EXTRA_SAMPLES;
    let mut picked: Vec<&AlphaChannel> = doc.channels.iter().take(max).collect();
    if let Some(q) = &doc.quick_mask
        && picked.len() < max
    {
        picked.push(q);
    }
    if total > picked.len() {
        warnings.push(format!("{total} extra channels; TIFF keeps the first {}", picked.len()));
    }
    let canvas = Rect::new(0, 0, w as i32, h as i32);
    let mut planes = Vec::with_capacity(picked.len());
    for ch in &picked {
        match channel_plane(ch, canvas, img.sample_type()) {
            Ok(data) => planes.push(data),
            Err(msg) => {
                warnings.push(msg);
                break;
            }
        }
    }
    picked.truncate(planes.len());
    if picked.is_empty() {
        return warnings;
    }
    let names: Vec<&str> = picked.iter().map(|c| c.name.as_str()).collect();
    let quick_last = matches!((picked.last(), doc.quick_mask.as_ref()), (Some(p), Some(q)) if std::ptr::eq(*p, q));
    let color_channels = img.layout().color_channels();
    if img.layout().has_alpha() && !strip_alpha(img) {
        warnings.push("extra channels were not written: the composite transparency could not be separated from them".into());
        return warnings;
    }
    let quick_id = if quick_last { u16::try_from(color_channels.saturating_add(picked.len()).saturating_sub(1)).ok() } else { None };
    match channel_resources(img.meta.photoshop_resources.as_deref(), &names, &picked, quick_id) {
        Ok(bytes) => img.meta.photoshop_resources = Some(bytes),
        Err(e) => warnings.push(format!("spot channel options were not written to the TIFF: {e}")),
    }
    img.meta.extra_samples = planes.into_iter().map(|data| codecs::ExtraSample { kind: 0, data }).collect();
    warnings
}

/// Existing Photoshop resources with the channel-name, display-info and quick-mask blocks replaced.
fn channel_resources(existing: Option<&[u8]>, names: &[&str], channels: &[&AlphaChannel], quick_id: Option<u16>) -> Result<Vec<u8>, IoError> {
    let mut resources = match existing.filter(|b| !b.is_empty()) {
        Some(b) => resources_from_bytes(b)?.0,
        None => Vec::new(),
    };
    resources.retain(|r| {
        !matches!(r.id, 1006 | 1045 | crate::channel_map::DISPLAY_INFO | crate::channel_map::QUICK_MASK_INFO | crate::channel_map::ALPHA_IDENTIFIERS)
    });
    resources.push(ImageResource::new(1006, crate::psd_export::pascal_names_resource(names)));
    resources.push(ImageResource::new(1045, crate::psd_export::unicode_names_resource(names)));
    resources.push(ImageResource::new(crate::channel_map::DISPLAY_INFO, crate::channel_map::display_info(channels)));
    resources.push(ImageResource::new(crate::channel_map::ALPHA_IDENTIFIERS, crate::channel_map::alpha_identifiers(names.len())));
    if let Some(id) = quick_id {
        let mut data = id.to_be_bytes().to_vec();
        data.push(0);
        resources.push(ImageResource::new(crate::channel_map::QUICK_MASK_INFO, data));
    }
    Ok(resources_to_bytes(&resources)?)
}

/// Extra samples of a flat TIFF become alpha / spot channels. Layered TIFFs rebuild those
/// through the PSD importer instead, so this is only for a file with no layer tag.
pub(crate) fn channels_from_extras(doc: &mut Document, img: &Image) -> Vec<String> {
    if img.meta.extra_samples.is_empty() {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    let (w, h) = img.dimensions();
    if w != doc.size.width || h != doc.size.height {
        warnings.push("extra TIFF samples were dropped: their size differs from the image".into());
        return warnings;
    }
    let parsed = img.meta.photoshop_resources.as_deref().filter(|b| !b.is_empty()).and_then(|b| resources_from_bytes(b).ok());
    let none = Vec::new();
    let resources = parsed.as_ref().map(|(r, _)| r.as_slice()).unwrap_or(&none);
    let names = resources.iter().find(|r| r.id == 1045).map(|r| crate::psd_import::unicode_names(&r.data)).unwrap_or_default();
    let start = doc.channels.len();
    let n_px = (w as usize).saturating_mul(h as usize);
    for (k, e) in img.meta.extra_samples.iter().take(codecs::MAX_EXTRA_SAMPLES).enumerate() {
        match mask_from_plane(&e.data, w, h, n_px, img.sample_type(), doc.depth) {
            Ok(surface) => {
                let name = names.get(k).cloned().filter(|s| !s.is_empty()).unwrap_or_else(|| format!("Alpha {}", k + 1));
                doc.channels.push(AlphaChannel::new(name, surface));
            }
            Err(msg) => warnings.push(msg),
        }
    }
    if doc.channels.len() == start {
        return warnings;
    }
    {
        let slice = &mut doc.channels[start..];
        if let Some(r) = resources.iter().find(|r| r.id == crate::channel_map::DISPLAY_INFO) {
            crate::channel_map::apply_display_info(&r.data, true, slice);
        } else if let Some(r) = resources.iter().find(|r| r.id == crate::channel_map::DISPLAY_INFO_OLD) {
            crate::channel_map::apply_display_info(&r.data, false, slice);
        }
    }
    let first = img.layout().color_channels().saturating_add(usize::from(img.layout().has_alpha()));
    if let Some(r) = resources.iter().find(|r| r.id == crate::channel_map::QUICK_MASK_INFO)
        && r.data.len() >= 2
        && let Some(k) = usize::from(u16::from_be_bytes([r.data[0], r.data[1]])).checked_sub(first)
        && start.saturating_add(k) < doc.channels.len()
    {
        doc.quick_mask = Some(doc.channels.remove(start + k));
    }
    warnings
}

/// Native-endian extra-sample bytes → a grayscale surface at the document depth.
fn mask_from_plane(data: &[u8], w: u32, h: u32, n: usize, from: CSample, depth: SampleType) -> Result<Surface, String> {
    let rect = Rect::new(0, 0, w as i32, h as i32);
    let bytes = match (from, depth) {
        (CSample::U8, SampleType::U8) | (CSample::U16, SampleType::U16) | (CSample::F32, SampleType::F32) => {
            if data.len() != n.saturating_mul(from.bytes()) {
                return Err("an extra TIFF sample did not match the image and was dropped".into());
            }
            data.to_vec()
        }
        (CSample::F16, SampleType::F32) => {
            if data.len() != n.saturating_mul(2) {
                return Err("an extra TIFF sample did not match the image and was dropped".into());
            }
            let mut out = Vec::with_capacity(n.saturating_mul(4));
            for chunk in data.chunks_exact(2) {
                let Ok(arr) = <[u8; 2]>::try_from(chunk) else {
                    return Err("an extra TIFF sample did not match the image and was dropped".into());
                };
                let v = codecs::f16::from_bits(u16::from_ne_bytes(arr)).to_f32();
                out.extend_from_slice(&v.to_ne_bytes());
            }
            out
        }
        _ => return Err(format!("an extra TIFF sample ({from:?}) does not match the document depth and was dropped")),
    };
    if bytes.len() != n.saturating_mul(depth.bytes()) {
        return Err("an extra TIFF sample did not match the image and was dropped".into());
    }
    let mut s = Surface::new(PixelFormat::new(ColorMode::Grayscale, depth, false));
    s.write_interleaved(rect, &bytes);
    s.prune();
    Ok(s)
}
