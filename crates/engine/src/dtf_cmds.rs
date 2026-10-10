//! Image › DTF Spot Channel, and the built-in Actions preset "DTF".
//!
//! A direct-to-film print file needs a spot channel of the artwork, shrunk by a pixel so the
//! white underbase does not peek past the colour. Photoshop's recorded action is three steps:
//! load the active layer's transparency as the selection, contract by 1px without treating the
//! canvas edge as a selection edge, then make a spot channel named "Spot Color 1" (RGB red,
//! solidity 60) from that selection. [`run`] does the same work as one history step, with those
//! values as defaults. The Actions panel preset plays the three commands so each step stays
//! visible.

use photocraft_algo::selection::{self as sel, SelectionMode};
use photocraft_color::Color;
use photocraft_doc::AlphaChannel;
use serde_json::{Value, json};

use crate::actions_cmds::Action;
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// Name of the built-in action in the Actions panel.
pub const ACTION_NAME: &str = "DTF";
/// Contract radius, in pixels. `0` keeps the transparency mask as it is.
pub const DEFAULT_CONTRACT: f32 = 1.0;
pub const DEFAULT_NAME: &str = "Spot Color 1";
pub const DEFAULT_COLOR: &str = "#ff0000";
/// Solidity on Photoshop's 0..100 scale. Stored on the channel as a 0..1 fraction.
pub const DEFAULT_SOLIDITY: f64 = 60.0;

const CMD: &str = "image.dtf";
const MAX_CONTRACT: f32 = 500.0;
const MAX_NAME_CHARS: usize = 256;

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: CMD.into(), msg: msg.into() }
}

fn enabled(s: &Session) -> std::result::Result<(), String> {
    s.active().map(|_| ()).ok_or_else(|| "no document open".into())
}

fn number(p: &Value, key: &str) -> Result<Option<f64>> {
    match p.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => {
            let v = n.as_f64().filter(|v| v.is_finite()).ok_or_else(|| bad(format!("`{key}` is not a usable number")))?;
            Ok(Some(v))
        }
        Some(_) => Err(bad(format!("`{key}` must be a number"))),
    }
}

fn flag(p: &Value, key: &str, default: bool) -> Result<bool> {
    match p.get(key) {
        None => Ok(default),
        Some(Value::Bool(v)) => Ok(*v),
        Some(_) => Err(bad(format!("`{key}` must be true or false"))),
    }
}

/// `#rrggbb`, or a JSON array of three components. Components above 1 are 0..255.
fn color_of(p: &Value) -> Result<Color> {
    match p.get("color") {
        None => Ok(Color::rgb(1.0, 0.0, 0.0)),
        Some(Value::String(s)) => {
            let c = crate::commands::parse_hex(s).ok_or_else(|| bad("color must be #rrggbb"))?;
            Ok(Color::rgb(c[0], c[1], c[2]))
        }
        Some(Value::Array(a)) if a.len() >= 3 => {
            let mut v = [0.0f32; 3];
            for (i, slot) in v.iter_mut().enumerate() {
                let n = a.get(i).and_then(Value::as_f64).filter(|n| n.is_finite()).ok_or_else(|| bad("color components must be numbers"))?;
                *slot = n as f32;
            }
            if v.iter().any(|c| *c > 1.0) {
                if v.iter().any(|c| !(0.0..=255.0).contains(c)) {
                    return Err(bad("color components must be 0..1 or 0..255"));
                }
                v = v.map(|c| c / 255.0);
            } else if v.iter().any(|c| *c < 0.0) {
                return Err(bad("color components must be 0..1 or 0..255"));
            }
            Ok(Color::rgb(v[0], v[1], v[2]))
        }
        Some(_) => Err(bad("color must be #rrggbb or [r, g, b]")),
    }
}

fn name_of(p: &Value) -> Result<String> {
    match p.get("name") {
        None => Ok(DEFAULT_NAME.to_string()),
        Some(Value::String(s)) if s.is_empty() => Err(bad("name must not be empty")),
        Some(Value::String(s)) if s.chars().count() > MAX_NAME_CHARS => Err(bad(format!("name must be at most {MAX_NAME_CHARS} characters"))),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(bad("name must be a string")),
    }
}

/// Active-layer transparency over the canvas: 0 where the layer is clear, 1 where a layer
/// without pixels (an adjustment or fill) covers the canvas.
fn transparency(doc: &photocraft_doc::Document, layer: photocraft_doc::LayerId, n: usize) -> Result<Vec<f32>> {
    let area = doc.bounds();
    let layer = doc.layer(layer).ok_or(EngineError::NoLayer(layer))?;
    match layer.surface() {
        Some(surf) => {
            let mut px = vec![[0.0f32; 4]; n];
            surf.read_rgba_into(area, &mut px);
            Ok(px.into_iter().map(|p| p[3]).collect())
        }
        None => Ok(vec![1.0; n]),
    }
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let contract = number(p, "contract")?.unwrap_or(f64::from(DEFAULT_CONTRACT)) as f32;
    if !(0.0..=MAX_CONTRACT).contains(&contract) {
        return Err(bad(format!("contract must be a number in 0..={MAX_CONTRACT}")));
    }
    let name = name_of(p)?;
    let color = color_of(p)?;
    let solidity_pct = number(p, "solidity")?.unwrap_or(DEFAULT_SOLIDITY);
    if !(0.0..=100.0).contains(&solidity_pct) {
        return Err(bad("solidity must be a number in 0..=100"));
    }
    let solidity = (solidity_pct as f32) / 100.0;
    let at_bounds = flag(p, "applyAtCanvasBounds", false)?;

    let i = s.edit("DTF Spot Channel", |doc, active| {
        let id = active.ok_or_else(|| EngineError::Other("transparency needs a layer".into()))?;
        let area = doc.bounds();
        let (w, h) = (area.width() as usize, area.height() as usize);
        let n = w.checked_mul(h).ok_or_else(|| bad("the canvas is too large"))?;
        let mut mask = transparency(doc, id, n)?;
        if contract > 0.0 {
            mask = crate::selection_cmds::modify_mask(&mask, w, h, "contract", contract, at_bounds).map_err(bad)?;
        }
        if mask.len() != n {
            return Err(bad("the contracted selection does not cover the canvas"));
        }
        doc.selection = sel::combine(None, &mask, area, SelectionMode::Replace);
        let fmt = crate::channel_cmds::channel_format(doc);
        let surface = crate::channel_cmds::plane_surface(&mask, area, fmt);
        doc.channels.push(AlphaChannel { spot: Some((color, solidity)), ..AlphaChannel::new(name.clone(), surface) });
        Ok(doc.channels.len() - 1)
    })?;
    if let Some(st) = s.active_mut() {
        st.channel_view.set_alpha_visible(i, true);
        crate::channel_cmds::touch(st);
    }
    let selected = s.active().is_some_and(|st| st.doc.selection.is_some());
    Ok(json!({ "channel": i, "name": name, "solidity": solidity_pct, "selected": selected }))
}

/// The three-step action from the DTF set, with the screenshot's values.
pub fn builtin_action() -> Action {
    Action {
        name: ACTION_NAME.into(),
        steps: vec![
            ("select.loadSelection".into(), json!({"channel": "transparency"})),
            ("select.modify.contract".into(), json!({"radius": 1, "applyAtCanvasBounds": false})),
            ("channel.newSpot".into(), json!({"name": DEFAULT_NAME, "color": DEFAULT_COLOR, "solidity": DEFAULT_SOLIDITY, "fromSelection": true})),
        ],
    }
}

/// Append the built-in DTF action when the list doesn't already contain that name.
pub fn ensure_preset(s: &mut Session) {
    if s.actions.list.iter().any(|a| a.name == ACTION_NAME) {
        return;
    }
    s.actions.list.push(builtin_action());
    s.actions.touch();
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "DTF Spot Channel…",
        menu: &["Image"],
        shortcut: None,
        params: r##"{"contract":0..500=1,"name":str="Spot Color 1","color":"#rrggbb"|[r,g,b]="#ff0000","solidity":0..100=60,"applyAtCanvasBounds":bool=false} → {channel,name,solidity,selected}"##,
        enabled,
        journal: true,
        run,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_geom::Rect;

    fn paint_rect(s: &mut Session, rect: Rect) {
        s.edit("paint", |doc, active| {
            let id = active.ok_or(EngineError::NoDocument)?;
            let fmt = doc.pixel_format();
            let px = photocraft_raster::from_rgba(&fmt, [0.2, 0.4, 0.6, 1.0]);
            doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?.surface_mut().ok_or_else(|| EngineError::Other("no pixels".into()))?.fill_rect(rect, &px);
            Ok(())
        })
        .unwrap();
    }

    fn channel_at(s: &Session, x: i32, y: i32) -> f32 {
        let doc = &s.active().unwrap().doc;
        let ch = doc.channels.last().unwrap();
        let px = ch.surface.read_region(Rect::new(x, y, x + 1, y + 1));
        px.first().copied().unwrap_or(0.0)
    }

    #[test]
    fn contracts_an_inset_shape_by_one_pixel() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8, "background": "transparent"})).unwrap();
        paint_rect(&mut s, Rect::new(2, 2, 6, 6));
        let r = s.execute(CMD, json!({})).unwrap();
        assert_eq!(r["name"], DEFAULT_NAME);
        assert_eq!(r["solidity"], DEFAULT_SOLIDITY);
        assert_eq!(r["selected"], true);
        let doc = &s.active().unwrap().doc;
        assert_eq!(doc.channels.len(), 1);
        let ch = &doc.channels[0];
        assert_eq!(ch.name, DEFAULT_NAME);
        let (ink, sol) = ch.spot.unwrap();
        assert_eq!(ink.to_rgb(), [1.0, 0.0, 0.0]);
        assert!((sol - 0.6).abs() < 1e-5);
        // The opaque rect was (2,2)-(6,6). Contracting by 1 drops the outer pixel.
        assert_eq!(channel_at(&s, 2, 3), 0.0);
        assert_eq!(channel_at(&s, 3, 3), 1.0);
        assert_eq!(channel_at(&s, 4, 4), 1.0);
        assert_eq!(channel_at(&s, 5, 4), 0.0);
        assert!(s.active().unwrap().channel_view.alpha_shown(0));
        assert_eq!(s.active().unwrap().history.undo_label(), Some("DTF Spot Channel"));
    }

    #[test]
    fn canvas_bounds_option_matches_select_modify() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 6, "height": 4, "background": "white"})).unwrap();
        // A locked white background is opaque to the canvas edge.
        s.execute(CMD, json!({"applyAtCanvasBounds": false, "name": "Edge"})).unwrap();
        assert_eq!(channel_at(&s, 0, 0), 1.0);
        assert_eq!(channel_at(&s, 5, 3), 1.0);
        s.execute("channel.delete", json!({"channel": "Edge"})).unwrap();
        s.execute(CMD, json!({"applyAtCanvasBounds": true, "name": "Inset"})).unwrap();
        assert_eq!(channel_at(&s, 0, 0), 0.0);
        assert_eq!(channel_at(&s, 1, 1), 1.0);
    }

    #[test]
    fn custom_color_solidity_and_contract_are_honoured() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8, "background": "transparent"})).unwrap();
        paint_rect(&mut s, Rect::new(1, 1, 7, 7));
        s.execute(CMD, json!({"contract": 2, "name": "White", "color": [255, 0, 128], "solidity": 80})).unwrap();
        let ch = &s.active().unwrap().doc.channels[0];
        assert_eq!(ch.name, "White");
        let (ink, sol) = ch.spot.unwrap();
        assert!((ink.to_rgb()[0] - 1.0).abs() < 1e-5);
        assert!((ink.to_rgb()[2] - 128.0 / 255.0).abs() < 1e-5);
        assert!((sol - 0.8).abs() < 1e-5);
        // Two pixels in from the rect's left edge (x = 1).
        assert_eq!(channel_at(&s, 2, 3), 0.0);
        assert_eq!(channel_at(&s, 3, 3), 1.0);
    }

    #[test]
    fn zero_contract_keeps_the_transparency_edge() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 6, "height": 6, "background": "transparent"})).unwrap();
        paint_rect(&mut s, Rect::new(2, 2, 5, 5));
        s.execute(CMD, json!({"contract": 0})).unwrap();
        assert_eq!(channel_at(&s, 2, 2), 1.0);
        assert_eq!(channel_at(&s, 1, 2), 0.0);
    }

    #[test]
    fn bad_params_do_not_panic() {
        let mut s = Session::new();
        assert!(s.execute(CMD, json!({})).is_err());
        s.execute("file.new", json!({"width": 4, "height": 4, "background": "transparent"})).unwrap();
        for p in [
            json!(null),
            json!({"contract": -1}),
            json!({"contract": 501}),
            json!({"contract": "1"}),
            json!({"solidity": 101}),
            json!({"solidity": -0.1}),
            json!({"solidity": false}),
            json!({"color": "red"}),
            json!({"color": 1}),
            json!({"color": [1, 2]}),
            json!({"name": ""}),
            json!({"name": 3}),
            json!({"applyAtCanvasBounds": "no"}),
        ] {
            assert!(s.execute(CMD, p).is_err(), "should reject");
        }
        assert!(s.active().unwrap().doc.channels.is_empty());
    }

    #[test]
    fn builtin_action_matches_the_command_defaults() {
        let prep = |s: &mut Session| {
            s.execute("file.new", json!({"width": 8, "height": 8, "background": "transparent"})).unwrap();
            paint_rect(s, Rect::new(2, 2, 6, 6));
        };
        let mut via_cmd = Session::new();
        prep(&mut via_cmd);
        via_cmd.execute(CMD, json!({})).unwrap();
        let mut via_action = Session::new();
        prep(&mut via_action);
        ensure_preset(&mut via_action);
        via_action.execute("actions.play", json!({"action": ACTION_NAME})).unwrap();
        let a = &via_cmd.active().unwrap().doc.channels[0];
        let b = &via_action.active().unwrap().doc.channels[0];
        assert_eq!(a.name, b.name);
        assert_eq!(a.spot, b.spot);
        let area = via_cmd.active().unwrap().doc.bounds();
        assert_eq!(a.surface.to_interleaved(area), b.surface.to_interleaved(area));
    }

    #[test]
    fn psd_and_tiff_keep_the_spot_channel() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8, "background": "transparent"})).unwrap();
        paint_rect(&mut s, Rect::new(2, 2, 6, 6));
        s.execute(CMD, json!({})).unwrap();
        let doc = s.active().unwrap().doc.clone();
        let psd = photocraft_io::export(&doc, "spot.psd", &photocraft_io::ExportOptions::default()).unwrap();
        let back = photocraft_io::import("spot.psd", &psd.bytes).unwrap().document;
        let ch = &back.channels[0];
        assert_eq!(ch.name, DEFAULT_NAME);
        let (ink, sol) = ch.spot.expect("spot");
        assert_eq!(ink.to_rgb(), [1.0, 0.0, 0.0]);
        assert!((sol - 0.6).abs() < 1e-4);
        let area = doc.bounds();
        assert_eq!(ch.surface.to_interleaved(area), doc.channels[0].surface.to_interleaved(area));

        for (name, layers) in [("flat.tif", false), ("layered.tif", true)] {
            let opts = photocraft_io::ExportOptions { tiff_layers: layers, ..photocraft_io::ExportOptions::default() };
            let tiff = photocraft_io::export(&doc, name, &opts).unwrap();
            assert!(!tiff.warnings.iter().any(|w| w.contains("not written")), "{name}: {:?}", tiff.warnings);
            let back = photocraft_io::import(name, &tiff.bytes).unwrap().document;
            let ch = back.channels.iter().find(|c| c.name == DEFAULT_NAME).unwrap_or_else(|| panic!("{name}: channels {:?}", back.channels.iter().map(|c| &c.name).collect::<Vec<_>>()));
            let (ink, sol) = ch.spot.expect("spot");
            assert_eq!(ink.to_rgb(), [1.0, 0.0, 0.0], "{name}");
            assert!((sol - 0.6).abs() < 1e-4, "{name}");
            assert_eq!(ch.surface.to_interleaved(area), doc.channels[0].surface.to_interleaved(area), "{name}");
        }
    }
}
