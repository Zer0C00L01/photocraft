//! Actions panel: record and replay command sequences.
//!
//! The list and the recording flag live on the engine session (`actions.record` / `stop` /
//! `play` / `list` / `get` / `delete` / `move`), so the panel, the CLI and MCP share them. This module
//! keeps only which row is selected and which rows are expanded.

use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use photocraft_engine::actions_cmds::{self, Action};

use crate::PhotocraftApp;
use crate::theme::Tokens;

/// How Play walks the steps. Accelerated runs straight through. Step waits for another
/// Play. Pause waits [`ActionsUi::pause_seconds`] between steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackMode {
    /// Run every included step without stopping.
    #[default]
    Accelerated,
    /// Run one step, then wait for Play.
    Step,
    /// Run one step, then wait [`ActionsUi::pause_seconds`].
    Pause,
}

/// Which row is selected and which rows are expanded. The action list itself is
/// [`photocraft_engine::Session::actions`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionsUi {
    pub selected: Option<usize>,
    pub selected_step: Option<usize>,
    pub expanded: Vec<bool>,
    /// Folder row that is selected. `Some("")` is the Ungrouped Actions row.
    /// A new action is recorded into this folder when it is not empty.
    #[serde(default)]
    pub selected_set: Option<String>,
    /// Folder names the user has collapsed. Missing names are expanded.
    #[serde(default)]
    pub collapsed_sets: Vec<String>,
    /// `"{action name}\\t{step}"` keys the user has collapsed. Missing keys are expanded.
    #[serde(default)]
    pub collapsed_steps: Vec<String>,
    /// Photoshop's Button Mode: one coloured button per action.
    #[serde(default)]
    pub button_mode: bool,
    /// Play, the flyout's Playback Options.
    #[serde(default)]
    pub playback_mode: PlaybackMode,
    /// Seconds for [`PlaybackMode::Pause`]. Zero is treated as one second.
    #[serde(default)]
    pub pause_seconds: f32,
    #[serde(skip)]
    reveal_recording: Option<(usize, usize)>,
    /// Step-by-step, pause, or a dialog waiting in the middle of Play.
    #[serde(skip)]
    playback: Option<PlaybackCursor>,
    /// The next menu command is inserted into this action and not run.
    #[serde(skip)]
    capture_menu: Option<usize>,
    /// Action Options draft. `None` hides the editor.
    #[serde(skip)]
    options: Option<OptionsDraft>,
    /// Insert Stop draft.
    #[serde(skip)]
    stop_message: Option<StopDraft>,
}

#[derive(Clone, Debug, PartialEq)]
struct PlaybackCursor {
    action: usize,
    step: usize,
    rest: Vec<usize>,
    honor_enabled: bool,
    waiting_dialog: bool,
    wait_until: Option<std::time::Instant>,
    mode: PlaybackMode,
}

#[derive(Clone, Debug, PartialEq)]
struct OptionsDraft {
    index: usize,
    name: String,
    color: String,
    key: String,
    shift: bool,
    ctrl: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct StopDraft {
    message: String,
    allow_continue: bool,
}

fn step_key(name: &str, step: usize) -> String {
    format!("{name}\t{step}")
}

fn label_of(id: &str) -> String {
    photocraft_engine::commands::find(id)
        .map(|c| c.label)
        .or_else(|| crate::menus::UI_COMMANDS.iter().find(|c| c.0 == id).map(|c| c.1))
        .map(|label| label.trim_end_matches('…').to_string())
        .unwrap_or_else(|| id.to_string())
}

/// The selected action, or the first one when nothing is selected.
pub fn selected_action(app: &PhotocraftApp) -> Option<&Action> {
    let list = &app.session.actions.list;
    let idx = app.ui.actions.selected.filter(|i| *i < list.len());
    idx.and_then(|i| list.get(i)).or_else(|| list.first())
}

/// Index of the playback target. Photoshop's Play button uses the first action when
/// the panel has actions but none of its rows is selected.
fn playback_selection(app: &PhotocraftApp) -> Option<usize> {
    let list = &app.session.actions.list;
    let n = list.len();
    if n == 0 {
        return None;
    }
    if let Some(i) = app.ui.actions.selected.filter(|i| *i < n) {
        return Some(i);
    }
    if let Some(set) = app.ui.actions.selected_set.as_deref()
        && let Some(i) = (0..n).find(|i| app.session.actions.folder(*i) == set)
    {
        return Some(i);
    }
    Some(0)
}

/// Steps in the `[[id, params], …]` shape batch and droplets already accept.
pub fn action_steps(action: &Action) -> Vec<Value> {
    action.steps.iter().map(|(id, p)| json!([id, p])).collect()
}

/// Named action bindings use the existing persisted shortcut overrides. Keeping the
/// name in the key means selecting or deleting another row cannot retarget F6.
pub(crate) const SHORTCUT_PREFIX: &str = "actions.play:";

pub(crate) fn shortcut_id(name: &str) -> String {
    format!("{SHORTCUT_PREFIX}{name}")
}

pub(crate) fn assign_shortcut(app: &mut PhotocraftApp, name: &str, shortcut: &str) -> Result<Value, String> {
    let id = shortcut_id(name);
    let mut set = serde_json::Map::new();
    // A function key belongs to only one action. Ordinary menu bindings remain
    // intact and become available again when the action binding is removed.
    for (other, assigned) in &app.session.prefs().shortcuts {
        if other.starts_with(SHORTCUT_PREFIX) && other != &id && assigned == shortcut && !shortcut.is_empty() {
            set.insert(other.clone(), json!(""));
        }
    }
    set.insert(id, json!(shortcut));
    app.run("edit.keyboardShortcuts", json!({"set": set, "allowUnknown": true, "removeConflicts": false}))
}

/// Replay edits synchronously, with view steps handled by the shell in the same order.
/// Engine steps keep the headless action semantics (no dialogs or background jobs).
pub(crate) fn play(app: &mut PhotocraftApp, params: &Value) -> Result<Value, String> {
    let (action, from) = actions_cmds::playback_plan(&app.session, params).map_err(|e| e.to_string())?;
    // A set plays only actions whose checkbox is on. Play on the action itself still runs it.
    if params.get("honorEnabled").and_then(Value::as_bool) == Some(true) && !action.enabled {
        return Ok(json!({"action": action.name, "ran": 0, "skipped": true}));
    }
    let recording = actions_cmds::begin_playback(&mut app.session, &action).map_err(|e| e.to_string())?;
    let only = params.get("only").and_then(Value::as_u64);
    let mut ran = 0u64;
    let mut failed = None;
    for (i, (id, p)) in action.steps.iter().enumerate().skip(from) {
        if !action.step_on(i) {
            continue;
        }
        let result = (|| {
            if let Some(auth) = app.session.authorize {
                auth(id, p).map_err(|e| e.to_string())?;
            }
            if id == "actions.play" {
                play(app, p)
            } else if actions_cmds::shell_view_command(id) {
                app.sync_views();
                app.run(id, p.clone())
            } else {
                app.session.execute(id, p.clone()).map_err(|e| e.to_string())
            }
        })();
        match result {
            Ok(value) => {
                if let Some(error) = actions_cmds::nested_failure(id, &value) {
                    failed = Some(json!({"step": i, "id": id, "error": error}));
                    break;
                }
                ran += 1;
                if only.is_some_and(|n| ran >= n) {
                    break;
                }
            }
            Err(error) => {
                failed = Some(json!({"step": i, "id": id, "error": error}));
                break;
            }
        }
    }
    actions_cmds::finish_playback(&mut app.session, recording, &action, from);
    Ok(match failed {
        Some(f) => json!({"action": action.name, "ran": ran, "failed": f}),
        None => json!({"action": action.name, "ran": ran}),
    })
}

/// Execute the actual menu command, not a synthetic keystroke. Fit is deferred
/// until canvas layout, so it uses the dimensions after all preceding action steps.
pub(crate) fn run_view(app: &mut PhotocraftApp, id: &str, params: Value) -> Result<Value, String> {
    app.sync_views();
    let i = app.session.active_index().ok_or("no document")?;
    let v = app.ui.views.get_mut(i).ok_or("no document view")?;
    v.fill_pending = false;
    match id {
        "view.fitOnScreen" => v.fit_pending = true,
        "view.zoomIn" => {
            v.fit_pending = false;
            v.zoom = crate::zoom_levels::step(v.zoom, 1, v.doc_size);
        }
        "view.zoomOut" => {
            v.fit_pending = false;
            v.zoom = crate::zoom_levels::step(v.zoom, -1, v.doc_size);
        }
        "view.actualPixels" => {
            v.fit_pending = false;
            v.zoom = 1.0;
        }
        _ => return Err(format!("unsupported view command: {id}")),
    }
    app.session.journal.push((id.into(), params));
    Ok(Value::Null)
}

fn begin_recording(app: &mut PhotocraftApp, append: bool) {
    if app.session.actions.recording.is_some() {
        return;
    }
    let params = if append {
        app.ui.actions.selected.map_or(json!({}), |i| json!({"action": i}))
    } else {
        match app.ui.actions.selected_set.as_deref() {
            Some(set) if !set.is_empty() => json!({"set": set}),
            _ => json!({}),
        }
    };
    let after = if append { app.ui.actions.selected_step } else { None };
    let Ok(v) = app.run("actions.record", params) else { return };
    let Some(i) = v.get("index").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()) else { return };
    app.session.actions.recording_after = after;
    app.ui.actions.selected = Some(i);
    app.ui.actions.selected_step = None;
    if app.ui.actions.expanded.len() <= i {
        app.ui.actions.expanded.resize(i + 1, false);
    }
    app.ui.actions.expanded[i] = true;
}

pub(crate) fn report_play(app: &mut PhotocraftApp, v: &Value) {
    let Some(failed) = v.get("failed").filter(|f| f.is_object()) else {
        let ran = v.get("ran").and_then(Value::as_u64).unwrap_or(0);
        app.ui.status = format!("Played {ran} steps");
        app.ui.status_error = false;
        return;
    };
    let step = failed.get("step").and_then(Value::as_u64).unwrap_or(0);
    let id = failed.get("id").and_then(Value::as_str).unwrap_or("");
    let error = failed.get("error").and_then(Value::as_str).unwrap_or("");
    app.ui.status = format!("Step {} ({id}) failed: {error}", step + 1);
    app.ui.status_error = true;
}

fn visible_steps(app: &PhotocraftApp, action: usize) -> Vec<(String, Value)> {
    let committed: Vec<(String, Value)> = app.session.actions.list.get(action).into_iter().flat_map(|a| a.steps.iter().cloned()).collect();
    let pending: Vec<(String, Value)> = actions_cmds::pending_steps(&app.session, action).cloned().collect();
    let recording_here = app.session.actions.recording.is_some_and(|(_, i)| i == action);
    if recording_here && let Some(after) = app.session.actions.recording_after {
        let at = after.saturating_add(1).min(committed.len());
        let mut out = Vec::with_capacity(committed.len() + pending.len());
        out.extend(committed.iter().take(at).cloned());
        out.extend(pending);
        out.extend(committed.iter().skip(at).cloned());
        return out;
    }
    let mut out = committed;
    out.extend(pending);
    out
}

/// Visible step index back to the committed step. Pending recorded steps have none yet.
fn committed_step_index(app: &PhotocraftApp, action: usize, visible: usize) -> Option<usize> {
    let committed = app.session.actions.list.get(action).map(|item| item.steps.len()).unwrap_or(0);
    let pending = actions_cmds::pending_steps(&app.session, action).count();
    let recording_here = app.session.actions.recording.is_some_and(|(_, i)| i == action);
    if recording_here && let Some(after) = app.session.actions.recording_after {
        let at = after.saturating_add(1).min(committed);
        if visible < at {
            return Some(visible);
        }
        if visible < at.saturating_add(pending) {
            return None;
        }
        return visible.checked_sub(pending).filter(|index| *index < committed);
    }
    (visible < committed).then_some(visible)
}

struct ParamLine {
    depth: u8,
    text: String,
}

fn number_text(value: &Value) -> String {
    match value.as_f64() {
        Some(n) if n.is_finite() && n.fract() == 0.0 && n.abs() < 1.0e6 => format!("{}", n as i64),
        Some(n) if n.is_finite() => format!("{n}"),
        _ => value.to_string(),
    }
}

fn rgb_bytes(params: &Value) -> Option<(u8, u8, u8)> {
    let color = params.get("color")?;
    if let Some(text) = color.as_str() {
        let hex = text.trim().trim_start_matches('#');
        if hex.len() == 6
            && let Ok(n) = u32::from_str_radix(hex, 16)
        {
            return Some(((n >> 16) as u8, (n >> 8) as u8, n as u8));
        }
    }
    let arr = color.as_array()?;
    let byte = |i: usize| -> Option<u8> {
        let n = arr.get(i)?.as_f64()?;
        let n = if (0.0..=1.0).contains(&n) { n * 255.0 } else { n };
        Some(n.round().clamp(0.0, 255.0) as u8)
    };
    Some((byte(0)?, byte(1)?, byte(2)?))
}

fn bounds_line(params: &Value) -> String {
    if params.get("applyAtCanvasBounds").and_then(Value::as_bool) == Some(true) {
        "With Effect At Canvas Bounds".into()
    } else {
        "Without Effect At Canvas Bounds".into()
    }
}

/// Readable parameter rows under a step, in the order Photoshop shows them.
fn parameter_lines(id: &str, params: &Value) -> Vec<ParamLine> {
    let line = |depth: u8, text: String| ParamLine { depth, text };
    if id == "image.dtf"
        || id == "select.loadSelection"
        || id == "select.modify.contract"
        || id == "select.modify.expand"
        || id == "select.modify.feather"
        || id == "channel.newSpot"
    {
        let mut lines = Vec::new();
        if id == "image.dtf" || (id == "select.loadSelection" && params.get("channel").and_then(Value::as_str) == Some("transparency")) {
            if id == "image.dtf" {
                lines.push(line(1, "Set Selection".into()));
            }
            lines.push(line(if id == "image.dtf" { 2 } else { 1 }, "To: transparency channel".into()));
        }
        if id == "image.dtf" || id.starts_with("select.modify.") {
            if id == "image.dtf" {
                lines.push(line(1, "Contract".into()));
            }
            let radius = if id == "image.dtf" { params.get("contract") } else { params.get("radius") };
            let depth = if id == "image.dtf" { 2 } else { 1 };
            lines.push(line(depth, format!("By: {} pixels", radius.map(number_text).unwrap_or_else(|| "1".into()))));
            lines.push(line(depth, bounds_line(params)));
        }
        if id == "image.dtf" || id == "channel.newSpot" {
            if id == "image.dtf" {
                lines.push(line(1, "Make".into()));
            }
            let depth = if id == "image.dtf" { 2 } else { 1 };
            lines.push(line(depth, "New: spot color channel".into()));
            let name = params.get("name").and_then(Value::as_str).unwrap_or(photocraft_engine::dtf_cmds::DEFAULT_NAME);
            lines.push(line(depth, format!("Name: \"{name}\"")));
            lines.push(line(depth, "Color: RGB color".into()));
            if let Some((r, g, b)) = rgb_bytes(params) {
                lines.push(line(depth, format!("Red: {r}")));
                lines.push(line(depth, format!("Green: {g}")));
                lines.push(line(depth, format!("Blue: {b}")));
            }
            let solidity = params.get("solidity").map(number_text).unwrap_or_else(|| "60".into());
            lines.push(line(depth, format!("Solidity: {solidity}")));
        }
        return lines;
    }
    if id == "actions.message" {
        let message = params.get("message").and_then(Value::as_str).unwrap_or("");
        let mut lines = vec![line(1, format!("Message: \"{message}\""))];
        if params.get("allowContinue").and_then(Value::as_bool) == Some(false) {
            lines.push(line(1, "Allow Continue: off".into()));
        }
        return lines;
    }
    let Some(map) = params.as_object() else { return Vec::new() };
    map.iter()
        .filter(|(key, value)| !key.starts_with("__") && !value.is_object() && !value.is_array())
        .map(|(key, value)| {
            let text = match value {
                Value::String(s) => format!("{}: \"{s}\"", field_label(key)),
                Value::Bool(true) => field_label(key),
                Value::Bool(false) => format!("{}: off", field_label(key)),
                _ => format!("{}: {}", field_label(key), number_text(value)),
            };
            line(1, text)
        })
        .collect()
}

fn step_label(id: &str, params: &Value) -> String {
    if id == "image.dtf" {
        let contract = params.get("contract").and_then(Value::as_f64).unwrap_or(f64::from(photocraft_engine::dtf_cmds::DEFAULT_CONTRACT));
        let name = params.get("name").and_then(Value::as_str).unwrap_or(photocraft_engine::dtf_cmds::DEFAULT_NAME);
        let contract = if contract.fract() == 0.0 && contract.abs() < 1.0e6 { format!("{}", contract as i64) } else { format!("{contract}") };
        return format!("DTF Spot Channel ({contract} px, {name})");
    }
    if id == "select.loadSelection" && params.get("channel").and_then(Value::as_str) == Some("transparency") {
        return "Set Selection".into();
    }
    if id == "select.modify.contract" {
        return "Contract".into();
    }
    if id == "select.modify.expand" {
        return "Expand".into();
    }
    if id == "select.modify.feather" {
        return "Feather".into();
    }
    if id == "channel.newSpot" {
        return "Make".into();
    }
    if id == "actions.message" {
        return "Stop".into();
    }
    if id == "actions.play" {
        let target = params.get("action").map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default();
        format!("{}: {target}", label_of(id))
    } else {
        label_of(id)
    }
}

#[derive(Clone, Copy)]
struct DragRow {
    action: usize,
    step: Option<usize>,
}

/// Drops use an insertion line, converted to the final index expected by actions.move.
fn drag_row(ui: &egui::Ui, response: &egui::Response, row: DragRow, t: &Tokens) -> Option<(DragRow, usize)> {
    response.dnd_set_drag_payload(row);
    let source = response.dnd_hover_payload::<DragRow>()?;
    if source.step.is_some() != row.step.is_some() || (source.step.is_some() && source.action != row.action) {
        return None;
    }
    let after = ui.input(|i| i.pointer.hover_pos().is_some_and(|p| p.y >= response.rect.center().y));
    let index = row.step.unwrap_or(row.action);
    let from = source.step.unwrap_or(source.action);
    let slot = index.saturating_add(usize::from(after));
    let to = if from < slot { slot.saturating_sub(1) } else { slot };
    let y = if after { response.rect.bottom() } else { response.rect.top() };
    ui.painter().line_segment([pos2(response.rect.left() + 22.0, y), pos2(response.rect.right(), y)], Stroke::new(2.0, t.accent));
    response.dnd_release_payload::<DragRow>().map(|source| (*source, to))
}

fn duplicate_row(app: &mut PhotocraftApp, source: DragRow) {
    let mut params = json!({"action": source.action});
    if let Some(step) = source.step {
        params["step"] = json!(step);
    }
    let Ok(v) = app.run("actions.duplicate", params) else { return };
    if source.step.is_some() {
        if let Some(step) = v.get("step").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()) {
            app.ui.actions.selected = Some(source.action);
            app.ui.actions.selected_step = Some(step);
        }
        return;
    }
    if let Some(index) = v.get("index").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()) {
        app.ui.actions.expanded.resize(app.session.actions.list.len(), false);
        app.ui.actions.selected = Some(index);
        app.ui.actions.selected_step = None;
        app.ui.actions.selected_set = Some(app.session.actions.folder(index).to_string());
    }
}

fn move_row(app: &mut PhotocraftApp, source: DragRow, to: usize) {
    let mut params = json!({"action": source.action, "to": to});
    if let Some(step) = source.step {
        params["step"] = json!(step);
    }
    if app.run("actions.move", params).is_err() {
        return;
    }
    if source.step.is_some() {
        app.ui.actions.selected = Some(source.action);
        app.ui.actions.selected_step = Some(to);
    } else {
        app.ui.actions.expanded.resize(app.session.actions.list.len(), false);
        if source.action < app.ui.actions.expanded.len() && to < app.ui.actions.expanded.len() {
            let expanded = app.ui.actions.expanded.remove(source.action);
            app.ui.actions.expanded.insert(to, expanded);
        }
        app.ui.actions.selected = Some(to);
        app.ui.actions.selected_step = None;
    }
}

fn delete_selection(app: &mut PhotocraftApp) {
    if app.ui.actions.selected.is_none()
        && let Some(set) = app.ui.actions.selected_set.clone()
        && !set.is_empty()
    {
        if app.run("actions.deleteSet", json!({"name": set})).is_ok() {
            app.ui.actions.selected_set = None;
            app.ui.actions.collapsed_sets.retain(|name| name != &set);
        }
        return;
    }
    let Some(action) = app.ui.actions.selected else { return };
    let step = app.ui.actions.selected_step;
    let mut params = json!({"action": action});
    if let Some(step) = step {
        params["step"] = json!(step);
    }
    if app.run("actions.delete", params).is_err() {
        return;
    }
    // Clear selection after deletion so a second click cannot delete a different
    // step or unexpectedly fall back to deleting the entire action.
    app.ui.actions.selected = None;
    app.ui.actions.selected_step = None;
    if step.is_none() && action < app.ui.actions.expanded.len() {
        app.ui.actions.expanded.remove(action);
    }
}

/// Folders in panel order, then the action indices inside each one. Actions with an empty
/// folder are "Ungrouped Actions". A named folder with no actions is still shown.
fn grouped(state: &photocraft_engine::actions_cmds::ActionState) -> Vec<(String, Vec<usize>)> {
    let mut seen = Vec::new();
    let mut names = Vec::new();
    let ungrouped: Vec<usize> = (0..state.list.len()).filter(|i| state.folder(*i).is_empty()).collect();
    if !ungrouped.is_empty() {
        names.push(actions_cmds::UNGROUPED.to_string());
    }
    for name in state.sets.iter().map(String::as_str).chain((0..state.list.len()).map(|i| state.folder(i))) {
        if name.is_empty() || seen.iter().any(|have: &String| have == name) {
            continue;
        }
        seen.push(name.to_string());
        names.push(name.to_string());
    }
    names
        .into_iter()
        .map(|name| {
            let idxs = if name == actions_cmds::UNGROUPED {
                (0..state.list.len()).filter(|i| state.folder(*i).is_empty()).collect()
            } else {
                (0..state.list.len()).filter(|i| state.folder(*i) == name).collect()
            };
            (name, idxs)
        })
        .collect()
}

fn field_label(key: &str) -> String {
    let mut out = String::new();
    for (i, c) in key.chars().enumerate() {
        if i == 0 {
            out.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Number, text and checkbox fields for the selected step. Objects and arrays stay as recorded.
fn step_editor(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let Some(ai) = app.ui.actions.selected else { return };
    let Some(si) = app.ui.actions.selected_step else { return };
    let Some((id, params)) = app.session.actions.list.get(ai).and_then(|action| action.steps.get(si)).cloned() else { return };
    let Value::Object(map) = &params else { return };
    let keys: Vec<String> = map
        .iter()
        .filter(|(key, value)| !key.starts_with("__") && matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_)))
        .map(|(key, _)| key.clone())
        .collect();
    if keys.is_empty() {
        return;
    }
    ui.separator();
    ui.label(egui::RichText::new(format!("{} — {}", tl!("Step settings"), step_label(&id, &params))).size(11.0));
    let mut updated = params.clone();
    let mut changed = false;
    for key in keys {
        let Some(value) = map.get(&key).cloned() else { continue };
        ui.horizontal(|ui| {
            ui.label(field_label(&key));
            match value {
                Value::Bool(mut on) => {
                    if ui.checkbox(&mut on, "").changed()
                        && let Some(obj) = updated.as_object_mut()
                    {
                        obj.insert(key, json!(on));
                        changed = true;
                    }
                }
                Value::String(mut text) => {
                    let edited = if key == "source" {
                        let opts = [("layer".to_string(), "Layer"), ("composite".to_string(), "Visible layers")];
                        crate::widgets::dropdown(ui, &format!("step-source-{ai}-{si}"), &mut text, &opts, 160.0)
                    } else {
                        ui.add(egui::TextEdit::singleline(&mut text).desired_width(160.0).id(egui::Id::new(("step-field", ai, si, key.clone())))).changed()
                    };
                    if edited && let Some(obj) = updated.as_object_mut() {
                        obj.insert(key, json!(text));
                        changed = true;
                    }
                }
                Value::Number(n) => {
                    if let Some(mut i) = n.as_i64() {
                        if ui.add(egui::DragValue::new(&mut i)).changed()
                            && let Some(obj) = updated.as_object_mut()
                        {
                            obj.insert(key, json!(i));
                            changed = true;
                        }
                    } else if let Some(mut x) = n.as_f64()
                        && ui.add(egui::DragValue::new(&mut x).speed(0.5)).changed()
                        && let Some(obj) = updated.as_object_mut()
                    {
                        obj.insert(key, json!(x));
                        changed = true;
                    }
                }
                _ => {}
            }
        });
    }
    if changed {
        let _ = app.run("actions.setStep", json!({"action": ai, "step": si, "params": updated}));
    }
}

fn fully_on(action: &Action) -> bool {
    action.enabled && action.steps.iter().enumerate().all(|(i, _)| action.step_on(i))
}

/// `None` when no step opens a dialog, `Some(true)` when every step does, `Some(false)` when mixed.
fn dialog_state(action: &Action) -> Option<bool> {
    if action.steps.is_empty() {
        return None;
    }
    let any = (0..action.steps.len()).any(|i| action.step_dialog_on(i));
    let all = (0..action.steps.len()).all(|i| action.step_dialog_on(i));
    if !any { None } else { Some(all) }
}

fn button_color(name: &str) -> Color32 {
    match name {
        "red" => Color32::from_rgb(176, 64, 64),
        "orange" => Color32::from_rgb(196, 120, 48),
        "yellow" => Color32::from_rgb(186, 164, 64),
        "green" => Color32::from_rgb(64, 140, 84),
        "blue" => Color32::from_rgb(58, 110, 176),
        "violet" => Color32::from_rgb(130, 84, 168),
        "gray" => Color32::from_rgb(110, 110, 114),
        _ => Color32::from_rgb(88, 88, 92),
    }
}

fn toggle_include(app: &mut PhotocraftApp, action: usize, step: Option<usize>) {
    let Some(item) = app.session.actions.list.get(action) else { return };
    let enabled = match step {
        Some(step) => !item.step_on(step),
        None => !fully_on(item),
    };
    let mut params = json!({"action": action, "enabled": enabled});
    if let Some(step) = step {
        params["step"] = json!(step);
    }
    let _ = app.run("actions.setEnabled", params);
}

fn toggle_set(app: &mut PhotocraftApp, set: &str) {
    let on =
        (0..app.session.actions.list.len()).filter(|i| app.session.actions.folder(*i) == set).all(|i| app.session.actions.list.get(i).is_some_and(fully_on));
    let _ = app.run("actions.setEnabled", json!({"set": set, "enabled": !on}));
}

fn toggle_dialog(app: &mut PhotocraftApp, action: usize, step: Option<usize>) {
    let Some(item) = app.session.actions.list.get(action) else { return };
    let dialog = match step {
        Some(step) => !item.step_dialog_on(step),
        None => !(0..item.steps.len()).all(|i| item.step_dialog_on(i)),
    };
    let mut params = json!({"action": action, "dialog": dialog});
    if let Some(step) = step {
        params["step"] = json!(step);
    }
    let _ = app.run("actions.setDialog", params);
}

fn paint_mark(ui: &egui::Ui, center: egui::Pos2, on: bool, partial: bool, dialog: bool, t: &Tokens) {
    if dialog {
        let rect = Rect::from_center_size(center, vec2(9.0, 9.0));
        let color = if partial {
            Color32::from_rgb(210, 48, 48)
        } else if on {
            t.text
        } else {
            t.text_faint
        };
        ui.painter().rect_stroke(rect, 1.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
        if on {
            ui.painter().rect_filled(rect.shrink(2.0), 0.0, color);
        }
        return;
    }
    if !on {
        return;
    }
    let color = if partial { Color32::from_rgb(210, 48, 48) } else { t.text };
    ui.painter().text(center, Align2::CENTER_CENTER, "✓", egui::FontId::proportional(12.0), color);
}

fn shortcut_parts(shortcut: &str) -> (String, bool, bool) {
    let mut shift = false;
    let mut ctrl = false;
    let mut key = String::new();
    for part in shortcut.split('+') {
        match part {
            "Shift" => shift = true,
            "Ctrl" | "Cmd" => ctrl = true,
            other => key = other.to_string(),
        }
    }
    (key, shift, ctrl)
}

fn shortcut_text(key: &str, shift: bool, ctrl: bool) -> String {
    if key.is_empty() {
        return String::new();
    }
    match (ctrl, shift) {
        (true, true) => format!("Ctrl+Shift+{key}"),
        (true, false) => format!("Ctrl+{key}"),
        (false, true) => format!("Shift+{key}"),
        (false, false) => key.to_string(),
    }
}

fn open_options(app: &mut PhotocraftApp, index: usize) {
    let Some(action) = app.session.actions.list.get(index) else { return };
    let shortcut = app.session.prefs().shortcuts.get(&shortcut_id(&action.name)).cloned().unwrap_or_default();
    let (key, shift, ctrl) = shortcut_parts(&shortcut);
    app.ui.actions.options = Some(OptionsDraft { index, name: action.name.clone(), color: action.color.clone(), key, shift, ctrl });
    app.ui.actions.stop_message = None;
}

fn open_step_dialog(app: &mut PhotocraftApp, action: usize, step: usize, playback: bool) -> bool {
    let Some((id, params)) = app.session.actions.list.get(action).and_then(|item| item.steps.get(step)).cloned() else { return false };
    let mut fields = serde_json::Map::new();
    fields.insert("__command".into(), json!(id));
    fields.insert("__label".into(), json!(step_label(&id, &params)));
    fields.insert("__form".into(), json!(true));
    fields.insert("__recorded".into(), params.clone());
    fields.insert("__actionStep".into(), json!([action, step]));
    if playback {
        fields.insert("__actionPlayback".into(), json!(true));
    } else {
        fields.insert("__editStep".into(), json!(true));
    }
    let source = if id == "image.dtf" { photocraft_engine::dtf_cmds::dialog_fields(Some(&params)) } else { params };
    if id == "image.dtf" {
        fields.insert("__choices".into(), json!({"source": ["layer", "composite"]}));
    }
    if let Some(map) = source.as_object() {
        for (key, value) in map {
            if !key.starts_with("__") && !value.is_object() && !value.is_array() {
                fields.insert(key.clone(), value.clone());
            }
        }
    }
    if id == "actions.message"
        && let Some(message) = fields.get("message").and_then(Value::as_str)
    {
        fields.insert("__note".into(), json!(message));
    }
    app.ui.open_dialog(crate::state::DialogKind::Command, fields);
    true
}

/// Overlay the values the user edited onto the recorded parameters, keeping keys the form does not show.
pub(crate) fn merge_recorded(recorded: &Value, edited: &Value) -> Value {
    let mut base = recorded.clone();
    if let (Some(base), Some(edited)) = (base.as_object_mut(), edited.as_object()) {
        for (key, value) in edited {
            base.insert(key.clone(), value.clone());
        }
        return Value::Object(base.clone());
    }
    edited.clone()
}

pub(crate) fn committed_dialog(app: &mut PhotocraftApp, fields: &serde_json::Map<String, Value>) {
    if fields.get("__editStep").and_then(Value::as_bool) == Some(true)
        && let Some(pair) = fields.get("__actionStep").and_then(Value::as_array)
        && let (Some(action), Some(step)) = (pair.first().and_then(Value::as_u64), pair.get(1).and_then(Value::as_u64))
    {
        let edited = crate::filter_dialog::params_of(fields);
        let params = fields.get("__recorded").map(|recorded| merge_recorded(recorded, &edited)).unwrap_or(edited);
        let _ = app.run("actions.setStep", json!({"action": action, "step": step, "params": params}));
    }
    if fields.get("__actionPlayback").and_then(Value::as_bool) == Some(true)
        && let Some(cursor) = app.ui.actions.playback.as_mut()
    {
        cursor.waiting_dialog = false;
        cursor.step = cursor.step.saturating_add(1);
        cursor.wait_until = None;
    }
}

pub(crate) fn cancelled_playback(app: &mut PhotocraftApp) {
    if app.ui.actions.playback.take().is_some() {
        app.ui.status = "Playback stopped".into();
        app.ui.status_error = false;
    }
}

pub(crate) fn menu_capture_target(app: &PhotocraftApp) -> Option<usize> {
    app.ui.actions.capture_menu
}

pub(crate) fn take_menu_capture(app: &mut PhotocraftApp) -> Option<usize> {
    app.ui.actions.capture_menu.take()
}

fn play_action(app: &mut PhotocraftApp, index: usize, from: usize, honor: bool) {
    let mut params = json!({"action": index, "from": from});
    if honor {
        params["honorEnabled"] = json!(true);
    }
    match app.run("actions.play", params) {
        Ok(v) => report_play(app, &v),
        Err(e) => {
            app.ui.status = format!("Action playback failed: {e}");
            app.ui.status_error = true;
        }
    }
}

fn needs_cursor(app: &PhotocraftApp, index: usize, from: usize) -> bool {
    if app.ui.actions.playback_mode != PlaybackMode::Accelerated {
        return true;
    }
    app.session.actions.list.get(index).is_some_and(|action| {
        action.steps.iter().enumerate().skip(from).any(|(i, (id, _))| action.step_on(i) && (action.step_dialog_on(i) || id == "actions.message"))
    })
}

fn queue_playback(app: &mut PhotocraftApp, mut indices: Vec<usize>, from: usize, honor: bool) {
    if app.ui.actions.playback.as_ref().is_some_and(|cursor| cursor.waiting_dialog) {
        return;
    }
    if app.ui.actions.playback.as_ref().is_some_and(|cursor| !cursor.waiting_dialog) {
        if let Some(cursor) = app.ui.actions.playback.as_mut() {
            cursor.wait_until = None;
        }
        return;
    }
    while let Some(index) = indices.first().copied() {
        if !needs_cursor(app, index, from) {
            play_action(app, index, from, honor);
            if app.ui.status_error {
                return;
            }
            indices.remove(0);
            continue;
        }
        indices.remove(0);
        app.ui.actions.playback = Some(PlaybackCursor {
            action: index,
            step: from,
            rest: indices,
            honor_enabled: honor,
            waiting_dialog: false,
            wait_until: None,
            mode: app.ui.actions.playback_mode,
        });
        return;
    }
}

fn start_playback(app: &mut PhotocraftApp) {
    let n = app.session.actions.list.len();
    if let Some(index) = app.ui.actions.selected.filter(|i| *i < n) {
        let from = app.ui.actions.selected_step.unwrap_or(0);
        queue_playback(app, vec![index], from, false);
        return;
    }
    if let Some(set) = app.ui.actions.selected_set.clone() {
        let indices: Vec<usize> = (0..n).filter(|i| app.session.actions.folder(*i) == set).collect();
        if !indices.is_empty() {
            queue_playback(app, indices, 0, true);
            return;
        }
    }
    if let Some(index) = playback_selection(app) {
        queue_playback(app, vec![index], 0, false);
    }
}

fn pump(app: &mut PhotocraftApp, ctx: &egui::Context) {
    loop {
        let Some(cursor) = app.ui.actions.playback.clone() else { return };
        if cursor.waiting_dialog {
            return;
        }
        if let Some(until) = cursor.wait_until {
            let now = std::time::Instant::now();
            if now < until {
                ctx.request_repaint_after(until.saturating_duration_since(now));
                return;
            }
            if let Some(live) = app.ui.actions.playback.as_mut() {
                live.wait_until = None;
            }
        }
        let Some(action) = app.session.actions.list.get(cursor.action).cloned() else {
            app.ui.actions.playback = None;
            return;
        };
        if cursor.honor_enabled && !action.enabled || cursor.step >= action.steps.len() {
            let rest = cursor.rest.clone();
            let honor = cursor.honor_enabled;
            let mode = cursor.mode;
            app.ui.actions.playback = None;
            if rest.first().is_some() {
                app.ui.actions.playback_mode = mode;
                queue_playback(app, rest, 0, honor);
                if app.ui.actions.playback.is_none() {
                    return;
                }
                continue;
            }
            return;
        }
        if !action.step_on(cursor.step) {
            if let Some(live) = app.ui.actions.playback.as_mut() {
                live.step = live.step.saturating_add(1);
            }
            continue;
        }
        let id = action.steps.get(cursor.step).map(|(id, _)| id.clone()).unwrap_or_default();
        let wants_dialog = action.step_dialog_on(cursor.step) || id == "actions.message";
        if wants_dialog && open_step_dialog(app, cursor.action, cursor.step, true) {
            if let Some(live) = app.ui.actions.playback.as_mut() {
                live.waiting_dialog = true;
            }
            app.ui.status = format!("Paused for the {id} dialog");
            app.ui.status_error = false;
            return;
        }
        let mut params = json!({"action": cursor.action, "from": cursor.step, "only": 1});
        if cursor.honor_enabled {
            params["honorEnabled"] = json!(true);
        }
        match app.run("actions.play", params) {
            Ok(v) if v.get("failed").is_some() => {
                report_play(app, &v);
                app.ui.actions.playback = None;
                return;
            }
            Ok(_) => {
                let mode = cursor.mode;
                if let Some(live) = app.ui.actions.playback.as_mut() {
                    live.step = live.step.saturating_add(1);
                    if mode == PlaybackMode::Pause {
                        let secs = if app.ui.actions.pause_seconds > 0.0 { app.ui.actions.pause_seconds } else { 1.0 };
                        live.wait_until = Some(std::time::Instant::now() + std::time::Duration::from_secs_f32(secs));
                    }
                }
                if mode == PlaybackMode::Accelerated {
                    continue;
                }
                let step = app.ui.actions.playback.as_ref().map(|c| c.step).unwrap_or(0);
                app.ui.status = format!("Paused after step {step}. Press Play to continue.");
                app.ui.status_error = false;
                if mode == PlaybackMode::Pause {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f32(app.ui.actions.pause_seconds.max(0.05)));
                }
                return;
            }
            Err(e) => {
                app.ui.status = format!("Action playback failed: {e}");
                app.ui.status_error = true;
                app.ui.actions.playback = None;
                return;
            }
        }
    }
}

fn load_action_bytes(app: &mut PhotocraftApp, name: &str, bytes: &[u8], replace: bool) -> Result<Value, String> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".atn") {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let v = app.run("actions.importAtn", json!({"base64": encoded, "replace": replace}))?;
        if let Some(shortcuts) = v.get("shortcuts").and_then(Value::as_array) {
            for item in shortcuts {
                let Some(action) = item.get("action").and_then(Value::as_str) else { continue };
                let Some(shortcut) = item.get("shortcut").and_then(Value::as_str) else { continue };
                let _ = assign_shortcut(app, action, shortcut);
            }
        }
        let skipped = v.get("skipped").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
        let set = v.get("set").and_then(Value::as_str).unwrap_or("actions");
        app.ui.status =
            if skipped == 0 { format!("Loaded action set {set}") } else { format!("Loaded action set {set}. Skipped {skipped} unsupported steps.") };
        app.ui.status_error = false;
        Ok(v)
    } else {
        let text = String::from_utf8(bytes.to_vec()).map_err(|_| format!("{name}: not a PhotoCraft actions file"))?;
        let v = app.run("actions.importJson", json!({"text": text, "replace": replace}))?;
        app.ui.status = "Loaded actions".into();
        app.ui.status_error = false;
        Ok(v)
    }
}

fn save_actions(app: &mut PhotocraftApp) {
    let set = app.ui.actions.selected_set.clone().unwrap_or_default();
    let params = if set.is_empty() { json!({}) } else { json!({"set": set}) };
    let Ok(v) = app.run("actions.exportJson", params) else { return };
    let Some(text) = v.get("text").and_then(Value::as_str).map(str::to_owned) else { return };
    let _ = app.pick_save("Actions.json", move |app, path| {
        let write = app.services.write.as_mut().ok_or("no writer configured")?;
        write(&path, text.as_bytes())?;
        app.ui.status = format!("Saved actions to {path}");
        app.ui.status_error = false;
        Ok(json!({"path": path}))
    });
}

fn load_actions(app: &mut PhotocraftApp, replace: bool) {
    let _ = app.pick_file_bytes_filtered(&["atn", "json"], move |app, name, bytes| load_action_bytes(app, &name, &bytes, replace));
}

fn options_editor(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let Some(mut draft) = app.ui.actions.options.clone() else { return };
    ui.separator();
    ui.label(egui::RichText::new(tl!("Action Options")).strong());
    ui.horizontal(|ui| {
        ui.label(tl!("Name"));
        ui.add(egui::TextEdit::singleline(&mut draft.name).desired_width(160.0));
    });
    ui.horizontal(|ui| {
        ui.label(tl!("Color"));
        let colors: Vec<(String, &str)> = [
            ("", "None"),
            ("red", "Red"),
            ("orange", "Orange"),
            ("yellow", "Yellow"),
            ("green", "Green"),
            ("blue", "Blue"),
            ("violet", "Violet"),
            ("gray", "Gray"),
        ]
        .into_iter()
        .map(|(id, label)| (id.to_string(), label))
        .collect();
        crate::widgets::dropdown(ui, "action-color", &mut draft.color, &colors, 120.0);
    });
    ui.horizontal(|ui| {
        ui.label(tl!("Function Key"));
        let keys: Vec<(String, String)> = std::iter::once(("".into(), "None".into())).chain((1..=12).map(|n| (format!("F{n}"), format!("F{n}")))).collect();
        let refs: Vec<(String, &str)> = keys.iter().map(|(a, b)| (a.clone(), b.as_str())).collect();
        crate::widgets::dropdown(ui, "action-fkey", &mut draft.key, &refs, 80.0);
        ui.checkbox(&mut draft.shift, "Shift");
        ui.checkbox(&mut draft.ctrl, "Ctrl");
    });
    let mut close = false;
    ui.horizontal(|ui| {
        if ui.button(tl!("OK")).clicked() {
            let index = draft.index;
            let previous = app.session.actions.list.get(index).map(|action| action.name.clone());
            if let Some(previous) = previous.as_deref().filter(|name| *name != draft.name)
                && app.run("actions.rename", json!({"action": index, "name": draft.name})).is_ok()
            {
                let _ = assign_shortcut(app, previous, "");
            }
            if let Some(action) = app.session.actions.list.get(index) {
                let name = action.name.clone();
                let _ = app.run("actions.setColor", json!({"action": index, "color": draft.color}));
                let _ = assign_shortcut(app, &name, &shortcut_text(&draft.key, draft.shift, draft.ctrl));
            }
            close = true;
        }
        if ui.button(tl!("Cancel")).clicked() {
            close = true;
        }
    });
    if close {
        app.ui.actions.options = None;
    } else {
        app.ui.actions.options = Some(draft);
    }
}

fn stop_editor(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let Some(mut draft) = app.ui.actions.stop_message.clone() else { return };
    ui.separator();
    ui.label(egui::RichText::new(tl!("Record Stop")).strong());
    ui.add(egui::TextEdit::multiline(&mut draft.message).desired_width(f32::INFINITY).desired_rows(3));
    ui.checkbox(&mut draft.allow_continue, tl!("Allow Continue"));
    let mut close = false;
    ui.horizontal(|ui| {
        if ui.button(tl!("OK")).clicked() {
            if let Some(index) = app.ui.actions.selected.or_else(|| playback_selection(app)) {
                let mut params = json!({"action": index, "message": draft.message, "allowContinue": draft.allow_continue});
                if let Some(step) = app.ui.actions.selected_step {
                    params["after"] = json!(step);
                }
                let _ = app.run("actions.insertStop", params);
            }
            close = true;
        }
        if ui.button(tl!("Cancel")).clicked() {
            close = true;
        }
    });
    if close {
        app.ui.actions.stop_message = None;
    } else {
        app.ui.actions.stop_message = Some(draft);
    }
}

/// The Actions panel menu (the hamburger on the panel tab).
pub fn panel_menu(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let recording = app.session.actions.recording.is_some();
    if ui.checkbox(&mut app.ui.actions.button_mode, tl!("Button Mode")).changed() {
        ui.close();
    }
    ui.separator();
    if ui.add_enabled(!recording, egui::Button::new(tl!("New Action"))).clicked() {
        begin_recording(app, false);
        ui.close();
    }
    if ui.add_enabled(!recording, egui::Button::new(tl!("New Set"))).clicked() {
        if let Ok(v) = app.run("actions.newSet", json!({}))
            && let Some(name) = v.get("set").and_then(Value::as_str)
        {
            app.ui.actions.selected = None;
            app.ui.actions.selected_step = None;
            app.ui.actions.selected_set = Some(name.to_string());
        }
        ui.close();
    }
    if ui.add_enabled(app.ui.actions.selected.is_some() && !recording, egui::Button::new(tl!("Duplicate"))).clicked() {
        if let Some(index) = app.ui.actions.selected {
            let mut params = json!({"action": index});
            if let Some(step) = app.ui.actions.selected_step {
                params["step"] = json!(step);
            }
            let _ = app.run("actions.duplicate", params);
        }
        ui.close();
    }
    if ui
        .add_enabled(
            app.ui.actions.selected.is_some() || app.ui.actions.selected_set.as_deref().is_some_and(|s| !s.is_empty()),
            egui::Button::new(tl!("Delete")),
        )
        .clicked()
    {
        delete_selection(app);
        ui.close();
    }
    ui.separator();
    if ui.button(tl!("Play")).clicked() {
        app.ui.actions.playback = None;
        start_playback(app);
        ui.close();
    }
    if ui.add_enabled(!recording, egui::Button::new(tl!("Start Recording"))).clicked() {
        begin_recording(app, app.ui.actions.selected.is_some());
        ui.close();
    }
    if ui.add_enabled(app.ui.actions.selected_step.is_some(), egui::Button::new(tl!("Record Again"))).clicked() {
        if let (Some(action), Some(step)) = (app.ui.actions.selected, app.ui.actions.selected_step) {
            let _ = open_step_dialog(app, action, step, false);
        }
        ui.close();
    }
    if ui.add_enabled(app.ui.actions.selected.is_some() || playback_selection(app).is_some(), egui::Button::new(tl!("Insert Menu Item"))).clicked() {
        app.ui.actions.capture_menu = app.ui.actions.selected.or_else(|| playback_selection(app));
        app.ui.status = "Choose a menu item to insert into the action".into();
        app.ui.status_error = false;
        ui.close();
    }
    if ui.add_enabled(app.ui.actions.selected.is_some() || playback_selection(app).is_some(), egui::Button::new(tl!("Insert Stop"))).clicked() {
        app.ui.actions.stop_message = Some(StopDraft { message: String::new(), allow_continue: true });
        app.ui.actions.options = None;
        ui.close();
    }
    if ui.add_enabled(app.ui.actions.selected.is_some(), egui::Button::new(tl!("Action Options"))).clicked() {
        if let Some(index) = app.ui.actions.selected {
            open_options(app, index);
        }
        ui.close();
    }
    ui.separator();
    ui.label(tl!("Playback Options"));
    let mode = app.ui.actions.playback_mode;
    if ui.selectable_label(mode == PlaybackMode::Accelerated, tl!("Accelerated")).clicked() {
        app.ui.actions.playback_mode = PlaybackMode::Accelerated;
        ui.close();
    }
    if ui.selectable_label(mode == PlaybackMode::Step, tl!("Step by Step")).clicked() {
        app.ui.actions.playback_mode = PlaybackMode::Step;
        ui.close();
    }
    if ui.selectable_label(mode == PlaybackMode::Pause, tl!("Pause For")).clicked() {
        app.ui.actions.playback_mode = PlaybackMode::Pause;
        if app.ui.actions.pause_seconds <= 0.0 {
            app.ui.actions.pause_seconds = 1.0;
        }
        ui.close();
    }
    ui.add(egui::Slider::new(&mut app.ui.actions.pause_seconds, 0.1..=30.0).suffix(" s").text(tl!("Pause For")));
    ui.separator();
    if ui.add_enabled(!recording, egui::Button::new(tl!("Clear All Actions"))).clicked() {
        let _ = app.run("actions.clear", json!({}));
        app.ui.actions.selected = None;
        app.ui.actions.selected_step = None;
        app.ui.actions.selected_set = None;
        ui.close();
    }
    if ui.add_enabled(!recording, egui::Button::new(tl!("Reset Actions"))).clicked() {
        let _ = app.run("actions.reset", json!({}));
        app.ui.actions.selected = None;
        app.ui.actions.selected_step = None;
        app.ui.actions.selected_set = Some(photocraft_engine::dtf_cmds::SET_NAME.into());
        ui.close();
    }
    if ui.add_enabled(!recording, egui::Button::new(tl!("Load Actions"))).clicked() {
        load_actions(app, false);
        ui.close();
    }
    if ui.add_enabled(!recording, egui::Button::new(tl!("Replace Actions"))).clicked() {
        load_actions(app, true);
        ui.close();
    }
    if ui.button(tl!("Save Actions")).clicked() {
        save_actions(app);
        ui.close();
    }
}

pub fn panel(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let recording = app.session.actions.recording;
    let rows: Vec<_> = app.session.actions.list.iter().enumerate().map(|(i, a)| (a.name.clone(), visible_steps(app, i))).collect();
    if app
        .ui
        .actions
        .selected
        .is_some_and(|i| i >= rows.len() || app.ui.actions.selected_step.is_some_and(|step| rows.get(i).is_none_or(|row| step >= row.1.len())))
    {
        app.ui.actions.selected = None;
        app.ui.actions.selected_step = None;
    }
    if app.ui.actions.expanded.len() > rows.len() {
        app.ui.actions.expanded.truncate(rows.len());
    }
    let max_h = (ui.available_height() - 70.0).clamp(80.0, 320.0);
    let mut moved = None;
    let recording_row = recording.and_then(|(_, i)| rows.get(i).map(|(_, steps)| (i, steps.len())));
    let reveal = recording_row.filter(|row| app.ui.actions.reveal_recording != Some(*row));
    app.ui.actions.reveal_recording = recording_row;
    if let Some((i, _)) = reveal {
        app.ui.actions.expanded.resize(rows.len(), false);
        if let Some(expanded) = app.ui.actions.expanded.get_mut(i) {
            *expanded = true;
        }
    }
    let groups = grouped(&app.session.actions);
    egui::ScrollArea::vertical().id_salt("actions-rows").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
        if groups.is_empty() {
            ui.label(egui::RichText::new(tl!("Record ● a sequence of edits, then play ▶ it on any document.")).color(t.text_faint).size(11.5));
        }
        if egui::DragAndDrop::has_payload_of_type::<DragRow>(ui.ctx()) {
            let clip = ui.clip_rect();
            if let Some(p) = ui.input(|i| i.pointer.hover_pos()).filter(|p| clip.contains(*p)) {
                let delta = if p.y < clip.top() + 22.0 {
                    8.0
                } else if p.y > clip.bottom() - 22.0 {
                    -8.0
                } else {
                    0.0
                };
                if delta != 0.0 {
                    ui.scroll_with_delta(vec2(0.0, delta));
                    ui.ctx().request_repaint();
                }
            }
        }
        if app.ui.actions.button_mode {
            for (i, (name, _)) in rows.iter().enumerate() {
                let color_name = app.session.actions.list.get(i).map(|a| a.color.as_str()).unwrap_or("");
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name));
                ui.painter().rect_filled(rect.shrink(1.0), 3.0, button_color(color_name));
                ui.painter().text(pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, name, egui::FontId::proportional(13.0), Color32::WHITE);
                if let Some(key) = app.session.prefs().shortcuts.get(&shortcut_id(name)).filter(|s| !s.is_empty()) {
                    ui.painter().text(pos2(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, key, egui::FontId::proportional(11.0), Color32::WHITE);
                }
                if resp.clicked() {
                    app.ui.actions.selected = Some(i);
                    app.ui.actions.selected_step = None;
                    app.ui.actions.selected_set = Some(app.session.actions.folder(i).to_string());
                    app.ui.actions.playback = None;
                    queue_playback(app, vec![i], 0, false);
                }
            }
        }
        for (set_name, idxs) in &groups {
            if app.ui.actions.button_mode {
                continue;
            }
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click_and_drag());
            let collapsed = app.ui.actions.collapsed_sets.iter().any(|name| name == set_name);
            let set_key = if set_name == actions_cmds::UNGROUPED { "" } else { set_name.as_str() };
            let set_selected = app.ui.actions.selected.is_none() && app.ui.actions.selected_set.as_deref() == Some(set_key);
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, set_selected, set_name));
            if set_selected {
                ui.painter().rect_filled(rect, 0.0, t.row_selected);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 0.0, t.hover.gamma_multiply(0.5));
            }
            let members: Vec<&Action> = idxs.iter().filter_map(|&i| app.session.actions.list.get(i)).collect();
            let all_on = members.is_empty() || members.iter().all(|action| fully_on(action));
            let any_on = members.is_empty() || members.iter().any(|action| action.enabled);
            paint_mark(ui, pos2(rect.left() + 9.0, rect.center().y), any_on, any_on && !all_on, false, &t);
            let any_dialog = members.iter().any(|action| dialog_state(action).is_some());
            let all_dialog = !members.is_empty() && members.iter().all(|action| action.steps.is_empty() || dialog_state(action) == Some(true));
            paint_mark(ui, pos2(rect.left() + 27.0, rect.center().y), any_dialog, any_dialog && !all_dialog, true, &t);
            let tri = Rect::from_center_size(pos2(rect.left() + 48.0, rect.center().y), vec2(8.0, 8.0));
            let pts = if collapsed {
                vec![tri.left_top(), tri.right_center(), tri.left_bottom()]
            } else {
                vec![tri.left_top(), tri.right_top(), tri.center_bottom()]
            };
            ui.painter().add(egui::Shape::convex_polygon(pts, t.text_dim, Stroke::NONE));
            crate::icons::paint(ui, Rect::from_center_size(pos2(rect.left() + 66.0, rect.center().y), vec2(16.0, 16.0)), "folder", 12.0, t.icon);
            ui.painter().text(pos2(rect.left() + 80.0, rect.center().y), Align2::LEFT_CENTER, set_name, egui::FontId::proportional(12.0), t.text);
            if resp.clicked() {
                let x = resp.interact_pointer_pos().map(|p| p.x - rect.left()).unwrap_or(80.0);
                app.ui.actions.selected = None;
                app.ui.actions.selected_step = None;
                app.ui.actions.selected_set = Some(if set_name == actions_cmds::UNGROUPED { String::new() } else { set_name.clone() });
                if x < 18.0 {
                    toggle_set(app, set_key);
                } else if x < 36.0 {
                    let turn_on = idxs.iter().any(|&i| app.session.actions.list.get(i).is_some_and(|action| dialog_state(action) != Some(true)));
                    for &i in idxs {
                        let _ = app.run("actions.setDialog", json!({"action": i, "dialog": turn_on}));
                    }
                } else if x < 56.0 {
                    if collapsed {
                        app.ui.actions.collapsed_sets.retain(|name| name != set_name);
                    } else {
                        app.ui.actions.collapsed_sets.push(set_name.clone());
                    }
                }
            }
            if set_name != actions_cmds::UNGROUPED {
                resp.context_menu(|ui| {
                    if ui.button(tl!("Delete Set")).clicked() {
                        let _ = app.run("actions.deleteSet", json!({"name": set_name}));
                        if app.ui.actions.selected_set.as_deref() == Some(set_name.as_str()) {
                            app.ui.actions.selected_set = None;
                        }
                        app.ui.actions.collapsed_sets.retain(|name| name != set_name);
                        ui.close();
                    }
                });
            }
            if collapsed {
                continue;
            }
            for &i in idxs {
                let (name, steps) = &rows[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click_and_drag());
                if reveal == Some((i, 0)) {
                    ui.scroll_to_rect(rect, Some(egui::Align::BOTTOM));
                }
                let sel = app.ui.actions.selected == Some(i) && app.ui.actions.selected_step.is_none();
                resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, sel, name));
                if sel {
                    ui.painter().rect_filled(rect, 0.0, t.row_selected);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 0.0, t.hover.gamma_multiply(0.5));
                }
                if let Some(change) = drag_row(ui, &resp, DragRow { action: i, step: None }, &t) {
                    moved = Some(change);
                }
                let item = app.session.actions.list.get(i);
                let all_on = item.is_some_and(fully_on);
                let any_on = item.is_none_or(|action| action.enabled);
                paint_mark(ui, pos2(rect.left() + 9.0, rect.center().y), any_on, any_on && !all_on, false, &t);
                let dialog = item.and_then(dialog_state);
                paint_mark(ui, pos2(rect.left() + 27.0, rect.center().y), dialog.is_some(), dialog == Some(false), true, &t);
                let expanded = app.ui.actions.expanded.get(i).copied().unwrap_or(false);
                let tri = Rect::from_center_size(pos2(rect.left() + 48.0, rect.center().y), vec2(8.0, 8.0));
                let pts = if expanded {
                    vec![tri.left_top(), tri.right_top(), tri.center_bottom()]
                } else {
                    vec![tri.left_top(), tri.right_center(), tri.left_bottom()]
                };
                ui.painter().add(egui::Shape::convex_polygon(pts, t.text_dim, Stroke::NONE));
                let recording_this = recording.is_some_and(|(_, r)| r == i);
                let nsteps = steps.len();
                ui.painter().text(pos2(rect.left() + 80.0, rect.center().y), Align2::LEFT_CENTER, name, egui::FontId::proportional(12.0), t.text);
                ui.painter().text(
                    pos2(rect.right() - 8.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    app.session
                        .prefs()
                        .shortcuts
                        .get(&shortcut_id(name))
                        .filter(|s| !s.is_empty())
                        .map_or_else(|| format!("{nsteps} steps"), |key| format!("{key} · {nsteps} steps")),
                    egui::FontId::proportional(11.0),
                    t.text_faint,
                );
                if recording_this {
                    ui.painter().circle_filled(pos2(rect.right() - 64.0, rect.center().y), 4.0, Color32::from_rgb(230, 60, 60));
                }
                if resp.clicked() {
                    let x = resp.interact_pointer_pos().map(|p| p.x - rect.left()).unwrap_or(80.0);
                    app.ui.actions.selected = Some(i);
                    app.ui.actions.selected_step = None;
                    app.ui.actions.selected_set = Some(app.session.actions.folder(i).to_string());
                    if x < 18.0 {
                        toggle_include(app, i, None);
                    } else if x < 36.0 {
                        toggle_dialog(app, i, None);
                    } else if x < 56.0 {
                        if app.ui.actions.expanded.len() <= i {
                            app.ui.actions.expanded.resize(i + 1, false);
                        }
                        app.ui.actions.expanded[i] = !expanded;
                    }
                }
                resp.context_menu(|ui| {
                    if ui.button(tl!("Play")).clicked() {
                        app.ui.actions.selected = Some(i);
                        app.ui.actions.selected_step = None;
                        app.ui.actions.playback = None;
                        queue_playback(app, vec![i], 0, false);
                        ui.close();
                    }
                    if ui.button(tl!("Action Options")).clicked() {
                        open_options(app, i);
                        ui.close();
                    }
                    ui.separator();
                    ui.label(tl!("Keyboard Shortcuts"));
                    let current = app.session.prefs().shortcuts.get(&shortcut_id(name)).cloned().unwrap_or_default();
                    for key in std::iter::once(String::new()).chain((1..=12).map(|n| format!("F{n}"))) {
                        let label = if key.is_empty() { tl!("None").to_string() } else { key.clone() };
                        if ui.selectable_label(current == key, label).clicked() {
                            let _ = assign_shortcut(app, name, &key);
                            ui.close();
                        }
                    }
                    ui.separator();
                    ui.label(tl!("Folder"));
                    let here = app.session.actions.folder(i).to_string();
                    if ui.selectable_label(here.is_empty(), tl!("Ungrouped Actions")).clicked() {
                        let _ = app.run("actions.setFolder", json!({"action": i, "set": ""}));
                        ui.close();
                    }
                    for set in app.session.actions.sets.clone() {
                        if ui.selectable_label(here == set, set.clone()).clicked() {
                            let _ = app.run("actions.setFolder", json!({"action": i, "set": set}));
                            ui.close();
                        }
                    }
                });
                if resp.double_clicked() && resp.interact_pointer_pos().is_none_or(|p| p.x >= rect.left() + 56.0) {
                    open_options(app, i);
                }
                if expanded {
                    for (step, (id, params)) in steps.iter().enumerate() {
                        let (r, response) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click_and_drag());
                        if reveal == Some((i, step + 1)) {
                            ui.scroll_to_rect(r, Some(egui::Align::BOTTOM));
                        }
                        let label = step_label(id, params);
                        let selected = app.ui.actions.selected == Some(i) && app.ui.actions.selected_step == Some(step);
                        response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &label));
                        if selected {
                            ui.painter().rect_filled(r, 0.0, t.row_selected);
                        } else if response.hovered() {
                            ui.painter().rect_filled(r, 0.0, t.hover.gamma_multiply(0.5));
                        }
                        if let Some(change) = drag_row(ui, &response, DragRow { action: i, step: Some(step) }, &t) {
                            moved = Some(change);
                        }
                        let stored = committed_step_index(app, i, step);
                        let included = stored.is_none_or(|index| app.session.actions.list.get(i).is_some_and(|action| action.step_on(index)));
                        let dialog_on = stored.is_some_and(|index| app.session.actions.list.get(i).is_some_and(|action| action.step_dialog_on(index)));
                        paint_mark(ui, pos2(r.left() + 9.0, r.center().y), included, false, false, &t);
                        paint_mark(ui, pos2(r.left() + 27.0, r.center().y), dialog_on, false, true, &t);
                        let key = step_key(name, step);
                        let params_open = !app.ui.actions.collapsed_steps.iter().any(|have| have == &key);
                        let lines = if params_open { parameter_lines(id, params) } else { Vec::new() };
                        let tri = Rect::from_center_size(pos2(r.left() + 48.0, r.center().y), vec2(8.0, 8.0));
                        let pts = if params_open && !lines.is_empty() {
                            vec![tri.left_top(), tri.right_top(), tri.center_bottom()]
                        } else {
                            vec![tri.left_top(), tri.right_center(), tri.left_bottom()]
                        };
                        if !lines.is_empty() || !params_open {
                            ui.painter().add(egui::Shape::convex_polygon(pts, t.text_dim, Stroke::NONE));
                        }
                        let color = if included { t.text_dim } else { t.text_faint };
                        ui.painter().text(pos2(r.left() + 80.0, r.center().y), Align2::LEFT_CENTER, label, egui::FontId::proportional(11.5), color);
                        if response.clicked() {
                            let x = response.interact_pointer_pos().map(|p| p.x - r.left()).unwrap_or(80.0);
                            app.ui.actions.selected = Some(i);
                            app.ui.actions.selected_step = Some(step);
                            app.ui.actions.selected_set = Some(app.session.actions.folder(i).to_string());
                            if x < 18.0 {
                                if let Some(index) = stored {
                                    toggle_include(app, i, Some(index));
                                }
                            } else if x < 36.0 {
                                if let Some(index) = stored {
                                    toggle_dialog(app, i, Some(index));
                                }
                            } else if x < 56.0 {
                                if params_open {
                                    app.ui.actions.collapsed_steps.push(key);
                                } else {
                                    app.ui.actions.collapsed_steps.retain(|have| have != &key);
                                }
                            }
                        }
                        if response.double_clicked()
                            && response.interact_pointer_pos().is_none_or(|p| p.x >= r.left() + 56.0)
                            && let Some(index) = stored
                        {
                            let _ = open_step_dialog(app, i, index, false);
                        }
                        for line in &lines {
                            let (line_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
                            let indent = 92.0 + f32::from(line.depth) * 12.0;
                            ui.painter().text(
                                pos2(line_rect.left() + indent, line_rect.center().y),
                                Align2::LEFT_CENTER,
                                &line.text,
                                egui::FontId::proportional(11.0),
                                t.text_faint,
                            );
                        }
                    }
                }
            }
        }
    });
    if let Some((source, to)) = moved {
        if ui.input(|input| input.modifiers.alt) {
            duplicate_row(app, source);
        } else {
            move_row(app, source, to);
        }
    }
    if app.ui.actions.options.is_none() && app.ui.actions.stop_message.is_none() {
        step_editor(app, ui);
    }
    options_editor(app, ui);
    stop_editor(app, ui);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let recording_now = app.session.actions.recording.is_some();
        if crate::icons::button(ui, "square", 24.0, false, tl!("Stop playing/recording")).clicked() {
            if recording_now {
                let _ = app.run("actions.stop", json!({}));
            }
            if app.ui.actions.playback.take().is_some() {
                app.ui.status = "Playback stopped".into();
                app.ui.status_error = false;
            }
        }
        let (r, rec) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
        if rec.hovered() {
            ui.painter().rect_filled(r, 3.0, t.hover);
        }
        ui.painter().circle_filled(r.center(), 5.5, if recording_now { Color32::from_rgb(230, 60, 60) } else { t.icon });
        if rec.on_hover_text(tl!("Begin recording")).clicked() && !recording_now {
            begin_recording(app, true);
        }
        let play = crate::icons::button(ui, "play", 24.0, false, tl!("Play selection"));
        play.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tl!("Play selection")));
        if play.clicked() {
            start_playback(app);
        }
        if crate::icons::button(ui, "folder-plus", 24.0, false, tl!("Create new set")).clicked()
            && !recording_now
            && let Ok(v) = app.run("actions.newSet", json!({}))
            && let Some(name) = v.get("set").and_then(Value::as_str)
        {
            app.ui.actions.selected = None;
            app.ui.actions.selected_step = None;
            app.ui.actions.selected_set = Some(name.to_string());
            app.ui.actions.collapsed_sets.retain(|set| set != name);
        }
        if crate::icons::button(ui, "plus", 24.0, false, tl!("Create new action")).clicked() && !recording_now {
            begin_recording(app, false);
        }
        let set_selected = app.ui.actions.selected.is_none() && app.ui.actions.selected_set.as_deref().is_some_and(|set| !set.is_empty());
        let can_delete =
            if set_selected { !recording_now } else { app.ui.actions.selected.is_some() && (!recording_now || app.ui.actions.selected_step.is_some()) };
        ui.add_enabled_ui(can_delete, |ui| {
            let response = crate::icons::button(ui, "trash", 24.0, false, tl!("Delete"));
            response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, can_delete, tl!("Delete")));
            if response.clicked() {
                delete_selection(app);
            }
        });
    });
    pump(app, ui.ctx());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_button_targets_first_action_if_nothing_is_selected() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 32, "height": 32})).unwrap();
        assert_eq!(playback_selection(&app), None);
        app.session.actions.list.push(Action::new("Make Layer".into(), vec![("layer.new.layer".into(), json!({}))]));
        app.session.actions.list.push(Action::new("Fit".into(), vec![("view.fitOnScreen".into(), json!({}))]));
        assert_eq!(playback_selection(&app), Some(0));
        app.ui.actions.selected = Some(1);
        assert_eq!(playback_selection(&app), Some(1));
        app.ui.actions.selected = Some(999);
        assert_eq!(playback_selection(&app), Some(0));

        app.ui.actions.selected = None;
        use egui_kittest::{Harness, kittest::Queryable};
        let mut h = Harness::builder().with_size(vec2(400.0, 320.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(3);
        let before = h.state().session.active().unwrap().doc.layers.len();
        h.get_by_label("Play selection").click();
        h.run_steps(3);
        assert_eq!(h.state().session.active().unwrap().doc.layers.len(), before + 1);
        assert!(!h.state().ui.status_error);
    }

    #[test]
    fn playback_errors_are_visible_in_status_bar() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 32, "height": 32})).unwrap();
        app.session.actions.list.push(Action::new("Bad action".into(), vec![("not.a.command".into(), json!({}))]));
        let mut h = Harness::builder().with_size(vec2(400.0, 320.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(3);
        h.get_by_label("Play selection").click();
        h.run_steps(3);
        assert!(h.state().ui.status_error);
        assert!(h.state().ui.status.contains("not.a.command") || h.state().ui.status.contains("unknown command"));
    }

    #[test]
    fn recording_reveals_new_action_and_latest_step_in_a_long_panel() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width":4,"height":4})).unwrap();
        for i in 0..30 {
            app.session.actions.list.push(Action::new(format!("Existing {i}"), vec![]));
        }
        let mut h = Harness::builder().with_size(vec2(320.0, 260.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(4);
        begin_recording(h.state_mut(), false);
        h.run_steps(20);
        let rect = h.get_by_label("Action 31").rect();
        assert!(rect.top() >= 0.0 && rect.bottom() <= 205.0, "new action visible: {rect:?}");
        for _ in 0..20 {
            h.state_mut().run("view.zoomOut", json!({})).unwrap();
        }
        h.state_mut().run("view.fitOnScreen", json!({})).unwrap();
        h.run_steps(20);
        let rect = h.get_by_label("Fit on Screen").rect();
        assert!(rect.top() >= 0.0 && rect.bottom() <= 205.0, "latest step visible: {rect:?}");
    }

    #[test]
    fn drag_steps_and_action_headers_reorders_and_keeps_selection() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.session.actions.list.push(Action::new(
            "First".into(),
            vec![("view.zoomOut".into(), json!({})), ("view.fitOnScreen".into(), json!({})), ("view.actualPixels".into(), json!({}))],
        ));
        app.session.actions.list.push(Action::new("Second".into(), vec![]));
        app.ui.actions.expanded = vec![true, false];
        let mut h = Harness::builder().with_size(vec2(320.0, 400.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(4);
        fn drag(h: &mut Harness<'_, PhotocraftApp>, from: egui::Pos2, to: egui::Pos2) {
            h.event(egui::Event::PointerMoved(from));
            h.run_steps(1);
            h.event(egui::Event::PointerButton { pos: from, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
            h.run_steps(1);
            h.event(egui::Event::PointerMoved(from + vec2(10.0, 0.0)));
            h.run_steps(2);
            h.event(egui::Event::PointerMoved(to));
            h.run_steps(2);
            h.event(egui::Event::PointerButton { pos: to, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
            h.run_steps(3);
        }
        let from = h.get_by_label("Zoom Out").rect().center();
        let target = h.get_by_label("100%").rect();
        drag(&mut h, from, pos2(target.center().x, target.bottom() - 2.0));
        assert_eq!(
            h.state().session.actions.list[0].steps.iter().map(|st| st.0.as_str()).collect::<Vec<_>>(),
            ["view.fitOnScreen", "view.actualPixels", "view.zoomOut"]
        );
        assert_eq!(h.state().ui.actions.selected_step, Some(2));
        let from = h.get_by_label("Second").rect().center();
        let target = h.get_by_label("First").rect();
        drag(&mut h, from, pos2(target.center().x, target.top() + 2.0));
        assert_eq!(h.state().session.actions.list[0].name, "Second");
        assert_eq!(h.state().ui.actions.expanded, [false, true]);
        assert_eq!(h.state().ui.actions.selected, Some(0));
    }

    #[test]
    fn nested_shell_actions_include_view_steps_and_propagate_failures() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width":40,"height":40})).unwrap();
        app.session.actions.list.push(Action::new("Child".into(), vec![("layer.new.layer".into(), json!({})), ("view.fitOnScreen".into(), json!({}))]));
        app.run("actions.record", json!({"name":"Parent"})).unwrap();
        app.run("actions.play", json!({"action":"Child"})).unwrap();
        app.run("actions.stop", json!({})).unwrap();
        assert_eq!(app.session.actions.list[1].steps, [("actions.play".into(), json!({"action":"Child"}))]);
        assert_eq!(step_label("actions.play", &json!({"action":"Child"})), "Play Action: Child");
        app.run("view.actualPixels", json!({})).unwrap();
        let r = app.run("actions.play", json!({"action":"Parent"})).unwrap();
        assert!(r.get("failed").is_none(), "{r}");
        assert!(app.ui.views[0].fit_pending);
        app.session.authorize =
            Some(|id, _| if id == "view.fitOnScreen" { Err(photocraft_engine::EngineError::Other("denied nested view".into())) } else { Ok(()) });
        let r = app.run("actions.play", json!({"action":"Parent"})).unwrap();
        assert!(r["failed"]["error"].as_str().unwrap().contains("denied nested view"));
        assert!(app.session.actions.playback_stack.is_empty());
        assert_eq!(app.session.actions.playing, 0);
    }

    #[test]
    fn panel_shows_pending_steps_and_trash_deletes_only_the_clicked_step() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 40, "height": 40})).unwrap();
        begin_recording(&mut app, false);
        app.run("view.fitOnScreen", json!({})).unwrap();
        app.run("view.zoomOut", json!({})).unwrap();
        let mut h = Harness::builder().with_size(vec2(320.0, 400.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(2);
        assert!(h.state().session.actions.recording.is_some());
        h.get_by_label("Fit on Screen");
        h.get_by_label("Zoom Out").click();
        h.run_steps(2);
        assert_eq!(h.state().ui.actions.selected_step, Some(1));
        h.get_by_label("Delete").click();
        h.run_steps(2);
        assert_eq!(visible_steps(h.state(), 0).iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["view.fitOnScreen"]);
        assert_eq!(h.state().session.actions.list.len(), 1);
        assert!(h.state().session.actions.recording.is_some());
        assert!(h.state().ui.actions.selected.is_none());
        delete_selection(h.state_mut());
        assert_eq!(h.state().session.actions.list.len(), 1, "repeated trash cannot delete the action");
        h.state_mut().run("view.zoomIn", json!({})).unwrap();
        h.run_steps(2);
        h.get_by_label("Zoom In");
        h.state_mut().run("actions.stop", json!({})).unwrap();
        h.run_steps(2);
        assert_eq!(visible_steps(h.state(), 0).iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["view.fitOnScreen", "view.zoomIn"]);
        h.get_by_label("Zoom In").click();
        h.run_steps(2);
        h.get_by_label("Delete").click();
        h.run_steps(2);
        assert_eq!(visible_steps(h.state(), 0).iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["view.fitOnScreen"]);
    }

    #[test]
    fn view_menu_commands_record_append_and_replay_after_resize() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 160, "height": 100})).unwrap();
        begin_recording(&mut app, false);
        app.run("image.canvasSize", json!({"width": 80, "height": 60})).unwrap();
        app.run("actions.stop", json!({})).unwrap();
        begin_recording(&mut app, true);
        crate::menus::invoke(&mut app, &ctx, "view.fitOnScreen", json!({})).unwrap();
        app.run("actions.stop", json!({})).unwrap();
        assert_eq!(app.session.actions.list.len(), 1);
        assert_eq!(app.session.actions.list[0].steps.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["image.canvasSize", "view.fitOnScreen"]);
        assert_eq!(label_of("view.fitOnScreen"), "Fit on Screen");
        // Persisted actions retain the menu step, and playback fits the new document.
        let saved = serde_json::to_string(&app.session.actions.list).unwrap();
        app.session.actions.list = serde_json::from_str(&saved).unwrap();
        app.run("file.new", json!({"width": 200, "height": 140})).unwrap();
        app.run("view.actualPixels", json!({})).unwrap();
        let result = app.run("actions.play", json!({"action": 0})).unwrap();
        assert_eq!(result["ran"], 2);
        assert!(result.get("failed").is_none(), "{result}");
        let idx = app.session.active_index().unwrap();
        assert!(app.ui.views[idx].fit_pending);
        let doc = &app.session.active().unwrap().doc;
        crate::canvas::fit_view(&mut app.ui.views[idx], doc, vec2(100.0, 100.0), 1.0);
        assert_eq!(app.ui.views[idx].zoom, 0.75);
        assert_eq!(app.ui.views[idx].center, [40.0, 30.0]);
        assert!(!app.ui.views[idx].fit_pending);
    }

    #[test]
    fn view_action_failure_resume_and_authorization() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.session.actions.list.push(Action::new("Fit".into(), vec![("view.fitOnScreen".into(), json!({})), ("view.actualPixels".into(), json!({}))]));
        let result = app.run("actions.play", json!({"action": "Fit"})).unwrap();
        assert_eq!(result["ran"], 0);
        assert_eq!(result["failed"]["step"], 0);
        assert_eq!(app.session.actions.playing, 0);
        app.run("file.new", json!({"width": 40, "height": 40})).unwrap();
        app.session.authorize =
            Some(
                |id, _| {
                    if id == "view.fitOnScreen" { Err(photocraft_engine::EngineError::BadParams { cmd: id.into(), msg: "denied view".into() }) } else { Ok(()) }
                },
            );
        let result = app.run("actions.play", json!({"action": 0})).unwrap();
        assert_eq!(result["ran"], 0);
        assert!(result["failed"]["error"].as_str().unwrap().contains("denied view"));
        let result = app.run("actions.play", json!({"action": 0, "from": 1})).unwrap();
        assert_eq!(result["ran"], 1);
        assert!(!app.ui.views[0].fit_pending);
        assert!(app.run("actions.play", json!({"action": 0, "from": -1})).is_err());
        app.session.actions.playing = 1;
        assert!(app.run("actions.play", json!({"action": 0})).is_err());
    }

    #[test]
    fn record_and_replay_on_another_document() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 40, "height": 40})).unwrap();
        app.sync_views();
        app.run("actions.record", json!({})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("select.rect", json!({"x": 0, "y": 0, "width": 10, "height": 10})).unwrap();
        app.run("edit.fill", json!({"color": "#ff0000"})).unwrap();
        let _ = app.run("document.pixel", json!({"x": 1, "y": 1}));
        app.run("actions.stop", json!({})).unwrap();
        let a = &app.session.actions.list[0];
        assert_eq!(a.steps.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["layer.new.layer", "select.rect", "edit.fill"]);
        app.run("file.new", json!({"width": 40, "height": 40})).unwrap();
        app.sync_views();
        let played = app.run("actions.play", json!({"action": 0})).unwrap();
        assert_eq!(played["ran"], 3);
        assert!(played.get("failed").is_none(), "{played}");
        let d = &app.session.active().unwrap().doc;
        assert_eq!(d.layers.len(), 2);
        assert_eq!(d.layers[1].surface().unwrap().pixel(5, 5), vec![1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn folders_group_actions_and_the_selected_step_can_be_edited() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.session.actions.sets.push("DTF".into());
        app.session.actions.list.push(Action::new("Spot".into(), vec![("image.dtf".into(), json!({"contract": 1, "name": "Spot Color 1", "solidity": 60}))]));
        app.session.actions.folders.push("DTF".into());
        app.session.actions.list.push(Action::new("Mine".into(), vec![]));
        app.ui.actions.expanded = vec![true, false];
        let mut h = Harness::builder().with_size(vec2(420.0, 420.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(3);
        h.get_by_label("DTF");
        h.get_by_label("Ungrouped Actions");
        h.get_by_label("Mine");
        h.get_by_label("DTF Spot Channel (1 px, Spot Color 1)").click();
        h.run_steps(2);
        h.get_by_label("Contract");
        h.get_by_label("Name");
        assert_eq!(h.state().ui.actions.selected, Some(0));
        assert_eq!(h.state().ui.actions.selected_step, Some(0));
        h.get_by_label("DTF").click();
        h.run_steps(2);
        assert_eq!(h.state().ui.actions.selected, None);
        assert_eq!(h.state().ui.actions.selected_set.as_deref(), Some("DTF"));
        h.get_by_label("Delete").click();
        h.run_steps(2);
        assert!(h.state().session.actions.sets.iter().all(|name| name != "DTF"));
        assert!(h.state().session.actions.folder(0).is_empty(), "deleting the folder ungroups its action");
        assert_eq!(h.state().session.actions.list[0].name, "Spot");
    }

    #[test]
    fn dtf_step_reads_like_the_photoshop_action() {
        let lines =
            parameter_lines("image.dtf", &json!({"contract": 1, "name": "Spot Color 1", "color": "#ff0000", "solidity": 60, "applyAtCanvasBounds": false}));
        let text: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        for expected in [
            "Set Selection",
            "To: transparency channel",
            "Contract",
            "By: 1 pixels",
            "Without Effect At Canvas Bounds",
            "Make",
            "New: spot color channel",
            "Name: \"Spot Color 1\"",
            "Color: RGB color",
            "Red: 255",
            "Green: 0",
            "Blue: 0",
            "Solidity: 60",
        ] {
            assert!(text.contains(&expected), "{expected} missing from {text:?}");
        }
    }

    #[test]
    fn button_mode_plays_the_dtf_action_in_one_click() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 16, "height": 16})).unwrap();
        app.session.actions.sets.push("DTF".into());
        app.session
            .actions
            .list
            .push(Action::new("DTF".into(), vec![("image.dtf".into(), json!({"contract": 1, "name": "Spot Color 1", "color": "#ff0000", "solidity": 60}))]));
        app.session.actions.folders.push("DTF".into());
        app.session.actions.list[0].color = "red".into();
        app.ui.actions.button_mode = true;
        let mut h = Harness::builder().with_size(vec2(360.0, 280.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(3);
        h.get_by_label("DTF").click();
        h.run_steps(4);
        assert!(!h.state().ui.status_error, "{}", h.state().ui.status);
        assert!(h.state().session.active().unwrap().doc.channels.iter().any(|c| c.name == "Spot Color 1" && c.spot.is_some()));
    }

    #[test]
    fn insert_menu_item_records_without_running_and_rejects_action_edits() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 16, "height": 16})).unwrap();
        app.session.actions.list.push(Action::new("Rec".into(), vec![]));
        app.ui.actions.selected = Some(0);
        app.ui.actions.capture_menu = Some(0);
        let before = app.session.active().unwrap().doc.layers.len();
        crate::menus::invoke(&mut app, &ctx, "layer.new.layer", json!({})).unwrap();
        assert_eq!(app.session.active().unwrap().doc.layers.len(), before);
        assert_eq!(app.session.actions.list[0].steps[0].0, "layer.new.layer");
        assert!(app.ui.actions.capture_menu.is_none());
        app.ui.actions.capture_menu = Some(0);
        assert!(crate::menus::invoke(&mut app, &ctx, "actions.delete", json!({"action": 0})).is_err());
        assert_eq!(app.ui.actions.capture_menu, Some(0));
        assert_eq!(app.session.actions.list.len(), 1);
    }
}
