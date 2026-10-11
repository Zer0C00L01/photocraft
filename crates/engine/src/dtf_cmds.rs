//! Image › DTF Spot Channel, and the built-in Actions preset "DTF".
//!
//! A direct-to-film print file needs a spot channel of the artwork, shrunk by a pixel so the
//! white underbase does not peek past the colour. Photoshop's recorded action loads transparency,
//! contracts by 1px without treating the canvas edge as a selection edge, then makes a spot
//! channel named "Spot Color 1" (RGB red, solidity 60). [`run`] does that as one history step.
//! The Actions panel preset is that same command, in a folder also named "DTF", so the step's
//! numbers and text can be edited. The last successful settings are stored with the other
//! remembered dialogs and become the next dialog's defaults and, while the preset is still that
//! one step, the values Play uses.

use photocraft_color::{Color, PixelFormat, SampleType, write_sample};
use photocraft_doc::{AlphaChannel, Document, LayerId};
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::actions_cmds::Action;
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// Name of the built-in action in the Actions panel.
pub const ACTION_NAME: &str = "DTF";
/// Folder the built-in action is filed under.
pub const SET_NAME: &str = "DTF";
/// `source` value: transparency of the active layer.
pub const SOURCE_LAYER: &str = "layer";
/// `source` value: alpha of the composite of every visible layer.
pub const SOURCE_COMPOSITE: &str = "composite";
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

fn source_of(p: &Value) -> Result<bool> {
    match p.get("source") {
        None => Ok(false),
        Some(Value::String(s)) if s == SOURCE_LAYER => Ok(false),
        Some(Value::String(s)) if s == SOURCE_COMPOSITE => Ok(true),
        Some(_) => Err(bad("source must be \"layer\" or \"composite\"")),
    }
}

fn hex_color(color: Color) -> String {
    let [r, g, b] = color.to_rgb();
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// The dialog and the built-in action start from these values.
pub fn default_params() -> Value {
    json!({
        "source": SOURCE_LAYER,
        "contract": DEFAULT_CONTRACT,
        "name": DEFAULT_NAME,
        "color": DEFAULT_COLOR,
        "solidity": DEFAULT_SOLIDITY,
        "applyAtCanvasBounds": false,
    })
}

/// Fields for the Image › DTF Spot Channel dialog, prefilled from the last successful run.
pub fn dialog_fields(saved: Option<&Value>) -> Value {
    let mut fields = default_params();
    if let Some(saved) = saved.and_then(Value::as_object)
        && let Some(fields_obj) = fields.as_object_mut()
    {
        for (key, value) in saved {
            if key.starts_with("__") {
                continue;
            }
            fields_obj.insert(key.clone(), value.clone());
        }
        let source = fields_obj.get("source").and_then(Value::as_str).unwrap_or("");
        if source != SOURCE_LAYER && source != SOURCE_COMPOSITE {
            fields_obj.insert("source".into(), json!(SOURCE_LAYER));
        }
        if !fields_obj.get("applyAtCanvasBounds").is_some_and(Value::is_boolean) {
            fields_obj.insert("applyAtCanvasBounds".into(), json!(false));
        }
    }
    fields
}

/// Rows of alpha handled together. The contract window adds a halo, so this stays a small
/// multiple of the strip rather than the whole canvas.
fn strip_rows(halo: usize) -> usize {
    256.max(halo)
}

/// Working set of one distance-transform window (bool seeds, column distances, the f32 result,
/// and the alpha the window reads). Past this the edit is refused instead of aborting.
const MAX_WINDOW_BYTES: usize = 512 * 1024 * 1024;

/// Where the transparency mask comes from. Reads are limited to the rows a strip needs.
enum AlphaSrc<'a> {
    /// A layer with no pixels (an adjustment or fill) covers the canvas.
    Solid,
    Layer(&'a Surface),
    Composite(&'a Document),
}

impl AlphaSrc<'_> {
    fn alpha_rows(&self, x0: i32, y0: i32, x1: i32, y1: i32, dst: &mut [f32]) -> Result<()> {
        let width = x1.saturating_sub(x0);
        let height = y1.saturating_sub(y0);
        if width < 0 || height < 0 {
            return Err(bad("the canvas is too large"));
        }
        let n = (width as usize).checked_mul(height as usize).ok_or_else(|| bad("the canvas is too large"))?;
        if dst.len() != n {
            return Err(bad("the composite does not cover the canvas"));
        }
        match self {
            AlphaSrc::Solid => dst.fill(1.0),
            AlphaSrc::Layer(surf) => {
                let mut px = crate::allocation::filled(n, [0.0f32; 4], "DTF transparency")?;
                surf.read_rgba_into(Rect::new(x0, y0, x1, y1), &mut px);
                for (d, p) in dst.iter_mut().zip(px.iter()) {
                    *d = p[3];
                }
            }
            AlphaSrc::Composite(doc) => {
                let buf = photocraft_compose::render(doc, Rect::new(x0, y0, x1, y1));
                if buf.px.len() != n {
                    return Err(bad("the composite does not cover the canvas"));
                }
                for (d, p) in dst.iter_mut().zip(buf.px.iter()) {
                    *d = p[3];
                }
            }
        }
        Ok(())
    }

    /// Any outermost pixel with alpha above 0, the same test as Select › Modify's edge pad.
    fn touches_edge(&self, w: usize, h: usize) -> Result<bool> {
        if w == 0 || h == 0 {
            return Ok(false);
        }
        let selected = |row: &[f32]| row.iter().any(|v| *v > 0.0);
        let mut row = crate::allocation::filled(w, 0.0, "DTF transparency")?;
        self.alpha_rows(0, 0, w as i32, 1, &mut row)?;
        if selected(&row) {
            return Ok(true);
        }
        if h > 1 {
            self.alpha_rows(0, (h - 1) as i32, w as i32, h as i32, &mut row)?;
            if selected(&row) {
                return Ok(true);
            }
        }
        let mut col = crate::allocation::filled(h, 0.0, "DTF transparency")?;
        self.alpha_rows(0, 0, 1, h as i32, &mut col)?;
        if selected(&col) {
            return Ok(true);
        }
        if w > 1 {
            self.alpha_rows((w - 1) as i32, 0, w as i32, h as i32, &mut col)?;
            if selected(&col) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn source<'a>(doc: &'a Document, active: Option<LayerId>, composite: bool) -> Result<AlphaSrc<'a>> {
    if composite {
        return Ok(AlphaSrc::Composite(doc));
    }
    let id = active.ok_or_else(|| EngineError::Other("transparency needs a layer".into()))?;
    let layer = doc.layer(id).ok_or(EngineError::NoLayer(id))?;
    Ok(match layer.surface() {
        Some(surf) => AlphaSrc::Layer(surf),
        None => AlphaSrc::Solid,
    })
}

/// 8-bit selection value, matching `photocraft_algo::selection`'s coverage (NaN → 0).
fn coverage8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Select › Modify › Contract on one pixel: invert, expand by the Euclidean distance to the
/// nearest pixel at or under half coverage, invert back. `f32::max` keeps NaN behaviour.
fn contracted(alpha: f32, distance: f32, radius: f32) -> f32 {
    let inv = 1.0 - alpha;
    let grown = inv.max((radius + 1.0 - distance).clamp(0.0, 1.0));
    1.0 - grown
}

fn too_large() -> EngineError {
    bad("the selection is too large to modify")
}

fn commit_strip(spot: &mut Surface, sel: &mut Surface, any: &mut bool, y0: i32, width: i32, values: &[f32], sample: SampleType) -> Result<()> {
    let rows = if width <= 0 { 0 } else { values.len() / width as usize };
    let rect = Rect::new(0, y0, width, y0.saturating_add(rows as i32));
    let bpp = sample.bytes().max(1);
    let spot_len = values.len().checked_mul(bpp).ok_or_else(|| bad("the canvas is too large"))?;
    let mut spot_bytes = crate::allocation::filled(spot_len, 0u8, "DTF spot channel")?;
    let mut cov = crate::allocation::filled(values.len(), 0u8, "DTF selection")?;
    for (i, v) in values.iter().enumerate() {
        // `plane_surface` clamps before quantizing; NaN stays NaN and float channels keep it.
        write_sample(&mut spot_bytes, sample, i, v.clamp(0.0, 1.0));
        let b = coverage8(*v);
        if let Some(slot) = cov.get_mut(i) {
            *slot = b;
        }
        if b != 0 {
            *any = true;
        }
    }
    spot.write_interleaved_sparse(rect, &spot_bytes)?;
    sel.write_interleaved_sparse(rect, &cov)?;
    Ok(())
}

#[derive(Clone, Copy)]
struct Contract<'a> {
    src: &'a AlphaSrc<'a>,
    radius: f32,
    at_bounds: bool,
    sample: SampleType,
}

/// Contract one row strip at a time, matching [`crate::selection_cmds::modify_mask`].
///
/// The full-canvas distance transform only affects a pixel when the nearest half-coverage seed
/// is strictly inside radius `r + 1`. Those seeds lie in a Chebyshev halo of `ceil(r + 1)` rows,
/// so a window of the (optionally edge-padded) mask plus that halo reproduces the cropped
/// contract. Padding matches Modify: replicate the edge unless "apply at canvas bounds" treats
/// the outside as empty.
fn contract_strips(job: &Contract<'_>, spot: &mut Surface, sel: &mut Surface, any: &mut bool, w: usize, h: usize) -> Result<()> {
    let Contract { src, radius, at_bounds, sample } = *job;
    let touches = src.touches_edge(w, h)?;
    let pad = if touches { px_count(radius.ceil())?.saturating_add(2) } else { 0 };
    let pw = w.checked_add(pad.saturating_mul(2)).ok_or_else(too_large)?;
    let ph = h.checked_add(pad.saturating_mul(2)).ok_or_else(too_large)?;
    if pw.checked_mul(ph).is_none_or(|n| n > (1 << 30)) {
        return Err(too_large());
    }
    let halo = px_count((radius + 1.0).ceil())?;
    let outside_empty = at_bounds;
    let mut y0 = 0usize;
    while y0 < h {
        let mut rows = strip_rows(halo).min(h - y0);
        loop {
            let wh = rows.checked_add(halo.saturating_mul(2)).ok_or_else(too_large)?;
            if window_ok(pw, wh).is_ok() || rows == 1 {
                if window_ok(pw, wh).is_err() {
                    return Err(too_large());
                }
                break;
            }
            rows = (rows / 2).max(1);
        }
        let y1 = y0.saturating_add(rows).min(h);
        let win0 = y0.saturating_add(pad).saturating_sub(halo);
        let win1 = y1.saturating_add(pad).saturating_add(halo).min(ph);
        let wh = win1.saturating_sub(win0);
        if wh == 0 {
            return Err(too_large());
        }
        window_ok(pw, wh)?;
        let (cy0, cy1) = alpha_span(win0, win1, pad, h, outside_empty, y0, y1);
        let span = cy1.saturating_sub(cy0).checked_mul(w).ok_or_else(too_large)?;
        let alpha = if span == 0 {
            Vec::new()
        } else {
            let mut alpha = crate::allocation::filled(span, 0.0, "DTF transparency")?;
            src.alpha_rows(0, cy0 as i32, w as i32, cy1 as i32, &mut alpha)?;
            alpha
        };
        let at = |x: usize, y: usize| -> f32 {
            if y < cy0 || y >= cy1 || x >= w {
                return 0.0;
            }
            alpha.get((y - cy0) * w + x).copied().unwrap_or(0.0)
        };
        let padded = |px: usize, py: usize| -> f32 {
            let inside_x = px >= pad && px < pad + w;
            let inside_y = py >= pad && py < pad + h;
            if pad > 0 && outside_empty && (!inside_x || !inside_y) {
                return 0.0;
            }
            let x = px.saturating_sub(pad).min(w.saturating_sub(1));
            let y = py.saturating_sub(pad).min(h.saturating_sub(1));
            at(x, y)
        };
        let nseeds = pw.checked_mul(wh).ok_or_else(too_large)?;
        let mut seeds = crate::allocation::filled(nseeds, false, "DTF contract")?;
        for py in win0..win1 {
            for px in 0..pw {
                // A contract seed is a pixel whose inverted coverage is at least one half
                // (`expand` of `1 - alpha`), i.e. alpha <= 0.5. NaN is not a seed.
                let seed = padded(px, py) <= 0.5;
                if let Some(slot) = seeds.get_mut((py - win0) * pw + px) {
                    *slot = seed;
                }
            }
        }
        let distances = photocraft_algo::selection::edt(&seeds, pw, wh);
        if distances.len() != seeds.len() {
            return Err(too_large());
        }
        drop(seeds);
        let mut out = crate::allocation::filled(w.checked_mul(rows).ok_or_else(too_large)?, 0.0, "DTF contract")?;
        for y in y0..y1 {
            for x in 0..w {
                let py = y.saturating_add(pad);
                let px = x.saturating_add(pad);
                let distance = if (win0..win1).contains(&py) && px < pw { distances.get((py - win0) * pw + px).copied().unwrap_or(1.0e10) } else { 1.0e10 };
                if let Some(slot) = out.get_mut((y - y0) * w + x) {
                    *slot = contracted(at(x, y), distance, radius);
                }
            }
        }
        drop(distances);
        let y_i = i32::try_from(y0).map_err(|_| too_large())?;
        let width = i32::try_from(w).map_err(|_| too_large())?;
        commit_strip(spot, sel, any, y_i, width, &out, sample)?;
        y0 = y1;
    }
    Ok(())
}

fn px_count(v: f32) -> Result<usize> {
    if !v.is_finite() || v < 0.0 || v > usize::MAX as f32 {
        return Err(too_large());
    }
    Ok(v as usize)
}

fn window_ok(pw: usize, wh: usize) -> Result<()> {
    let n = pw.checked_mul(wh).ok_or_else(too_large)?;
    if n.checked_mul(32).is_none_or(|bytes| bytes > MAX_WINDOW_BYTES) || u32::try_from(pw).is_err() || u32::try_from(wh).is_err() {
        return Err(too_large());
    }
    Ok(())
}

/// Canvas rows the padded window reads, always including the output strip `[y0, y1)`.
fn alpha_span(win0: usize, win1: usize, pad: usize, h: usize, outside_empty: bool, y0: usize, y1: usize) -> (usize, usize) {
    if h == 0 || win0 >= win1 {
        return (y0.min(h), y1.min(h));
    }
    let (a, b) = if outside_empty {
        let py0 = win0.max(pad);
        let py1 = win1.min(pad.saturating_add(h));
        if py0 >= py1 { (y0, y0) } else { (py0 - pad, py1 - pad) }
    } else {
        let lo = win0.saturating_sub(pad).min(h - 1);
        let hi = (win1 - 1).saturating_sub(pad).min(h - 1);
        (lo, hi + 1)
    };
    (a.min(y0), b.max(y1).min(h))
}

/// Spot channel and selection for Image › DTF Spot Channel, without a full-canvas float mask.
fn spot_channel(doc: &Document, active: Option<LayerId>, composite: bool, contract: f32, at_bounds: bool) -> Result<(Option<Surface>, Surface)> {
    let area = doc.bounds();
    let w_i = i32::try_from(area.width()).map_err(|_| bad("the canvas is too large"))?;
    let h_i = i32::try_from(area.height()).map_err(|_| bad("the canvas is too large"))?;
    let (w, h) = (w_i as usize, h_i as usize);
    let fmt = crate::channel_cmds::channel_format(doc);
    let mut spot = Surface::new(fmt);
    let mut sel = Surface::new(PixelFormat::GRAY8);
    if w == 0 || h == 0 {
        return Ok((None, spot));
    }
    let src = source(doc, active, composite)?;
    let mut any = false;
    if contract > 0.0 {
        let job = Contract { src: &src, radius: contract, at_bounds, sample: fmt.sample };
        contract_strips(&job, &mut spot, &mut sel, &mut any, w, h)?;
    } else {
        let mut y = 0usize;
        while y < h {
            let rows = strip_rows(0).min(h - y);
            let mut alpha = crate::allocation::filled(w.checked_mul(rows).ok_or_else(|| bad("the canvas is too large"))?, 0.0, "DTF transparency")?;
            let y_i = i32::try_from(y).map_err(|_| bad("the canvas is too large"))?;
            let y1_i = i32::try_from(y.saturating_add(rows)).map_err(|_| bad("the canvas is too large"))?;
            src.alpha_rows(0, y_i, w_i, y1_i, &mut alpha)?;
            commit_strip(&mut spot, &mut sel, &mut any, y_i, w_i, &alpha, fmt.sample)?;
            y += rows;
        }
    }
    Ok((any.then_some(sel), spot))
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let composite = source_of(p)?;
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
        let (selection, surface) = spot_channel(doc, *active, composite, contract, at_bounds)?;
        doc.selection = selection;
        doc.channels.push(AlphaChannel { spot: Some((color, solidity)), ..AlphaChannel::new(name.clone(), surface) });
        Ok(doc.channels.len().saturating_sub(1))
    })?;
    if let Some(st) = s.active_mut() {
        st.channel_view.set_alpha_visible(i, true);
        crate::channel_cmds::touch(st);
    }
    let selected = s.active().is_some_and(|st| st.doc.selection.is_some());
    let params = json!({
        "source": if composite { SOURCE_COMPOSITE } else { SOURCE_LAYER },
        "contract": contract,
        "name": name,
        "color": hex_color(color),
        "solidity": solidity_pct,
        "applyAtCanvasBounds": at_bounds,
    });
    remember(s, params);
    Ok(json!({ "channel": i, "name": name, "solidity": solidity_pct, "selected": selected, "source": if composite { SOURCE_COMPOSITE } else { SOURCE_LAYER } }))
}

fn remember(s: &mut Session, params: Value) {
    s.edit_prefs(|prefs| {
        prefs.dialogs.insert(CMD.into(), params.clone());
    });
    let idx = s.actions.list.iter().position(|a| a.name == ACTION_NAME && a.steps.len() == 1 && a.steps.first().is_some_and(|(id, _)| id == CMD));
    if let Some(idx) = idx
        && let Some(step) = s.actions.list.get_mut(idx).and_then(|a| a.steps.first_mut())
    {
        step.1 = params;
        s.actions.touch();
    }
}

/// The Actions panel edited the DTF step: the next dialog should open on those values.
pub fn note_edited_step(s: &mut Session, idx: usize) {
    let Some(action) = s.actions.list.get(idx) else { return };
    if action.name != ACTION_NAME || action.steps.len() != 1 {
        return;
    }
    let Some((id, params)) = action.steps.first() else { return };
    if id != CMD {
        return;
    }
    let params = params.clone();
    s.edit_prefs(|prefs| {
        prefs.dialogs.insert(CMD.into(), params);
    });
}

fn params_for_preset(s: &Session) -> Value {
    dialog_fields(s.prefs().dialogs.get(CMD))
}

/// The one-step action, with the screenshot's values, filed in the DTF folder.
pub fn builtin_action() -> Action {
    Action::new(ACTION_NAME.into(), vec![(CMD.into(), default_params())])
}

/// The previous preset was the three Photoshop commands. Fold those into one `image.dtf` step,
/// keeping the contract, name, colour and solidity they recorded.
fn legacy_params(action: &Action) -> Option<Value> {
    if action.steps.len() != 3 {
        return None;
    }
    let (load, contract, spot) = (&action.steps[0], &action.steps[1], &action.steps[2]);
    if load.0 != "select.loadSelection" || contract.0 != "select.modify.contract" || spot.0 != "channel.newSpot" {
        return None;
    }
    if load.1.get("channel").and_then(Value::as_str) != Some("transparency") {
        return None;
    }
    let mut params = default_params();
    let obj = params.as_object_mut()?;
    if let Some(radius) = contract.1.get("radius") {
        obj.insert("contract".into(), radius.clone());
    }
    if let Some(at) = contract.1.get("applyAtCanvasBounds") {
        obj.insert("applyAtCanvasBounds".into(), at.clone());
    }
    for key in ["name", "color", "solidity"] {
        if let Some(v) = spot.1.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    Some(params)
}

/// Append the built-in DTF action when the list doesn't already contain that name.
/// A saved three-step DTF preset becomes the one `image.dtf` step, in the DTF folder.
pub fn ensure_preset(s: &mut Session) {
    s.actions.align_folders();
    if let Some(idx) = s.actions.list.iter().position(|a| a.name == ACTION_NAME) {
        if let Some(params) = legacy_params(&s.actions.list[idx]) {
            s.actions.list[idx].steps = vec![(CMD.into(), params)];
            if s.actions.folder(idx).is_empty() {
                s.actions.folders[idx] = SET_NAME.into();
            }
            let _ = crate::actions_cmds::ensure_set(&mut s.actions, SET_NAME);
            s.actions.touch();
        }
        return;
    }
    let params = params_for_preset(s);
    let action = Action::new(ACTION_NAME.into(), vec![(CMD.into(), params)]);
    let _ = crate::actions_cmds::push_action(s, action, SET_NAME);
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "DTF Spot Channel…",
        menu: &["Image"],
        shortcut: None,
        params: r##"{"source":"layer"|"composite"="layer","contract":0..500=1,"name":str="Spot Color 1","color":"#rrggbb"|[r,g,b]="#ff0000","solidity":0..100=60,"applyAtCanvasBounds":bool=false} → {channel,name,solidity,selected,source}. source "layer" is the active layer; "composite" is every visible layer. The values used are remembered for the next dialog and, while the DTF preset is still this one step, for Actions playback."##,
        enabled,
        journal: true,
        run,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_algo::selection::{self as sel, SelectionMode};
    use photocraft_color::ColorMode;
    use photocraft_doc::LayerId;
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
            json!({"source": "both"}),
            json!({"source": 1}),
        ] {
            assert!(s.execute(CMD, p.clone()).is_err(), "should reject {p}");
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
        assert_eq!(via_action.actions.folder(0), SET_NAME);
        assert_eq!(via_action.actions.list[0].steps.len(), 1);
        assert_eq!(via_action.actions.list[0].steps[0].0, CMD);
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
            let ch = back
                .channels
                .iter()
                .find(|c| c.name == DEFAULT_NAME)
                .unwrap_or_else(|| panic!("{name}: channels {:?}", back.channels.iter().map(|c| &c.name).collect::<Vec<_>>()));
            let (ink, sol) = ch.spot.expect("spot");
            assert_eq!(ink.to_rgb(), [1.0, 0.0, 0.0], "{name}");
            assert!((sol - 0.6).abs() < 1e-4, "{name}");
            assert_eq!(ch.surface.to_interleaved(area), doc.channels[0].surface.to_interleaved(area), "{name}");
        }
    }

    #[test]
    fn composite_includes_every_visible_layer_and_skips_hidden_ones() {
        let prep = || {
            let mut s = Session::new();
            s.execute("file.new", json!({"width": 8, "height": 8, "background": "transparent"})).unwrap();
            paint_rect(&mut s, Rect::new(1, 1, 4, 4));
            let bottom = s.active().unwrap().doc.layers[0].id;
            s.execute("layer.new.layer", json!({})).unwrap();
            paint_rect(&mut s, Rect::new(4, 4, 7, 7));
            (s, bottom)
        };
        let (mut layer, _) = prep();
        layer.execute(CMD, json!({"contract": 0, "source": "layer", "name": "Layer"})).unwrap();
        assert_eq!(channel_at(&layer, 1, 1), 0.0, "the active layer does not cover the lower layer");
        assert_eq!(channel_at(&layer, 4, 4), 1.0);
        assert_eq!(layer.execute(CMD, json!({})).unwrap()["source"], "layer");

        let (mut both, bottom) = prep();
        both.execute(CMD, json!({"contract": 0, "source": "composite", "name": "Both"})).unwrap();
        assert_eq!(channel_at(&both, 1, 1), 1.0, "the lower layer is part of the composite");
        assert_eq!(channel_at(&both, 5, 5), 1.0);
        assert_eq!(channel_at(&both, 0, 0), 0.0);

        both.execute("channel.delete", json!({"channel": "Both"})).unwrap();
        both.execute("layer.setProps", json!({"layer": bottom.0, "visible": false})).unwrap();
        both.execute(CMD, json!({"contract": 0, "source": "composite", "name": "Top"})).unwrap();
        assert_eq!(channel_at(&both, 1, 1), 0.0, "a hidden layer is not in the composite");
        assert_eq!(channel_at(&both, 4, 4), 1.0);
    }

    #[test]
    fn last_settings_prefill_the_dialog_and_the_action() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 6, "height": 6, "background": "transparent"})).unwrap();
        paint_rect(&mut s, Rect::new(1, 1, 5, 5));
        ensure_preset(&mut s);
        s.execute(CMD, json!({"contract": 0, "name": "Varnish", "color": "#00ff00", "solidity": 40, "source": "composite", "applyAtCanvasBounds": true}))
            .unwrap();
        let saved = s.prefs().dialogs.get(CMD).expect("remembered");
        assert_eq!(saved["name"], "Varnish");
        assert_eq!(saved["source"], "composite");
        assert_eq!(saved["solidity"], json!(40.0));
        assert_eq!(saved["color"], "#00ff00");
        assert_eq!(saved["applyAtCanvasBounds"], json!(true));
        let fields = dialog_fields(Some(saved));
        assert_eq!(fields["name"], "Varnish");
        assert_eq!(fields["source"], "composite");
        assert_eq!(s.actions.list[0].steps[0].1["name"], "Varnish");
        assert_eq!(s.actions.list[0].steps[0].1["contract"], json!(0.0));

        s.execute("file.new", json!({"width": 6, "height": 6, "background": "transparent"})).unwrap();
        paint_rect(&mut s, Rect::new(0, 0, 6, 6));
        s.execute("actions.play", json!({"action": ACTION_NAME})).unwrap();
        let ch = s.active().unwrap().doc.channels.last().unwrap();
        assert_eq!(ch.name, "Varnish");
        assert!((ch.spot.unwrap().1 - 0.4).abs() < 1e-5);
        // Contract 0 on a full-canvas opaque layer keeps the edge.
        assert_eq!(channel_at(&s, 0, 0), 1.0);

        s.execute("actions.setStep", json!({"action": ACTION_NAME, "step": 0, "params": {"contract": 2, "name": "Gloss", "source": "layer", "solidity": 60, "color": "#ff0000", "applyAtCanvasBounds": false}})).unwrap();
        assert_eq!(s.prefs().dialogs.get(CMD).unwrap()["name"], "Gloss");
        assert_eq!(s.actions.list[0].steps[0].1["contract"], json!(2));
    }

    #[test]
    fn a_saved_three_step_preset_becomes_one_dtf_step_in_the_folder() {
        let mut s = Session::new();
        s.actions.list.push(Action::new(
            ACTION_NAME.into(),
            vec![
                ("select.loadSelection".into(), json!({"channel": "transparency"})),
                ("select.modify.contract".into(), json!({"radius": 2, "applyAtCanvasBounds": true})),
                ("channel.newSpot".into(), json!({"name": "White", "color": "#ffffff", "solidity": 80, "fromSelection": true})),
            ],
        ));
        ensure_preset(&mut s);
        assert_eq!(s.actions.list.len(), 1);
        assert_eq!(s.actions.folder(0), SET_NAME);
        assert!(s.actions.sets.iter().any(|n| n == SET_NAME));
        assert_eq!(s.actions.list[0].steps.len(), 1);
        assert_eq!(s.actions.list[0].steps[0].0, CMD);
        assert_eq!(s.actions.list[0].steps[0].1["name"], "White");
        assert_eq!(s.actions.list[0].steps[0].1["contract"], json!(2));
        assert_eq!(s.actions.list[0].steps[0].1["applyAtCanvasBounds"], json!(true));
        // A customized action that is no longer those three commands is left in place.
        s.actions.list[0].steps.push(("layer.new.layer".into(), json!({})));
        s.actions.folders[0].clear();
        ensure_preset(&mut s);
        assert_eq!(s.actions.list[0].steps.len(), 2);
        assert!(s.actions.folder(0).is_empty());
    }

    /// The strip path against the full-canvas contract (`modify_mask` + `plane_surface`).
    fn reference(doc: &photocraft_doc::Document, layer: Option<LayerId>, composite: bool, contract: f32, at_bounds: bool) -> (Option<Surface>, Surface) {
        let area = doc.bounds();
        let (w, h) = (area.width() as usize, area.height() as usize);
        let n = w * h;
        let mask = if composite {
            photocraft_compose::flatten(doc).px.into_iter().map(|p| p[3]).collect::<Vec<_>>()
        } else {
            match layer.and_then(|id| doc.layer(id)).and_then(|l| l.surface()) {
                Some(surf) => {
                    let mut px = vec![[0.0f32; 4]; n];
                    surf.read_rgba_into(area, &mut px);
                    px.into_iter().map(|p| p[3]).collect()
                }
                None => vec![1.0; n],
            }
        };
        let mask = if contract > 0.0 { crate::selection_cmds::modify_mask(&mask, w, h, "contract", contract, at_bounds).unwrap() } else { mask };
        let selection = sel::combine(None, &mask, area, SelectionMode::Replace);
        let spot = crate::channel_cmds::plane_surface(&mask, area, crate::channel_cmds::channel_format(doc));
        (selection, spot)
    }

    fn paint_alpha(s: &mut Session, w: i32, h: i32, pixel: impl Fn(i32, i32) -> f32) {
        s.edit("alpha", |doc, active| {
            let id = active.ok_or(EngineError::NoDocument)?;
            let fmt = doc.pixel_format();
            let ch = fmt.channels();
            let mut data = vec![0.0f32; w as usize * h as usize * ch];
            for y in 0..h {
                for x in 0..w {
                    let px = photocraft_raster::from_rgba(&fmt, [0.2, 0.4, 0.6, pixel(x, y)]);
                    let o = (y as usize * w as usize + x as usize) * ch;
                    data[o..o + ch].copy_from_slice(&px);
                }
            }
            doc.layer_mut(id)
                .ok_or(EngineError::NoLayer(id))?
                .surface_mut()
                .ok_or_else(|| EngineError::Other("no pixels".into()))?
                .write_region(Rect::new(0, 0, w, h), &data);
            Ok(())
        })
        .unwrap();
    }

    fn assert_matches(s: &Session, composite: bool, contract: f32, at_bounds: bool) {
        let st = s.active().unwrap();
        let area = st.doc.bounds();
        let (expect_sel, expect_spot) = reference(&st.doc, st.active_layer, composite, contract, at_bounds);
        let got = st.doc.channels.last().unwrap();
        assert_eq!(got.surface.to_interleaved(area), expect_spot.to_interleaved(area), "spot contract={contract} bounds={at_bounds} composite={composite}");
        match (&st.doc.selection, &expect_sel) {
            (None, None) => {}
            (Some(got), Some(expect)) => {
                assert_eq!(got.to_interleaved(area), expect.to_interleaved(area), "selection contract={contract} bounds={at_bounds} composite={composite}")
            }
            (got, expect) => panic!("selection presence contract={contract} bounds={at_bounds}: strip {}, full {}", got.is_some(), expect.is_some()),
        }
    }

    fn pattern(w: i32, h: i32) -> impl Fn(i32, i32) -> f32 {
        let boundary = strip_rows(2) as i32;
        move |x, y| {
            let levels = [0.0, 0.25, 0.5, 0.51, 1.0, 10.0 / 255.0, 128.0 / 255.0, 200.0 / 255.0];
            if x == 0 && y < 6 {
                return 1.0;
            }
            if y == h - 1 && x > w / 2 {
                return 1.0;
            }
            if x == w - 1 && (10..18).contains(&y) {
                return 0.51;
            }
            if (boundary - 5..boundary + 5).contains(&y) && (8..28).contains(&x) {
                return 1.0;
            }
            if (boundary * 2 - 3..boundary * 2 + 7).contains(&y) && (30..48).contains(&x) {
                return 0.5;
            }
            if (4..14).contains(&x) && (4..14).contains(&y) {
                return levels[((x + y * 3) as usize) % levels.len()];
            }
            let k = (x as u32).wrapping_mul(1664525).wrapping_add((y as u32).wrapping_mul(1013904223));
            if k.is_multiple_of(47) { levels[(k as usize) % levels.len()] } else { 0.0 }
        }
    }

    #[test]
    fn strip_contract_matches_the_full_mask_on_boundaries() {
        let w = 70i32;
        let h = strip_rows(2) as i32 * 2 + 40;
        let pixel = pattern(w, h);
        for contract in [0.0, 1.0, 3.0, 1.5, 2.5] {
            for at_bounds in [false, true] {
                let mut s = Session::new();
                s.execute("file.new", json!({"width": w, "height": h, "background": "transparent"})).unwrap();
                paint_alpha(&mut s, w, h, &pixel);
                s.execute(CMD, json!({"contract": contract, "applyAtCanvasBounds": at_bounds, "source": "layer"})).unwrap();
                assert_matches(&s, false, contract, at_bounds);
            }
        }
    }

    #[test]
    fn strip_contract_matches_on_clear_opaque_and_tiny_canvases() {
        for (w, h, alpha, contract, at_bounds) in
            [(1, 1, 1.0, 1.0, true), (1, 1, 1.0, 1.0, false), (3, 1, 1.0, 3.0, true), (8, 8, 0.0, 1.0, false), (8, 8, 1.0, 1.0, true), (8, 8, 1.0, 0.0, false)]
        {
            let mut s = Session::new();
            s.execute("file.new", json!({"width": w, "height": h, "background": "transparent"})).unwrap();
            paint_alpha(&mut s, w, h, |_, _| alpha);
            s.execute(CMD, json!({"contract": contract, "applyAtCanvasBounds": at_bounds})).unwrap();
            assert_matches(&s, false, contract, at_bounds);
        }
    }

    #[test]
    fn strip_composite_matches_flatten() {
        let w = 48i32;
        let h = strip_rows(4) as i32 + 20;
        let mut s = Session::new();
        s.execute("file.new", json!({"width": w, "height": h, "background": "transparent"})).unwrap();
        paint_alpha(&mut s, w, h, |x, y| if (2..20).contains(&x) && (strip_rows(4) as i32 - 3..strip_rows(4) as i32 + 3).contains(&y) { 1.0 } else { 0.0 });
        s.execute("layer.new.layer", json!({})).unwrap();
        paint_alpha(&mut s, w, h, |x, y| if (24..40).contains(&x) && (4..16).contains(&y) { 128.0 / 255.0 } else { 0.0 });
        s.execute(CMD, json!({"contract": 3, "source": "composite", "applyAtCanvasBounds": false})).unwrap();
        assert_matches(&s, true, 3.0, false);
    }

    #[test]
    fn strip_contract_matches_at_16_bit() {
        let w = 36i32;
        let h = strip_rows(2) as i32 + 8;
        let mut s = Session::new();
        s.execute("file.new", json!({"width": w, "height": h, "background": "transparent"})).unwrap();
        paint_alpha(&mut s, w, h, pattern(w, h));
        s.edit("depth", |doc, active| {
            let id = active.ok_or(EngineError::NoDocument)?;
            let layer = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
            let surf = layer.surface_mut().ok_or_else(|| EngineError::Other("no pixels".into()))?;
            let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::U16, true);
            *surf = surf.convert(fmt);
            doc.depth = SampleType::U16;
            Ok(())
        })
        .unwrap();
        s.execute(CMD, json!({"contract": 1, "applyAtCanvasBounds": true})).unwrap();
        assert_matches(&s, false, 1.0, true);
    }
}
