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
    #[serde(skip)]
    reveal_recording: Option<(usize, usize)>,
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
    let recording = actions_cmds::begin_playback(&mut app.session, &action).map_err(|e| e.to_string())?;
    let mut ran = 0u64;
    let mut failed = None;
    for (i, (id, p)) in action.steps.iter().enumerate().skip(from) {
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
    let Ok(v) = app.run("actions.record", params) else { return };
    let Some(i) = v.get("index").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()) else { return };
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
    app.session.actions.list.get(action).into_iter().flat_map(|a| a.steps.iter()).chain(actions_cmds::pending_steps(&app.session, action)).cloned().collect()
}

fn step_label(id: &str, params: &Value) -> String {
    if id == "image.dtf" {
        let contract = params.get("contract").and_then(Value::as_f64).unwrap_or(f64::from(photocraft_engine::dtf_cmds::DEFAULT_CONTRACT));
        let name = params.get("name").and_then(Value::as_str).unwrap_or(photocraft_engine::dtf_cmds::DEFAULT_NAME);
        let contract = if contract.fract() == 0.0 && contract.abs() < 1.0e6 { format!("{}", contract as i64) } else { format!("{contract}") };
        return format!("DTF Spot Channel ({contract} px, {name})");
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
                    if edited
                        && let Some(obj) = updated.as_object_mut()
                    {
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
    let mut play_idx = None;
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
        for (set_name, idxs) in &groups {
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
            let tri = Rect::from_center_size(pos2(rect.left() + 12.0, rect.center().y), vec2(8.0, 8.0));
            let pts = if collapsed { vec![tri.left_top(), tri.right_center(), tri.left_bottom()] } else { vec![tri.left_top(), tri.right_top(), tri.center_bottom()] };
            ui.painter().add(egui::Shape::convex_polygon(pts, t.text_dim, Stroke::NONE));
            crate::icons::paint(ui, Rect::from_center_size(pos2(rect.left() + 30.0, rect.center().y), vec2(16.0, 16.0)), "folder", 12.0, t.icon);
            ui.painter().text(pos2(rect.left() + 44.0, rect.center().y), Align2::LEFT_CENTER, set_name, egui::FontId::proportional(12.0), t.text);
            if resp.clicked() {
                app.ui.actions.selected = None;
                app.ui.actions.selected_step = None;
                app.ui.actions.selected_set = Some(if set_name == actions_cmds::UNGROUPED { String::new() } else { set_name.clone() });
                if resp.interact_pointer_pos().is_some_and(|p| p.x < rect.left() + 20.0) {
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
            let expanded = app.ui.actions.expanded.get(i).copied().unwrap_or(false);
            let tri = Rect::from_center_size(pos2(rect.left() + 12.0, rect.center().y), vec2(8.0, 8.0));
            let pts =
                if expanded { vec![tri.left_top(), tri.right_top(), tri.center_bottom()] } else { vec![tri.left_top(), tri.right_center(), tri.left_bottom()] };
            ui.painter().add(egui::Shape::convex_polygon(pts, t.text_dim, Stroke::NONE));
            crate::icons::paint(ui, Rect::from_center_size(pos2(rect.left() + 30.0, rect.center().y), vec2(16.0, 16.0)), "play", 11.0, t.icon);
            let recording_this = recording.is_some_and(|(_, r)| r == i);
            let nsteps = steps.len();
            ui.painter().text(pos2(rect.left() + 44.0, rect.center().y), Align2::LEFT_CENTER, name, egui::FontId::proportional(12.0), t.text);
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
                app.ui.actions.selected = Some(i);
                app.ui.actions.selected_step = None;
                app.ui.actions.selected_set = Some(app.session.actions.folder(i).to_string());
                if resp.interact_pointer_pos().is_some_and(|p| p.x < rect.left() + 20.0) {
                    if app.ui.actions.expanded.len() <= i {
                        app.ui.actions.expanded.resize(i + 1, false);
                    }
                    app.ui.actions.expanded[i] = !expanded;
                }
            }
            resp.context_menu(|ui| {
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
            if resp.double_clicked() {
                play_idx = Some(i);
            }
            if expanded {
                for (step, (id, params)) in steps.iter().enumerate() {
                    let (r, response) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click_and_drag());
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
                    ui.painter().text(pos2(r.left() + 44.0, r.center().y), Align2::LEFT_CENTER, label, egui::FontId::proportional(11.5), t.text_dim);
                    if response.clicked() {
                        app.ui.actions.selected = Some(i);
                        app.ui.actions.selected_step = Some(step);
                    }
                }
            }
            }
        }
    });
    if let Some((source, to)) = moved {
        move_row(app, source, to);
    }
    step_editor(app, ui);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let recording_now = app.session.actions.recording.is_some();
        if crate::icons::button(ui, "square", 24.0, false, tl!("Stop playing/recording")).clicked() && recording_now {
            let _ = app.run("actions.stop", json!({}));
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
            play_idx = playback_selection(app);
        }
        if crate::icons::button(ui, "plus", 24.0, false, tl!("Create new action")).clicked() && !recording_now {
            begin_recording(app, false);
        }
        if crate::icons::button(ui, "folder-plus", 24.0, false, tl!("Create new set")).clicked() && !recording_now
            && let Ok(v) = app.run("actions.newSet", json!({}))
            && let Some(name) = v.get("set").and_then(Value::as_str)
        {
            app.ui.actions.selected = None;
            app.ui.actions.selected_step = None;
            app.ui.actions.selected_set = Some(name.to_string());
            app.ui.actions.collapsed_sets.retain(|set| set != name);
        }
        let set_selected = app.ui.actions.selected.is_none() && app.ui.actions.selected_set.as_deref().is_some_and(|set| !set.is_empty());
        let can_delete = if set_selected { !recording_now } else { app.ui.actions.selected.is_some() && (!recording_now || app.ui.actions.selected_step.is_some()) };
        ui.add_enabled_ui(can_delete, |ui| {
            let response = crate::icons::button(ui, "trash", 24.0, false, tl!("Delete"));
            response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, can_delete, tl!("Delete")));
            if response.clicked() {
                delete_selection(app);
            }
        });
    });
    if let Some(i) = play_idx {
        match app.run("actions.play", json!({"action": i})) {
            Ok(v) => report_play(app, &v),
            Err(e) => {
                app.ui.status = format!("Action playback failed: {e}");
                app.ui.status_error = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_button_targets_first_action_if_nothing_is_selected() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 32, "height": 32})).unwrap();
        assert_eq!(playback_selection(&app), None);
        app.session.actions.list.push(Action { name: "Make Layer".into(), steps: vec![("layer.new.layer".into(), json!({}))] });
        app.session.actions.list.push(Action { name: "Fit".into(), steps: vec![("view.fitOnScreen".into(), json!({}))] });
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
        app.session.actions.list.push(Action { name: "Bad action".into(), steps: vec![("not.a.command".into(), json!({}))] });
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
            app.session.actions.list.push(Action { name: format!("Existing {i}"), steps: vec![] });
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
        app.session.actions.list.push(Action {
            name: "First".into(),
            steps: vec![("view.zoomOut".into(), json!({})), ("view.fitOnScreen".into(), json!({})), ("view.actualPixels".into(), json!({}))],
        });
        app.session.actions.list.push(Action { name: "Second".into(), steps: vec![] });
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
        app.session
            .actions
            .list
            .push(Action { name: "Child".into(), steps: vec![("layer.new.layer".into(), json!({})), ("view.fitOnScreen".into(), json!({}))] });
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
        app.session
            .actions
            .list
            .push(Action { name: "Fit".into(), steps: vec![("view.fitOnScreen".into(), json!({})), ("view.actualPixels".into(), json!({}))] });
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
        app.session.actions.list.push(Action {
            name: "Spot".into(),
            steps: vec![("image.dtf".into(), json!({"contract": 1, "name": "Spot Color 1", "solidity": 60}))],
        });
        app.session.actions.folders.push("DTF".into());
        app.session.actions.list.push(Action { name: "Mine".into(), steps: vec![] });
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
}
