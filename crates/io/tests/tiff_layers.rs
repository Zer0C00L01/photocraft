//! Layered TIFF (Photoshop layer data in tags 37724 / 34377): Document → TIFF → Document round
//! trips in every mode and depth, equivalence with the PSD path, the flat fall-backs, files
//! written by an independent implementation (psdtags, both byte orders), and malformed input.

mod common;

use common::*;
use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::{Document, LayerContent};
use photocraft_io::*;
use photocraft_psd::tiff::{ByteOrder, ImageSourceData};

fn tiff(doc: &Document, layers: bool) -> ExportResult {
    export(doc, "x.tif", &ExportOptions { tiff_layers: layers, ..Default::default() }).expect("export")
}

/// The two Photoshop tags of a TIFF: (resources, layers).
fn tags(bytes: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let (r, l) = photocraft_codecs::tiff_photoshop_tags(bytes);
    (r.map(<[u8]>::to_vec), l.map(<[u8]>::to_vec))
}

fn roundtrip(doc: &Document) -> Document {
    let r = tiff(doc, true);
    let (resources, layers) = tags(&r.bytes);
    let layers = layers.expect("layer data");
    assert!(resources.is_some_and(|r| !r.is_empty()), "image resources");
    // The layer data matches the container's byte order and is byte-stable through the psd crate.
    let order = if photocraft_codecs::tiff_writes_little_endian() { ByteOrder::Little } else { ByteOrder::Big };
    let (isd, w) = ImageSourceData::from_bytes(&layers).expect("parse layer data");
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(isd.byte_order, order);
    assert_eq!(isd.to_bytes(order).unwrap().0, layers);
    // The TIFF is a plain image to any other reader: the composite at the document's size.
    let flat = photocraft_codecs::decode(&r.bytes).expect("flat decode");
    assert_eq!(flat.dimensions(), (doc.size.width, doc.size.height));
    // Extra alpha channels and a Quick Mask travel as TIFF extra samples (at most MAX_EXTRA_SAMPLES).
    let extra = doc.channels.len() + usize::from(doc.quick_mask.is_some());
    assert!(!r.warnings.iter().any(|w| w.contains("not written")), "{:?}", r.warnings);
    if extra > 0 && extra <= photocraft_codecs::MAX_EXTRA_SAMPLES {
        assert_eq!(flat.meta.extra_samples.len(), extra, "{:?}", r.warnings);
    }
    import("x.tif", &r.bytes).expect("import").document
}

/// `doc` as a layered TIFF round trip returns it, including extra channels.
fn expected(doc: &Document) -> Document {
    doc.clone()
}

macro_rules! rt_case {
    ($name:ident, $mode:expr, $depth:expr, $f:expr) => {
        #[test]
        fn $name() {
            let d = gen_doc($mode, $depth, $f);
            let back = roundtrip(&d);
            assert_docs_eq(&expected(&d), &back);
        }
    };
}

rt_case!(rt_rgb8_all, ColorMode::Rgb, SampleType::U8, Features::ALL);
rt_case!(rt_rgb16_all, ColorMode::Rgb, SampleType::U16, Features::ALL);
rt_case!(rt_rgb32_all, ColorMode::Rgb, SampleType::F32, Features::ALL);
rt_case!(rt_gray8_all, ColorMode::Grayscale, SampleType::U8, Features::ALL);
rt_case!(rt_gray16_all, ColorMode::Grayscale, SampleType::U16, Features::ALL);
rt_case!(rt_gray32_all, ColorMode::Grayscale, SampleType::F32, Features::ALL);
rt_case!(rt_cmyk8_all, ColorMode::Cmyk, SampleType::U8, Features::ALL);
rt_case!(rt_cmyk16_all, ColorMode::Cmyk, SampleType::U16, Features::ALL);
rt_case!(rt_rgb8_pixels, ColorMode::Rgb, SampleType::U8, Features::PIXELS);
rt_case!(rt_gray16_pixels, ColorMode::Grayscale, SampleType::U16, Features::PIXELS);
rt_case!(rt_cmyk8_pixels, ColorMode::Cmyk, SampleType::U8, Features::PIXELS);

#[test]
fn tiff_and_psd_open_as_the_same_document() {
    for (mode, depth) in [(ColorMode::Rgb, SampleType::U8), (ColorMode::Cmyk, SampleType::U16), (ColorMode::Grayscale, SampleType::F32)] {
        let d = gen_doc(mode, depth, Features::ALL);
        let psd = export(&d, "x.psd", &ExportOptions::default()).unwrap();
        let from_psd = import("x.psd", &psd.bytes).unwrap().document;
        let from_tif = roundtrip(&d);
        assert_docs_eq(&expected(&from_psd), &from_tif);
        // The composite the TIFF carries is what the document looks like.
        let tif = tiff(&d, true);
        let flat = import(
            "flat.tif",
            &photocraft_codecs::encode(&photocraft_codecs::decode(&tif.bytes).unwrap(), photocraft_codecs::Format::Tiff, &Default::default()).unwrap(),
        )
        .unwrap()
        .document;
        let a = photocraft_compose::flatten(&d).px;
        let b = photocraft_compose::flatten(&flat).px;
        assert!(max_diff(&a, &b) <= 2.0 / 255.0, "{mode:?} {depth:?}: composite differs by {}", max_diff(&a, &b));
    }
}

#[test]
fn every_compression_keeps_the_layers() {
    use photocraft_codecs::TiffCompression;
    let d = gen_doc(ColorMode::Rgb, SampleType::U16, Features::PIXELS);
    for c in [TiffCompression::None, TiffCompression::Lzw, TiffCompression::Deflate, TiffCompression::PackBits] {
        let mut opts = ExportOptions { tiff_layers: true, ..Default::default() };
        opts.encode.tiff_compression = c;
        let r = export(&d, "x.tiff", &opts).unwrap();
        let back = import("x.tiff", &r.bytes).unwrap().document;
        assert_docs_eq(&d, &back);
    }
}

/// The Artist tag's text, and the bytes every EXIF assertion looks for.
const ARTIST: &[u8] = b"fixture-artist";

/// A minimal little-endian EXIF (TIFF) block with a single Artist tag and no Orientation, so
/// `upright_exif` passes it through untouched.
fn exif() -> Vec<u8> {
    let mut artist = ARTIST.to_vec();
    artist.push(0); // ASCII NUL terminator
    let mut v = b"II*\0\x08\0\0\0".to_vec(); // TIFF header, first IFD at 8
    v.extend_from_slice(&1u16.to_le_bytes()); // one entry
    v.extend_from_slice(&0x013bu16.to_le_bytes()); // Artist
    v.extend_from_slice(&2u16.to_le_bytes()); // ASCII
    v.extend_from_slice(&(artist.len() as u32).to_le_bytes());
    v.extend_from_slice(&26u32.to_le_bytes()); // value offset: 8 + 2 + 12 + 4
    v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    v.extend_from_slice(&artist);
    v
}

/// #1545: a layered TIFF carries EXIF as Photoshop resource 1058 and reads it back, so it must
/// not report a loss it never makes. A flat TIFF really does lose the tags and still says so.
#[test]
fn layered_tiff_keeps_exif_without_claiming_a_loss() {
    let mut d = gen_doc(ColorMode::Rgb, SampleType::U8, Features::PIXELS);
    d.metadata.exif = Some(std::sync::Arc::new(exif()));

    let r = tiff(&d, true);
    assert!(!r.warnings.iter().any(|w| w.contains("EXIF")), "a layered TIFF keeps EXIF, so it must not claim to drop it: {:?}", r.warnings);
    // ...and the tags really are in the file, so the missing warning is a true absence of loss.
    let back = import("x.tif", &r.bytes).unwrap().document;
    let kept = back.metadata.exif.as_deref().expect("EXIF survives the layered export");
    assert!(kept.windows(ARTIST.len()).any(|w| w == ARTIST), "the Artist tag did not survive: {kept:?}");

    // The control from the issue: the flat path drops it for real, and keeps reporting that.
    let flat = tiff(&d, false);
    assert!(flat.warnings.iter().any(|w| w.contains("EXIF")), "a flat TIFF really does lose EXIF: {:?}", flat.warnings);
}

#[test]
fn discard_layers_writes_a_flat_file() {
    let d = gen_doc(ColorMode::Rgb, SampleType::U8, Features::PIXELS);
    let r = tiff(&d, false);
    assert_eq!(tags(&r.bytes), (None, None));
    assert!(r.warnings.iter().any(|w| w.contains("flattened")), "{:?}", r.warnings);
    let back = import("x.tif", &r.bytes).unwrap().document;
    assert_eq!(back.layers.len(), 1);
    assert!(!tiff_layers::would_write_layers(&back));
}

#[test]
fn a_plain_background_needs_no_layer_data() {
    let d = import("x.png", &export(&gen_doc(ColorMode::Rgb, SampleType::U8, Features::PIXELS), "x.png", &Default::default()).unwrap().bytes).unwrap().document;
    assert_eq!(d.layers.len(), 1);
    assert!(!tiff_layers::would_write_layers(&d));
    let r = tiff(&d, true);
    assert_eq!(tags(&r.bytes), (None, None));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let back = import("x.tif", &r.bytes).unwrap().document;
    assert_docs_eq(&d, &back);
}

#[test]
fn lab_documents_are_saved_flat_with_a_warning() {
    let d = gen_doc(ColorMode::Lab, SampleType::U8, Features::PIXELS);
    let r = tiff(&d, true);
    assert_eq!(tags(&r.bytes), (None, None));
    assert!(r.warnings.iter().any(|w| w.contains("Lab") && w.contains("layers are not kept")), "{:?}", r.warnings);
    assert!(import("x.tif", &r.bytes).is_ok());
}

#[test]
fn transparency_is_straight_in_the_composite() {
    // A translucent layer over nothing: the TIFF's composite keeps its colour under the alpha
    // instead of being matted against white like a PSD's merged image.
    let mut d = Document::new("t", photocraft_geom::Size::new(4, 4), ColorMode::Rgb, SampleType::U8);
    let fmt = d.pixel_format();
    let mut l = raster("Red", fmt, photocraft_geom::Rect::new(0, 0, 4, 4), 0, false);
    if let LayerContent::Raster(s) = &mut l.content {
        let px: Vec<f32> = (0..16).flat_map(|_| [1.0, 0.0, 0.0, 0.5]).collect();
        s.write_region(photocraft_geom::Rect::new(0, 0, 4, 4), &px);
    }
    d.layers.push(l);
    let r = tiff(&d, true);
    let flat = photocraft_codecs::decode(&r.bytes).unwrap();
    assert!(flat.layout().has_alpha());
    let px = flat.data();
    assert_eq!(&px[..4], &[255, 0, 0, 128]);
    let back = import("x.tif", &r.bytes).unwrap().document;
    assert_docs_eq(&d, &back);
}

#[test]
fn big_documents_use_the_psb_signature() {
    // Forcing PSB stands in for a > 30000 px document.
    let d = gen_doc(ColorMode::Rgb, SampleType::U8, Features::PIXELS);
    let r = export(&d, "x.tif", &ExportOptions { force_psb: true, tiff_layers: true, ..Default::default() }).unwrap();
    let (_, layers) = tags(&r.bytes);
    assert!(layers.unwrap().starts_with(photocraft_psd::tiff::SIGNATURE_PSB));
    let back = import("x.tif", &r.bytes).unwrap().document;
    assert_docs_eq(&d, &back);
}

/// Files written by psdtags (`scripts/layered_tiff_fixtures.py`): two RGB layers, "Background"
/// (RLE) and an offset, translucent "Upper é" (ZIP, Multiply, opacity 200), in both byte orders
/// at 8 and 16 bits. Binary files stay out of this repository: run the script to write them
/// to `corpus/layered-tiff/` (gitignored); without them these checks are skipped.
fn fixture(name: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(format!("{}/../../corpus/layered-tiff/{name}", env!("CARGO_MANIFEST_DIR"))).ok();
    if bytes.is_none() {
        eprintln!("skipped {name}: run `python scripts/layered_tiff_fixtures.py` to write corpus/layered-tiff/");
    }
    bytes
}

#[test]
fn psdtags_fixtures_open_with_their_layers() {
    for (name, depth) in [
        ("layered-le-8bit.tif", SampleType::U8),
        ("layered-be-8bit.tif", SampleType::U8),
        ("layered-le-16bit.tif", SampleType::U16),
        ("layered-be-16bit.tif", SampleType::U16),
    ] {
        let Some(bytes) = fixture(name) else { continue };
        let r = import(name, &bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let d = &r.document;
        assert_eq!(d.size, photocraft_geom::Size::new(6, 4), "{name}");
        assert_eq!(d.mode, ColorMode::Rgb);
        assert_eq!(d.depth, depth, "{name}");
        let names: Vec<&str> = d.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Background", "Upper \u{e9}"], "{name}");
        let upper = &d.layers[1];
        assert_eq!(upper.blend, photocraft_color::BlendMode::Multiply);
        assert!((upper.opacity - 200.0 / 255.0).abs() < 1e-6);
        let LayerContent::Raster(s) = &upper.content else { panic!("raster") };
        let bounds = s.content_bounds();
        assert_eq!((bounds.x0, bounds.y0, bounds.width(), bounds.height()), (1, 1, 4, 3), "{name}");
        // Pixel (x=2, y=0) of the upper layer: the generator's ramp, alpha 160/255.
        let px = s.pixel(3, 1);
        let expect = |v: u32| (v % 256) as f32 / 255.0;
        assert!((px[0] - expect(2 * 40 + 7)).abs() < 1e-3, "{name}: {px:?}");
        assert!((px[1] - expect(7 * 3)).abs() < 1e-3, "{name}: {px:?}");
        assert!((px[3] - 160.0 / 255.0).abs() < 1e-3, "{name}: {px:?}");
        let LayerContent::Raster(bg) = &d.layers[0].content else { panic!("raster") };
        let b = bg.pixel(5, 3);
        assert!((b[0] - expect(5 * 40 + 1)).abs() < 1e-3, "{name}: {b:?}");
        assert!((b[3] - 1.0).abs() < 1e-6);
        assert_eq!(d.resolution_dpi, 300.0);
        // tifffile writes a Software tag; nothing else is worth a warning.
        assert!(r.warnings.iter().all(|w| w.contains("text metadata")), "{name}: {:?}", r.warnings);
        // psdtags wrote the composite's alpha sample without the transparency flag, so by the
        // PSD rules it is an alpha channel. A re-save keeps it as a TIFF extra sample.
        assert_eq!(d.channels.len(), 1, "{name}");
        let out = tiff(d, true);
        let back = import(name, &out.bytes).unwrap().document;
        assert_docs_eq(d, &back);
    }
}

/// Advanced Blending (`knko`, `infx`, `clbl`, `tsly`, `lmgm`, `vmgm`), fill opacity (`iOpa`),
/// channel restrictions (`brst`) and Blend If (the records' blending ranges) survive a layered
/// TIFF with its layer data in either byte order: the encoder's own, and the other one made by
/// transcoding the tag (the same length, spliced in place).
#[test]
fn advanced_blending_survives_layered_tiff_in_both_byte_orders() {
    use photocraft_doc::{AdvancedBlending, BlendRange, Knockout};
    for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
        let mut d = gen_doc(ColorMode::Rgb, depth, Features::PIXELS);
        let adv = AdvancedBlending {
            knockout: Knockout::Deep,
            blend_interior: true,
            blend_clipped: false,
            transparency_shapes: false,
            layer_mask_hides_effects: true,
            vector_mask_hides_effects: true,
        };
        d.layers[1].advanced = adv;
        d.layers[1].fill_opacity = 64.0 / 255.0;
        d.layers[1].excluded_channels = 0b010;
        d.layers[1].blend_if.set(0, [BlendRange { black: [10, 40], white: [200, 250] }, BlendRange { black: [0, 0], white: [128, 128] }]);
        d.layers[2].advanced.knockout = Knockout::Shallow;
        let r = tiff(&d, true);
        let (_, layers) = tags(&r.bytes);
        let layers = layers.expect("layer data");
        let (isd, w) = ImageSourceData::from_bytes(&layers).expect("parse");
        assert!(w.is_empty(), "{w:?}");
        let native = isd.byte_order;
        let other = if native == ByteOrder::Little { ByteOrder::Big } else { ByteOrder::Little };
        let (swapped, w) = isd.to_bytes(other).expect("transcode");
        assert!(w.is_empty(), "{depth:?}: blocks dropped while transcoding: {w:?}");
        assert_eq!(swapped.len(), layers.len());
        let at = r.bytes.windows(layers.len()).position(|x| x == &layers[..]).expect("tag data in the file");
        let mut flipped = r.bytes.clone();
        flipped[at..at + layers.len()].copy_from_slice(&swapped);
        for (order, bytes) in [(native, &r.bytes), (other, &flipped)] {
            let back = import("x.tif", bytes).unwrap_or_else(|e| panic!("{depth:?} {order:?}: {e}")).document;
            let l = &back.layers[1];
            assert_eq!(l.advanced, adv, "{depth:?} {order:?}");
            assert_eq!(l.fill_opacity, 64.0 / 255.0, "{depth:?} {order:?}");
            assert_eq!(l.excluded_channels, 0b010, "{depth:?} {order:?}");
            assert_eq!(l.blend_if, d.layers[1].blend_if, "{depth:?} {order:?}");
            assert_eq!(back.layers[2].advanced.knockout, Knockout::Shallow, "{depth:?} {order:?}");
            assert_docs_eq(&expected(&d), &back);
        }
    }
}

#[test]
fn damaged_layer_data_opens_flattened() {
    let d = gen_doc(ColorMode::Rgb, SampleType::U8, Features::PIXELS);
    let r = tiff(&d, true);
    let (_, layers) = tags(&r.bytes);
    let layers = layers.unwrap();
    let at = r.bytes.windows(layers.len()).position(|w| w == &layers[..]).unwrap();
    // Garbage after the signature: no layers, a warning, the composite as one layer.
    let mut bad = r.bytes.clone();
    for b in &mut bad[at + 36..at + 36 + 24] {
        *b ^= 0xa5;
    }
    let r2 = import("x.tif", &bad).unwrap();
    assert_eq!(r2.document.layers.len(), 1);
    assert!(r2.warnings.iter().any(|w| w.contains("could not be read")), "{:?}", r2.warnings);
    // Every truncation and a sweep of bit flips through the whole file: never a panic.
    for cut in (0..r.bytes.len()).step_by(11) {
        let _ = import("x.tif", &r.bytes[..cut]);
    }
    for i in (0..r.bytes.len()).step_by(29) {
        let mut b = r.bytes.clone();
        b[i] ^= 0x5a;
        let _ = import("x.tif", &b);
    }
    for name in ["layered-le-8bit.tif", "layered-be-16bit.tif"] {
        let Some(bytes) = fixture(name) else { continue };
        for cut in (0..bytes.len()).step_by(7) {
            let _ = import(name, &bytes[..cut]);
        }
        for i in (0..bytes.len()).step_by(5) {
            let mut b = bytes.clone();
            b[i] ^= 0xff;
            let _ = import(name, &b);
        }
    }
}

/// Every sample of a TIFF, read by the `tiff` crate rather than PhotoCraft's decoder.
///
/// That decoder drops unspecified extra samples when the photometric interpretation is RGB, so
/// the file is retagged MinIsBlack first. The result is then a multiband image of every sample,
/// which is what libtiff's `TIFFReadScanline` returns and what a RIP walks.
fn independent_samples(bytes: &[u8]) -> (u16, Vec<u16>, Vec<u16>, u32, Vec<u8>) {
    let mut tags = tiff::decoder::Decoder::new(std::io::Cursor::new(bytes)).expect("tiff tags");
    let spp: u16 = tags.get_tag_unsigned(tiff::tags::Tag::SamplesPerPixel).expect("spp");
    let bps = tags.get_tag_u16_vec(tiff::tags::Tag::BitsPerSample).expect("bps");
    let extra = tags.get_tag_u16_vec(tiff::tags::Tag::ExtraSamples).unwrap_or_default();
    let rows = tags.find_tag_unsigned::<u32>(tiff::tags::Tag::RowsPerStrip).ok().flatten().unwrap_or(0);
    let patched = photometric_min_is_black(bytes);
    let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(patched.as_slice())).expect("tiff decode");
    let tiff::decoder::DecodingResult::U8(samples) = dec.read_image().expect("read image") else {
        panic!("expected 8-bit samples");
    };
    (spp, bps, extra, rows, samples)
}

/// Classic-TIFF photometric tag 262 → 1 (MinIsBlack), value stored inline in the IFD entry.
fn photometric_min_is_black(src: &[u8]) -> Vec<u8> {
    let mut bytes = src.to_vec();
    let le = bytes.first().copied() == Some(b'I');
    let ru16 = |b: &[u8], i: usize| -> Option<u16> {
        let s = b.get(i..i + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    };
    let ru32 = |b: &[u8], i: usize| -> Option<u32> {
        let s = b.get(i..i + 4)?;
        Some(if le { u32::from_le_bytes(s.try_into().ok()?) } else { u32::from_be_bytes(s.try_into().ok()?) })
    };
    let Some(ifd) = ru32(&bytes, 4) else { return bytes };
    let Some(n) = ru16(&bytes, ifd as usize) else { return bytes };
    for e in 0..n as usize {
        let at = ifd as usize + 2 + e * 12;
        if ru16(&bytes, at) == Some(262) {
            let value = if le { 1u16.to_le_bytes() } else { 1u16.to_be_bytes() };
            if let Some(slot) = bytes.get_mut(at + 8..at + 10) {
                slot.copy_from_slice(&value);
            }
            break;
        }
    }
    bytes
}

fn assert_plane(samples: &[u8], spp: usize, plane: usize, expected: &[u8]) {
    assert_eq!(samples.len(), expected.len().saturating_mul(spp));
    let mut bad = 0usize;
    let mut first = None;
    for (i, want) in expected.iter().enumerate() {
        let got = samples.get(i * spp + plane).copied();
        if got != Some(*want) {
            bad += 1;
            first.get_or_insert((i, got));
        }
    }
    assert_eq!(bad, 0, "{bad} mismatches, first {first:?}");
}

/// A multi-layer transparent document, large enough for several TIFF strips and several
/// raster tiles, with a spot whose every pixel is a known value.
#[test]
fn dtf_like_spot_is_written_in_full() {
    use photocraft_color::{Color, PixelFormat};
    use photocraft_doc::{AlphaChannel, Layer};
    use photocraft_geom::{Rect, Size};
    use photocraft_psd::tiff::resources_from_bytes;
    use photocraft_raster::Surface;

    let (w, h) = (900u32, 360u32);
    let mut doc = Document::new("dtf", Size::new(w, h), ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();
    let mut art = Surface::new(fmt);
    let px = photocraft_raster::from_rgba(&fmt, [0.2, 0.5, 0.9, 1.0]);
    art.fill_rect(Rect::new(30, 20, 870, 340), &px);
    doc.layers.push(Layer::new("Art", LayerContent::Raster(art)));
    let mut ink = Surface::new(fmt);
    let px = photocraft_raster::from_rgba(&fmt, [0.9, 0.1, 0.2, 0.5]);
    ink.fill_rect(Rect::new(80, 40, 700, 300), &px);
    let mut layer = Layer::new("Ink", LayerContent::Raster(ink));
    layer.opacity = 0.8;
    doc.layers.push(layer);

    let n = (w as usize) * (h as usize);
    let mut spot_bytes = vec![0u8; n];
    for y in 0..h as usize {
        for x in 0..w as usize {
            spot_bytes[y * w as usize + x] = ((x * 3 + y * 17) % 251) as u8;
        }
    }
    // Outside the artwork the composite is transparent, but the spot is not zero: a reader that
    // binds "Spot Color 1" to the alpha plane fails here.
    assert_eq!(spot_bytes[10 * w as usize + 10], 200);
    let gray = PixelFormat::new(ColorMode::Grayscale, SampleType::U8, false);
    let spot_surface = Surface::from_interleaved(gray, Rect::new(0, 0, w as i32, h as i32), &spot_bytes);
    let mut channel = AlphaChannel::new("Spot Color 1", spot_surface);
    channel.spot = Some((Color::rgb(1.0, 0.0, 0.0), 0.6));
    doc.channels.push(channel);

    let mut seen = Vec::new();
    for (name, layers) in [("flat", false), ("layered", true)] {
        let opts = ExportOptions { tiff_layers: layers, ..ExportOptions::default() };
        let r = export(&doc, "dtf.tif", &opts).unwrap();
        assert!(!r.warnings.iter().any(|w| w.contains("not written")), "{name}: {:?}", r.warnings);
        let img = photocraft_codecs::decode(&r.bytes).unwrap();
        assert!(!img.layout().has_alpha(), "{name}: {:?}", img.layout());
        assert_eq!(img.layout().color_channels(), 3, "{name}");
        assert_eq!(img.meta.extra_samples.len(), 1, "{name}");
        assert_eq!(img.meta.extra_samples[0].kind, 0, "{name}");
        assert_eq!(img.meta.extra_samples[0].data, spot_bytes, "{name} own decoder");

        let (spp, bps, extra, rows, samples) = independent_samples(&r.bytes);
        assert_eq!(spp, 4, "{name}");
        assert_eq!(bps, vec![8, 8, 8, 8], "{name}");
        assert_eq!(extra, vec![0], "{name} ExtraSamples");
        assert!(rows > 0 && rows < h, "{name} rows_per_strip {rows}");
        assert_plane(&samples, 4, 3, &spot_bytes);
        seen.push(samples);

        let (resources, layer_tag) = tags(&r.bytes);
        let resources = resources.expect("photoshop resources");
        let (blocks, warnings) = resources_from_bytes(&resources).unwrap();
        assert!(warnings.is_empty(), "{name} {warnings:?}");
        let block = |id: u16| blocks.iter().find(|b| b.id == id).unwrap_or_else(|| panic!("{name} missing {id}"));
        // "Spot Color 1" is 12 bytes: length byte + text is odd, so resource 1006 pads to even.
        assert_eq!(block(1006).data.len(), 14, "{name}");
        assert_eq!(block(1045).data.len(), 30, "{name}");
        let info = &block(1077).data;
        assert_eq!(info.len(), 18, "{name}");
        assert_eq!(u16::from_be_bytes([info[14], info[15]]), 60, "{name} solidity");
        assert_eq!(info[16], 2, "{name} spot kind");
        assert_eq!(block(1053).data, 1u32.to_be_bytes(), "{name} channel id");

        let back = import("dtf.tif", &r.bytes).unwrap().document;
        assert_eq!(back.channels.len(), 1, "{name}");
        assert_eq!(back.channels[0].name, "Spot Color 1");
        let (ink, sol) = back.channels[0].spot.expect("spot");
        assert_eq!(ink.to_rgb(), [1.0, 0.0, 0.0], "{name}");
        assert!((sol - 0.6).abs() < 1e-4, "{name}");
        let area = Rect::new(0, 0, w as i32, h as i32);
        assert_eq!(back.channels[0].surface.to_interleaved(area), spot_bytes, "{name} reimport");
        if layers {
            let layer_tag = layer_tag.expect("layer tag");
            let (isd, w) = ImageSourceData::from_bytes(&layer_tag).unwrap();
            assert!(w.is_empty(), "{w:?}");
            assert!(!isd.layer_info.expect("layers").merged_alpha, "{name}");
            assert_eq!(back.layers.len(), 2, "{name}");
            let ink = back.layers.iter().find(|l| l.name == "Ink").expect("ink layer");
            let px = ink.surface().expect("ink pixels").pixel(100, 100);
            assert!((px[3] - 0.5).abs() < 0.02, "{name} layer alpha {px:?}");
            let outside = ink.surface().expect("ink pixels").pixel(0, 0);
            assert!(outside[3] < 0.02, "{name} transparent pixel {outside:?}");
        } else {
            assert!(layer_tag.is_none(), "{name}");
        }
    }
    assert_eq!(seen[0], seen[1], "flat and layered IFD samples differ");
}

/// A transparent document with no user channels still stores image alpha.
#[test]
fn transparent_tiff_without_channels_keeps_image_alpha() {
    use photocraft_doc::Layer;
    use photocraft_geom::{Rect, Size};
    use photocraft_raster::Surface;

    let mut doc = Document::new("t", Size::new(8, 8), ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();
    let mut art = Surface::new(fmt);
    art.fill_rect(Rect::new(1, 1, 6, 6), &photocraft_raster::from_rgba(&fmt, [1.0, 0.0, 0.0, 0.5]));
    doc.layers.push(Layer::new("Art", LayerContent::Raster(art)));
    for layers in [false, true] {
        let r = export(&doc, "t.tif", &ExportOptions { tiff_layers: layers, ..ExportOptions::default() }).unwrap();
        let img = photocraft_codecs::decode(&r.bytes).unwrap();
        assert!(img.layout().has_alpha(), "{layers}");
        assert!(img.meta.extra_samples.is_empty(), "{layers}");
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(r.bytes.as_slice())).unwrap();
        let extra = dec.get_tag_u16_vec(tiff::tags::Tag::ExtraSamples).unwrap();
        assert_eq!(extra, vec![2], "{layers}");
    }
}

/// Quick Mask stays the last channel after the merged transparency plane is omitted.
#[test]
fn spot_tiff_keeps_quick_mask_when_composite_alpha_is_omitted() {
    use photocraft_color::{Color, PixelFormat};
    use photocraft_doc::{AlphaChannel, Layer};
    use photocraft_geom::{Rect, Size};
    use photocraft_raster::Surface;

    let mut doc = Document::new("q", Size::new(8, 8), ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();
    let mut art = Surface::new(fmt);
    art.fill_rect(Rect::new(1, 1, 6, 6), &photocraft_raster::from_rgba(&fmt, [1.0, 0.0, 0.0, 0.5]));
    doc.layers.push(Layer::new("Art", LayerContent::Raster(art)));
    let gray = PixelFormat::new(ColorMode::Grayscale, SampleType::U8, false);
    let area = Rect::new(0, 0, 8, 8);
    let mut spot_bytes = vec![9u8; 64];
    spot_bytes[0] = 3;
    let mut channel = AlphaChannel::new("Spot Color 1", Surface::from_interleaved(gray, area, &spot_bytes));
    channel.spot = Some((Color::rgb(1.0, 0.0, 0.0), 0.6));
    doc.channels.push(channel);
    let mut qbytes = vec![7u8; 64];
    qbytes[1] = 4;
    doc.quick_mask = Some(AlphaChannel::new("Quick Mask", Surface::from_interleaved(gray, area, &qbytes)));

    for layers in [false, true] {
        let r = export(&doc, "q.tif", &ExportOptions { tiff_layers: layers, ..ExportOptions::default() }).unwrap();
        let (spp, _bps, extra, _rows, samples) = independent_samples(&r.bytes);
        assert_eq!((spp, extra.as_slice()), (5, [0, 0].as_slice()), "{layers}");
        assert_plane(&samples, 5, 3, &spot_bytes);
        assert_plane(&samples, 5, 4, &qbytes);
        let back = import("q.tif", &r.bytes).unwrap().document;
        assert_eq!(back.channels.len(), 1, "{layers}");
        assert_eq!(back.channels[0].surface.to_interleaved(area), spot_bytes, "{layers}");
        let qm = back.quick_mask.as_ref().unwrap_or_else(|| panic!("{layers}: no quick mask"));
        assert_eq!(qm.surface.to_interleaved(area), qbytes, "{layers}");
    }
}

#[test]
fn default_export_options_write_a_flat_tiff() {
    // Scripted and agent saves (CLI, batch, MCP) build on the defaults: layers only on request.
    let d = gen_doc(ColorMode::Rgb, SampleType::U8, Features::PIXELS);
    assert!(tiff_layers::would_write_layers(&d));
    let r = export(&d, "x.tif", &ExportOptions::default()).unwrap();
    assert_eq!(tags(&r.bytes), (None, None));
    assert_eq!(import("x.tif", &r.bytes).unwrap().document.layers.len(), 1);
}
