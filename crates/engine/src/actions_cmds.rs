//! Window › Actions, as engine commands (`actions.record` / `stop` / `play` / `list` / `get` /
//! `delete`).
//!
//! The list lives on [`Session`] so the panel, the CLI, the control channel and MCP share one
//! copy. Recording copies replayable journal entries (commands whose [`CommandSpec::journal`] is
//! set, except action-editing commands and the already-expanded `edit.transform.again`) into an
//! action. Playback runs those steps with [`Session::execute`]
//! and stops at the first error, leaving one history step per step that ran.
//!
//! An untrusted session installs [`Session::authorize`]. `actions.play` calls it for every nested
//! step, because the outer `actions.play` id would otherwise hide a recorded `file.*` command from
//! the top-level check.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn default_true() -> bool {
    true
}

/// One recorded action. `steps` is the `[[id, params], …]` shape `file.automate.batch` and
/// droplets already accept.
///
/// `enabled` is the action's checkbox. A step missing from [`Self::step_enabled`] is included.
/// A step missing from [`Self::step_dialog`] plays with its recorded values and does not open a
/// dialog. Older action files omit these fields and load as fully enabled, silent playback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub name: String,
    #[serde(default)]
    pub steps: Vec<(String, Value)>,
    /// Included when its set plays. Direct Play still runs the action.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Button-mode colour: `""`, `red`, `orange`, `yellow`, `green`, `blue`, `violet`, `gray`.
    #[serde(default)]
    pub color: String,
    /// Parallel to `steps`. Missing entries are included.
    #[serde(default)]
    pub step_enabled: Vec<bool>,
    /// Parallel to `steps`. Missing entries do not open a dialog.
    #[serde(default)]
    pub step_dialog: Vec<bool>,
}

impl Default for Action {
    fn default() -> Self {
        Self { name: String::new(), steps: Vec::new(), enabled: true, color: String::new(), step_enabled: Vec::new(), step_dialog: Vec::new() }
    }
}

impl Action {
    /// An included action with no button colour. Every step plays, and none opens a dialog.
    pub fn new(name: String, steps: Vec<(String, Value)>) -> Self {
        Self { name, steps, ..Self::default() }
    }

    /// The step's checkbox. An index past the flag list is included.
    pub fn step_on(&self, index: usize) -> bool {
        self.step_enabled.get(index).copied().unwrap_or(true)
    }

    /// The step's dialog toggle. An index past the flag list stays off.
    pub fn step_dialog_on(&self, index: usize) -> bool {
        self.step_dialog.get(index).copied().unwrap_or(false)
    }
}

/// Button-mode colours Photoshop offers, in panel order. `""` is None.
pub const BUTTON_COLORS: [&str; 8] = ["", "red", "orange", "yellow", "green", "blue", "violet", "gray"];

/// Photoshop's colour index: 0 none, then red through gray.
pub fn color_from_index(index: u16) -> &'static str {
    BUTTON_COLORS.get(usize::from(index)).copied().unwrap_or("")
}

fn align_step_flags(action: &mut Action) {
    let n = action.steps.len();
    if action.step_enabled.len() < n {
        action.step_enabled.resize(n, true);
    } else if action.step_enabled.len() > n {
        action.step_enabled.truncate(n);
    }
    if action.step_dialog.len() < n {
        action.step_dialog.resize(n, false);
    } else if action.step_dialog.len() > n {
        action.step_dialog.truncate(n);
    }
}

fn insert_step(action: &mut Action, index: usize, step: (String, Value), dialog: bool) {
    align_step_flags(action);
    let index = index.min(action.steps.len());
    action.steps.insert(index, step);
    action.step_enabled.insert(index, true);
    action.step_dialog.insert(index, dialog);
}

fn remove_step_at(action: &mut Action, index: usize) {
    align_step_flags(action);
    if index < action.steps.len() {
        action.steps.remove(index);
        action.step_enabled.remove(index);
        action.step_dialog.remove(index);
    }
}

fn move_step_at(action: &mut Action, from: usize, to: usize) {
    align_step_flags(action);
    if from >= action.steps.len() || to >= action.steps.len() {
        return;
    }
    let step = action.steps.remove(from);
    let enabled = action.step_enabled.remove(from);
    let dialog = action.step_dialog.remove(from);
    action.steps.insert(to, step);
    action.step_enabled.insert(to, enabled);
    action.step_dialog.insert(to, dialog);
}

/// On-disk form of the Actions list (`actions.json` in the preset store).
///
/// Version 1 is a flat list: every action is ungrouped. Version 2 adds `sets` (folder names,
/// including empty folders) and `folders` (the folder of each action, parallel to `actions`;
/// an empty string is ungrouped).
pub const ACTIONS_VERSION: u32 = 2;

/// The panel label for actions that are not in a folder.
pub const UNGROUPED: &str = "Ungrouped Actions";

const MAX_SET_CHARS: usize = 256;
const MAX_SETS: usize = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionsFile {
    pub version: u32,
    pub actions: Vec<Action>,
    /// Named folders, in panel order. Version 1 files omit this.
    #[serde(default)]
    pub sets: Vec<String>,
    /// Folder of each action, parallel to `actions`. Empty string means ungrouped.
    /// Version 1 files omit this.
    #[serde(default)]
    pub folders: Vec<String>,
}

/// Actions list and the recording / playback flags. Only `list` is persisted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionState {
    pub list: Vec<Action>,
    /// Journal length when recording started, and the action index being recorded into.
    #[serde(skip)]
    pub recording: Option<(usize, usize)>,
    /// Nesting depth of `actions.play`, bounded by the playback stack limit.
    #[serde(skip)]
    pub playing: u8,
    /// Active action names, for cycle detection in engine and desktop playback.
    #[serde(skip)]
    pub playback_stack: Vec<String>,
    /// Bumped whenever `list` changes, so the preset store can skip unchanged writes.
    #[serde(skip)]
    pub rev: u64,
    /// Named folders, in panel order. Empty folders stay so a set can exist before it has an action.
    #[serde(default)]
    pub sets: Vec<String>,
    /// Folder name for each action, parallel to `list`. Empty string means ungrouped.
    /// Shorter than `list` when an action was pushed without a folder.
    #[serde(default)]
    pub folders: Vec<String>,
    /// While recording, new steps are inserted after this step. `None` appends.
    #[serde(skip)]
    pub recording_after: Option<usize>,
}

impl ActionState {
    pub(crate) fn touch(&mut self) {
        self.rev = self.rev.saturating_add(1);
    }

    /// Folder of action `index`. A missing entry is ungrouped.
    pub fn folder(&self, index: usize) -> &str {
        self.folders.get(index).map(String::as_str).unwrap_or("")
    }

    /// Keep `folders` the same length as `list`. New entries are ungrouped.
    pub(crate) fn align_folders(&mut self) {
        if self.folders.len() < self.list.len() {
            self.folders.resize(self.list.len(), String::new());
        } else if self.folders.len() > self.list.len() {
            self.folders.truncate(self.list.len());
        }
    }
}

/// Add `name` to the folder list when it is not already there. Does not enforce [`MAX_SETS`]:
/// the built-in DTF folder must still appear. [`new_set`] checks the cap.
pub(crate) fn ensure_set(state: &mut ActionState, name: &str) -> Result<()> {
    if name.is_empty() {
        return Ok(());
    }
    if name.chars().count() > MAX_SET_CHARS {
        return Err(bad("actions.newSet", format!("folder name must be at most {MAX_SET_CHARS} characters")));
    }
    if state.sets.iter().any(|n| n == name) {
        return Ok(());
    }
    state.sets.push(name.to_string());
    state.touch();
    Ok(())
}

/// Append an action and its folder. `set` empty means ungrouped.
pub(crate) fn push_action(s: &mut Session, action: Action, set: &str) -> Result<usize> {
    if !set.is_empty() {
        ensure_set(&mut s.actions, set)?;
    }
    s.actions.align_folders();
    s.actions.list.push(action);
    s.actions.folders.push(set.to_string());
    s.actions.touch();
    Ok(s.actions.list.len() - 1)
}

/// Replay commands and named action calls, excluding edits to the action definitions and the
/// already-expanded Again wrapper.
pub fn replayable(id: &str) -> bool {
    // Again journals its concrete transform too; recording the wrapper would replay it twice.
    id == "actions.play"
        || (!id.starts_with("actions.") && id != "edit.transform.again" && (shell_view_command(id) || crate::commands::find(id).is_some_and(|c| c.journal)))
}

/// View-only menu commands recorded by the desktop shell. Headless playback reports
/// them as unsupported instead of silently dropping them.
pub fn shell_view_command(id: &str) -> bool {
    matches!(id, "view.fitOnScreen" | "view.actualPixels" | "view.zoomIn" | "view.zoomOut")
}

/// Shared validation for headless and shell playback. Does not start playback.
pub fn playback_plan(s: &Session, p: &Value) -> Result<(Action, usize)> {
    if s.actions.playing > 0 && s.actions.playback_stack.is_empty() {
        return Err(bad("actions.play", "an action is already playing"));
    }
    let idx = resolve(&s.actions.list, p, "actions.play")?;
    let action = s.actions.list.get(idx).cloned().ok_or_else(|| bad("actions.play", "no such action"))?;
    if s.actions.playback_stack.iter().any(|name| name == &action.name) || s.actions.recording.is_some_and(|(_, i)| i == idx) {
        return Err(bad("actions.play", format!("recursive action call to `{}`", action.name)));
    }
    if s.actions.playback_stack.len() >= 16 {
        return Err(bad("actions.play", "action nesting exceeds 16 levels"));
    }
    let from = from_step(p, action.steps.len())?;
    Ok((action, from))
}

fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}

fn num(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f.round() as i64))
}

fn resolve(list: &[Action], p: &Value, cmd: &str) -> Result<usize> {
    let Some(v) = p.get("action") else {
        return Err(bad(cmd, "needs \"action\" (a name or an index)"));
    };
    if let Some(i) = num(v) {
        if i < 0 {
            return Err(bad(cmd, "action index is negative"));
        }
        let Some(i) = usize::try_from(i).ok() else {
            return Err(bad(cmd, "action index is out of range"));
        };
        if i >= list.len() {
            return Err(bad(cmd, format!("no action at index {i}")));
        }
        return Ok(i);
    }
    match v.as_str() {
        Some(name) if !name.is_empty() => list.iter().position(|a| a.name == name).ok_or_else(|| bad(cmd, format!("no action named `{name}`"))),
        Some(_) => Err(bad(cmd, "action name is empty")),
        None => Err(bad(cmd, "\"action\" must be a name or an index")),
    }
}

fn from_step(p: &Value, len: usize) -> Result<usize> {
    let Some(v) = p.get("from") else { return Ok(0) };
    if v.is_null() {
        return Ok(0);
    }
    let Some(i) = num(v) else {
        return Err(bad("actions.play", "\"from\" must be a step index"));
    };
    if i < 0 {
        return Err(bad("actions.play", "\"from\" is negative"));
    }
    let Some(i) = usize::try_from(i).ok() else {
        return Err(bad("actions.play", "\"from\" is out of range"));
    };
    if i > len {
        return Err(bad("actions.play", format!("\"from\" {i} is past the last step ({len})")));
    }
    Ok(i)
}

fn list(s: &mut Session, _p: &Value) -> Result<Value> {
    s.actions.align_folders();
    let actions: Vec<Value> =
        s.actions.list.iter().enumerate().map(|(i, a)| json!({"name": a.name, "set": s.actions.folder(i), "steps": a.steps.len()})).collect();
    let sets = s.actions.sets.clone();
    let recording = s.actions.recording.and_then(|(_, i)| s.actions.list.get(i).map(|a| a.name.clone()));
    Ok(json!({"actions": actions, "sets": sets, "recording": recording}))
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    s.actions.align_folders();
    let idx = resolve(&s.actions.list, p, "actions.get")?;
    let set = s.actions.folder(idx).to_string();
    let action = &s.actions.list[idx];
    let steps: Vec<Value> = action.steps.iter().map(|(id, params)| json!([id, params])).collect();
    Ok(json!({"name": action.name, "set": set, "steps": steps}))
}

fn record(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.record")?;
    if s.actions.recording.is_some() {
        return Err(bad("actions.record", "already recording; stop first"));
    }
    let idx = if p.get("action").is_some() {
        resolve(&s.actions.list, p, "actions.record")?
    } else {
        let name = match p.get("name") {
            None | Some(Value::Null) => {
                let mut n = s.actions.list.len().saturating_add(1);
                while s.actions.list.iter().any(|a| a.name == format!("Action {n}")) {
                    n = n.checked_add(1).ok_or_else(|| bad("actions.record", "cannot allocate a unique action name"))?;
                }
                format!("Action {n}")
            }
            Some(Value::String(n)) if !n.is_empty() => n.clone(),
            Some(Value::String(_)) => return Err(bad("actions.record", "\"name\" is empty")),
            Some(_) => return Err(bad("actions.record", "\"name\" must be a string")),
        };
        if s.actions.list.iter().any(|action| action.name == name) {
            return Err(bad("actions.record", "an action with this name already exists"));
        }
        let set = folder_name(p, "actions.record")?;
        if !set.is_empty() && !s.actions.sets.iter().any(|n| n == &set) && s.actions.sets.len() >= MAX_SETS {
            return Err(bad("actions.record", format!("at most {MAX_SETS} folders")));
        }
        push_action(s, Action::new(name, Vec::new()), &set)?
    };
    s.actions.recording_after = None;
    s.actions.recording = Some((s.journal.len(), idx));
    let name = s.actions.list.get(idx).map(|a| a.name.clone()).unwrap_or_default();
    Ok(json!({"action": name, "index": idx}))
}

/// Replayable journal entries not yet committed to this action while recording.
pub fn pending_steps(s: &Session, action: usize) -> impl Iterator<Item = &(String, Value)> {
    let from = s.actions.recording.filter(|(_, i)| *i == action).map_or(s.journal.len(), |(from, _)| from);
    s.journal.iter().skip(from).filter(|(id, _)| replayable(id))
}

fn flush_recording(s: &mut Session) -> Result<()> {
    let Some((_, idx)) = s.actions.recording else { return Ok(()) };
    if idx >= s.actions.list.len() {
        return Err(bad("actions.stop", "the action being recorded no longer exists"));
    }
    let steps: Vec<_> = pending_steps(s, idx).cloned().collect();
    s.actions.recording = Some((s.journal.len(), idx));
    if !steps.is_empty() {
        let after = s.actions.recording_after;
        if let Some(after) = after {
            let mut at = after.saturating_add(1);
            for step in steps {
                let len = s.actions.list[idx].steps.len();
                if at > len {
                    at = len;
                }
                insert_step(&mut s.actions.list[idx], at, step, false);
                at = at.saturating_add(1);
            }
            s.actions.recording_after = at.checked_sub(1);
        } else {
            s.actions.list[idx].steps.extend(steps);
        }
        s.actions.touch();
    }
    Ok(())
}

fn stop(s: &mut Session, _p: &Value) -> Result<Value> {
    not_playing(s, "actions.stop")?;
    let Some((_, idx)) = s.actions.recording else {
        return Err(bad("actions.stop", "not recording"));
    };
    flush_recording(s)?;
    s.actions.recording = None;
    s.actions.recording_after = None;
    let action = &s.actions.list[idx];
    Ok(json!({"action": action.name, "steps": action.steps.len()}))
}

fn run_recorded(s: &mut Session, action: &Action, from: usize) -> (u64, Option<Value>) {
    let mut ran = 0u64;
    let mut failed = None;
    for (i, (id, params)) in action.steps.iter().enumerate().skip(from) {
        if !action.step_on(i) {
            continue;
        }
        if let Some(auth) = s.authorize
            && let Err(e) = auth(id, params)
        {
            failed = Some(json!({"step": i, "id": id, "error": e.to_string()}));
            break;
        }
        match s.execute(id, params.clone()) {
            Ok(value) => {
                if let Some(error) = nested_failure(id, &value) {
                    failed = Some(json!({"step": i, "id": id, "error": error}));
                    break;
                }
                ran += 1;
            }
            Err(e) => {
                failed = Some(json!({"step": i, "id": id, "error": e.to_string()}));
                break;
            }
        }
    }
    (ran, failed)
}

/// Suspend recording while a called action executes. Keep its commands in the journal
/// for diagnostics, but record just the named call in the parent action.
pub fn begin_playback(s: &mut Session, action: &Action) -> Result<Option<(usize, usize)>> {
    flush_recording(s)?;
    let recording = s.actions.recording.take();
    s.actions.playing = s.actions.playing.saturating_add(1);
    s.actions.playback_stack.push(action.name.clone());
    Ok(recording)
}

pub fn finish_playback(s: &mut Session, recording: Option<(usize, usize)>, action: &Action, from: usize) {
    s.actions.playback_stack.pop();
    s.actions.playing = s.actions.playing.saturating_sub(1);
    if let Some((_, idx)) = recording {
        s.actions.recording = Some((s.journal.len(), idx));
        let mut params = json!({"action": action.name});
        if from > 0 {
            params["from"] = json!(from);
        }
        s.journal.push(("actions.play".into(), params));
    }
}

/// Nested playback returns a structured failure even though the command itself succeeded.
/// Promote it to the caller's failed step so no later parent steps run after a child fails.
pub fn nested_failure(id: &str, value: &Value) -> Option<String> {
    (id == "actions.play").then(|| value.get("failed")).flatten().map(|failure| {
        format!(
            "action `{}` step {} failed: {}",
            value.get("action").and_then(Value::as_str).unwrap_or(""),
            failure.get("step").and_then(Value::as_u64).unwrap_or(0).saturating_add(1),
            failure.get("error").and_then(Value::as_str).unwrap_or("unknown error")
        )
    })
}

fn play(s: &mut Session, p: &Value) -> Result<Value> {
    let (action, from) = playback_plan(s, p)?;
    // A set plays only the actions whose checkbox is on. Play on the action itself ignores that
    // checkbox and still skips unchecked steps.
    let honor = p.get("honorEnabled").and_then(Value::as_bool).unwrap_or(false);
    if honor && !action.enabled {
        return Ok(json!({"action": action.name, "ran": 0, "skipped": true}));
    }
    let recording = begin_playback(s, &action)?;
    let (ran, failed) = run_recorded(s, &action, from);
    finish_playback(s, recording, &action, from);
    Ok(match failed {
        Some(f) => json!({"action": action.name, "ran": ran, "failed": f}),
        None => json!({"action": action.name, "ran": ran}),
    })
}

fn not_playing(s: &Session, cmd: &str) -> Result<()> {
    if s.actions.playing > 0 { Err(bad(cmd, "cannot edit actions during playback")) } else { Ok(()) }
}

/// Move to a final zero-based index, either in an action's steps or in the action list.
fn move_item(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.move")?;
    let idx = resolve(&s.actions.list, p, "actions.move")?;
    let index = |key: &str| -> Result<usize> {
        p.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| bad("actions.move", format!("{key} must be a non-negative integer")))
    };
    let to = index("to")?;
    if p.get("step").is_some() {
        let step = index("step")?;
        let len = s.actions.list[idx].steps.len().saturating_add(pending_steps(s, idx).count());
        if step >= len || to >= len {
            return Err(bad("actions.move", "step or destination is out of range"));
        }
        if s.actions.recording.is_some_and(|(_, i)| i == idx) {
            flush_recording(s)?;
        }
        move_step_at(&mut s.actions.list[idx], step, to);
        let result = json!({"action": s.actions.list[idx].name, "step": to});
        s.actions.touch();
        Ok(result)
    } else {
        if to >= s.actions.list.len() {
            return Err(bad("actions.move", "destination is out of range"));
        }
        s.actions.align_folders();
        let moved = s.actions.list.remove(idx);
        let folder = s.actions.folders.remove(idx);
        let result = json!({"action": moved.name, "index": to});
        s.actions.list.insert(to, moved);
        s.actions.folders.insert(to, folder);
        if let Some((_, recording)) = s.actions.recording.as_mut() {
            *recording = moved_index(*recording, idx, to);
        }
        s.actions.touch();
        Ok(result)
    }
}

/// Follow a selected/recording row when a list item is moved.
pub fn moved_index(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.delete")?;
    let idx = resolve(&s.actions.list, p, "actions.delete")?;
    if let Some(step) = p.get("step") {
        let step = step.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or_else(|| bad("actions.delete", "step must be a non-negative integer"))?;
        let len = s.actions.list[idx].steps.len().saturating_add(pending_steps(s, idx).count());
        if step >= len {
            return Err(bad("actions.delete", "step is out of range"));
        }
        // Commit pending entries before editing the action; never erase the command
        // journal or undo the edit that produced the deleted recording step.
        if s.actions.recording.is_some_and(|(_, i)| i == idx) {
            flush_recording(s)?;
        }
        remove_step_at(&mut s.actions.list[idx], step);
        let action = &s.actions.list[idx];
        let result = json!({"action": action.name, "deletedStep": step, "steps": action.steps.len()});
        s.actions.touch();
        return Ok(result);
    }
    if s.actions.recording.is_some() {
        return Err(bad("actions.delete", "stop recording before deleting an action"));
    }
    s.actions.align_folders();
    let name = s.actions.list.remove(idx).name;
    s.actions.folders.remove(idx);
    s.actions.touch();
    Ok(json!({"deleted": name}))
}

fn folder_name(p: &Value, cmd: &str) -> Result<String> {
    match p.get("set") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(n)) if n.chars().count() > MAX_SET_CHARS => Err(bad(cmd, format!("folder name must be at most {MAX_SET_CHARS} characters"))),
        Some(Value::String(n)) => Ok(n.clone()),
        Some(_) => Err(bad(cmd, "\"set\" must be a string")),
    }
}

fn new_set(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.newSet")?;
    let name = match p.get("name") {
        None | Some(Value::Null) => {
            let mut n = s.actions.sets.len().saturating_add(1);
            while s.actions.sets.iter().any(|a| a == &format!("Set {n}")) {
                n = n.checked_add(1).ok_or_else(|| bad("actions.newSet", "cannot allocate a unique folder name"))?;
            }
            format!("Set {n}")
        }
        Some(Value::String(n)) if n.is_empty() => return Err(bad("actions.newSet", "folder name is empty")),
        Some(Value::String(n)) => n.clone(),
        Some(_) => return Err(bad("actions.newSet", "\"name\" must be a string")),
    };
    if name.chars().count() > MAX_SET_CHARS {
        return Err(bad("actions.newSet", format!("folder name must be at most {MAX_SET_CHARS} characters")));
    }
    if s.actions.sets.iter().any(|a| a == &name) {
        return Err(bad("actions.newSet", "a folder with this name already exists"));
    }
    if s.actions.sets.len() >= MAX_SETS {
        return Err(bad("actions.newSet", format!("at most {MAX_SETS} folders")));
    }
    ensure_set(&mut s.actions, &name)?;
    Ok(json!({"set": name}))
}

fn delete_set(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.deleteSet")?;
    let name = match p.get("name").or_else(|| p.get("set")) {
        Some(Value::String(n)) if !n.is_empty() => n.clone(),
        Some(Value::String(_)) => return Err(bad("actions.deleteSet", "folder name is empty")),
        _ => return Err(bad("actions.deleteSet", "needs \"name\"")),
    };
    s.actions.align_folders();
    if !s.actions.sets.iter().any(|n| n == &name) && !s.actions.folders.iter().any(|n| n == &name) {
        return Err(bad("actions.deleteSet", format!("no folder named `{name}`")));
    }
    s.actions.sets.retain(|n| n != &name);
    for folder in &mut s.actions.folders {
        if folder == &name {
            folder.clear();
        }
    }
    s.actions.touch();
    Ok(json!({"deleted": name}))
}

fn set_folder(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.setFolder")?;
    let idx = resolve(&s.actions.list, p, "actions.setFolder")?;
    if p.get("set").is_none() {
        return Err(bad("actions.setFolder", "needs \"set\" (a folder name, or \"\" for ungrouped)"));
    }
    let set = folder_name(p, "actions.setFolder")?;
    if !set.is_empty() && !s.actions.sets.iter().any(|n| n == &set) && s.actions.sets.len() >= MAX_SETS {
        return Err(bad("actions.setFolder", format!("at most {MAX_SETS} folders")));
    }
    if !set.is_empty() {
        ensure_set(&mut s.actions, &set)?;
    }
    s.actions.align_folders();
    s.actions.folders[idx] = set.clone();
    let name = s.actions.list[idx].name.clone();
    s.actions.touch();
    Ok(json!({"action": name, "set": set}))
}

fn set_step(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.setStep")?;
    let idx = resolve(&s.actions.list, p, "actions.setStep")?;
    let Some(step) = p.get("step").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()) else {
        return Err(bad("actions.setStep", "step must be a non-negative integer"));
    };
    let Some(params) = p.get("params").cloned() else {
        return Err(bad("actions.setStep", "needs \"params\""));
    };
    if !params.is_object() {
        return Err(bad("actions.setStep", "params must be an object"));
    }
    let len = s.actions.list[idx].steps.len().saturating_add(pending_steps(s, idx).count());
    if step >= len {
        return Err(bad("actions.setStep", "step is out of range"));
    }
    if s.actions.recording.is_some_and(|(_, i)| i == idx) {
        flush_recording(s)?;
    }
    let action = &mut s.actions.list[idx];
    let Some(slot) = action.steps.get_mut(step) else {
        return Err(bad("actions.setStep", "step is out of range"));
    };
    slot.1 = params;
    let name = action.name.clone();
    s.actions.touch();
    crate::dtf_cmds::note_edited_step(s, idx);
    Ok(json!({"action": name, "step": step}))
}

fn on_flag(p: &Value, cmd: &str) -> Result<bool> {
    match p.get("enabled").or_else(|| p.get("dialog")).or_else(|| p.get("on")) {
        Some(Value::Bool(v)) => Ok(*v),
        _ => Err(bad(cmd, "needs \"enabled\" or \"dialog\" (true or false)")),
    }
}

fn refresh_included(action: &mut Action) {
    align_step_flags(action);
    action.enabled = action.steps.is_empty() || action.step_enabled.iter().any(|on| *on);
}

fn set_enabled(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.setEnabled")?;
    let on = on_flag(p, "actions.setEnabled")?;
    if let Some(set) = p.get("set").and_then(Value::as_str) {
        s.actions.align_folders();
        let idxs: Vec<usize> = (0..s.actions.list.len()).filter(|i| s.actions.folder(*i) == set).collect();
        for i in &idxs {
            let action = &mut s.actions.list[*i];
            align_step_flags(action);
            for flag in &mut action.step_enabled {
                *flag = on;
            }
            action.enabled = on || action.steps.is_empty();
        }
        s.actions.touch();
        return Ok(json!({"set": set, "enabled": on, "actions": idxs.len()}));
    }
    let idx = resolve(&s.actions.list, p, "actions.setEnabled")?;
    if let Some(step) = p.get("step") {
        let step = step.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad("actions.setEnabled", "step must be a non-negative integer"))?;
        let action = &mut s.actions.list[idx];
        if step >= action.steps.len() {
            return Err(bad("actions.setEnabled", "step is out of range"));
        }
        align_step_flags(action);
        if let Some(flag) = action.step_enabled.get_mut(step) {
            *flag = on;
        }
        refresh_included(action);
        let name = action.name.clone();
        s.actions.touch();
        return Ok(json!({"action": name, "step": step, "enabled": on}));
    }
    let action = &mut s.actions.list[idx];
    align_step_flags(action);
    for flag in &mut action.step_enabled {
        *flag = on;
    }
    let enabled = on || action.steps.is_empty();
    action.enabled = enabled;
    let name = action.name.clone();
    s.actions.touch();
    Ok(json!({"action": name, "enabled": enabled}))
}

fn set_dialog(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.setDialog")?;
    let on = match p.get("dialog").or_else(|| p.get("enabled")) {
        Some(Value::Bool(v)) => *v,
        _ => return Err(bad("actions.setDialog", "needs \"dialog\" (true or false)")),
    };
    let idx = resolve(&s.actions.list, p, "actions.setDialog")?;
    let action = &mut s.actions.list[idx];
    align_step_flags(action);
    if let Some(step) = p.get("step") {
        let step = step.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad("actions.setDialog", "step must be a non-negative integer"))?;
        let Some(flag) = action.step_dialog.get_mut(step) else {
            return Err(bad("actions.setDialog", "step is out of range"));
        };
        *flag = on;
        let name = action.name.clone();
        s.actions.touch();
        return Ok(json!({"action": name, "step": step, "dialog": on}));
    }
    for flag in &mut action.step_dialog {
        *flag = on;
    }
    let name = action.name.clone();
    s.actions.touch();
    Ok(json!({"action": name, "dialog": on}))
}

fn set_color(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.setColor")?;
    let idx = resolve(&s.actions.list, p, "actions.setColor")?;
    let Some(color) = p.get("color").and_then(Value::as_str) else {
        return Err(bad("actions.setColor", "needs \"color\""));
    };
    if !BUTTON_COLORS.contains(&color) {
        return Err(bad("actions.setColor", "color must be none, red, orange, yellow, green, blue, violet or gray"));
    }
    s.actions.list[idx].color = color.to_string();
    let name = s.actions.list[idx].name.clone();
    s.actions.touch();
    Ok(json!({"action": name, "color": color}))
}

fn set_name(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.rename")?;
    let idx = resolve(&s.actions.list, p, "actions.rename")?;
    let Some(name) = p.get("name").and_then(Value::as_str) else {
        return Err(bad("actions.rename", "needs \"name\""));
    };
    if name.is_empty() {
        return Err(bad("actions.rename", "name is empty"));
    }
    if name.chars().count() > 256 {
        return Err(bad("actions.rename", "name must be at most 256 characters"));
    }
    if s.actions.list.iter().enumerate().any(|(i, action)| i != idx && action.name == name) {
        return Err(bad("actions.rename", "an action with this name already exists"));
    }
    s.actions.list[idx].name = name.to_string();
    s.actions.touch();
    Ok(json!({"action": name}))
}

fn duplicate(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.duplicate")?;
    if s.actions.recording.is_some() {
        return Err(bad("actions.duplicate", "stop recording first"));
    }
    let idx = resolve(&s.actions.list, p, "actions.duplicate")?;
    if let Some(step) = p.get("step") {
        let step = step.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad("actions.duplicate", "step must be a non-negative integer"))?;
        let action = &s.actions.list[idx];
        if step >= action.steps.len() {
            return Err(bad("actions.duplicate", "step is out of range"));
        }
        let copied = action.steps[step].clone();
        let dialog = action.step_dialog_on(step);
        let enabled = action.step_on(step);
        let at = step.saturating_add(1);
        insert_step(&mut s.actions.list[idx], at, copied, dialog);
        if let Some(flag) = s.actions.list[idx].step_enabled.get_mut(at) {
            *flag = enabled;
        }
        let name = s.actions.list[idx].name.clone();
        s.actions.touch();
        return Ok(json!({"action": name, "step": at}));
    }
    let copy = s.actions.list[idx].clone();
    let name = unique_copy_name(&s.actions.list, &copy.name)?;
    let folder = s.actions.folder(idx).to_string();
    let mut copy = copy;
    copy.name = name.clone();
    let index = push_action(s, copy, &folder)?;
    Ok(json!({"action": name, "index": index}))
}

fn unique_copy_name(list: &[Action], name: &str) -> Result<String> {
    let mut n = 1u32;
    loop {
        let candidate = if n == 1 { format!("{name} copy") } else { format!("{name} copy {n}") };
        if !list.iter().any(|action| action.name == candidate) {
            return Ok(candidate);
        }
        n = n.checked_add(1).ok_or_else(|| bad("actions.duplicate", "could not allocate a unique action name"))?;
        if n > 100_000 {
            return Err(bad("actions.duplicate", "could not allocate a unique action name"));
        }
    }
}

fn placed_index(action: &Action, p: &Value, cmd: &str) -> Result<usize> {
    match p.get("after") {
        None | Some(Value::Null) => Ok(action.steps.len()),
        Some(v) => {
            let Some(i) = v.as_u64().and_then(|n| usize::try_from(n).ok()) else {
                return Err(bad(cmd, "\"after\" must be a step index"));
            };
            if i >= action.steps.len() {
                return Err(bad(cmd, "\"after\" is out of range"));
            }
            Ok(i.saturating_add(1))
        }
    }
}

fn insert_stop(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.insertStop")?;
    let idx = resolve(&s.actions.list, p, "actions.insertStop")?;
    let message = match p.get("message") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) if text.chars().count() <= 2_000 => text.clone(),
        Some(Value::String(_)) => return Err(bad("actions.insertStop", "message must be at most 2000 characters")),
        Some(_) => return Err(bad("actions.insertStop", "\"message\" must be a string")),
    };
    let allow = p.get("allowContinue").and_then(Value::as_bool).unwrap_or(true);
    let at = placed_index(&s.actions.list[idx], p, "actions.insertStop")?;
    insert_step(&mut s.actions.list[idx], at, ("actions.message".into(), json!({"message": message, "allowContinue": allow})), true);
    let name = s.actions.list[idx].name.clone();
    s.actions.touch();
    Ok(json!({"action": name, "step": at}))
}

fn insert_menu_item(s: &mut Session, p: &Value) -> Result<Value> {
    not_playing(s, "actions.insertMenuItem")?;
    let idx = resolve(&s.actions.list, p, "actions.insertMenuItem")?;
    let Some(command) = p.get("command").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
        return Err(bad("actions.insertMenuItem", "needs \"command\""));
    };
    if !replayable(command) {
        return Err(bad("actions.insertMenuItem", format!("`{command}` cannot be inserted into an action")));
    }
    let params = match p.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => return Err(bad("actions.insertMenuItem", "\"params\" must be an object")),
    };
    let at = placed_index(&s.actions.list[idx], p, "actions.insertMenuItem")?;
    insert_step(&mut s.actions.list[idx], at, (command.to_string(), params), false);
    let name = s.actions.list[idx].name.clone();
    s.actions.touch();
    Ok(json!({"action": name, "step": at, "command": command}))
}

fn message(s: &mut Session, p: &Value) -> Result<Value> {
    let _ = s;
    let allow = p.get("allowContinue").and_then(Value::as_bool).unwrap_or(true);
    let text = p.get("message").and_then(Value::as_str).unwrap_or("");
    if text.chars().count() > 2_000 {
        return Err(bad("actions.message", "message must be at most 2000 characters"));
    }
    if !allow {
        let msg = if text.is_empty() { "stopped".to_string() } else { text.to_string() };
        return Err(bad("actions.message", msg));
    }
    Ok(json!({"message": text, "continued": true}))
}

fn clear_actions(s: &mut Session, _p: &Value) -> Result<Value> {
    not_playing(s, "actions.clear")?;
    if s.actions.recording.is_some() {
        return Err(bad("actions.clear", "stop recording first"));
    }
    let n = s.actions.list.len();
    s.actions.list.clear();
    s.actions.sets.clear();
    s.actions.folders.clear();
    s.actions.recording_after = None;
    s.actions.touch();
    Ok(json!({"cleared": n}))
}

fn reset_actions(s: &mut Session, p: &Value) -> Result<Value> {
    clear_actions(s, p)?;
    crate::dtf_cmds::ensure_preset(s);
    Ok(json!({"actions": s.actions.list.len()}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "actions.move",
            label: "Move Action or Step",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "step":index?, "to":index}. Move a step within its action, or the action itself. to is the final zero-based index. Pending recorded steps can be moved."##,
            enabled: always,
            run: move_item,
            journal: false,
        },
        CommandSpec {
            id: "actions.list",
            label: "List Actions",
            menu: &[],
            shortcut: None,
            params: r##"{} → {actions:[{name, set, steps:count}], sets:[folder…], recording:name|null}. set is "" when the action is ungrouped."##,
            enabled: always,
            run: list,
            journal: false,
        },
        CommandSpec {
            id: "actions.get",
            label: "Get Action",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index} → {name, steps:[[id, params]…]} (the shape file.automate.batch and droplets take)"##,
            enabled: always,
            run: get,
            journal: false,
        },
        CommandSpec {
            id: "actions.record",
            label: "Record Action",
            menu: &[],
            shortcut: None,
            params: r##"{"name":str?, "set":str?} (new action, default name "Action N", set "" is ungrouped) or {"action":name|index} (append) → {action, index}"##,
            enabled: always,
            run: record,
            journal: false,
        },
        CommandSpec {
            id: "actions.stop",
            label: "Stop Recording",
            menu: &[],
            shortcut: None,
            params: "{} → {action, steps:count} (steps recorded since actions.record, queries and action-editing commands omitted)",
            enabled: always,
            run: stop,
            journal: false,
        },
        CommandSpec {
            id: "actions.play",
            label: "Play Action",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "from":step?} → {action, ran, failed?:{step, id, error}}. step and from are 0-based. Stops at the first error (the command still returns ok, with failed set) and leaves one history step per step that ran. Nested calls are supported up to 16 levels; cycles are rejected. Recording stores a named call rather than its expanded commands. Each step is checked with Session::authorize when one is installed."##,
            enabled: always,
            run: play,
            journal: false,
        },
        CommandSpec {
            id: "actions.delete",
            label: "Delete Action",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "step":index?} → {deleted:name} or {action, deletedStep, steps:count}. step is 0-based, including pending recorded steps. Whole-action deletion is refused while recording."##,
            enabled: always,
            run: delete,
            journal: false,
        },
        CommandSpec {
            id: "actions.newSet",
            label: "New Action Set",
            menu: &[],
            shortcut: None,
            params: r##"{"name":str?} → {set}. Creates an empty folder. The default name is "Set N". Refuses an empty name, a duplicate, or more than 64 folders."##,
            enabled: always,
            run: new_set,
            journal: false,
        },
        CommandSpec {
            id: "actions.deleteSet",
            label: "Delete Action Set",
            menu: &[],
            shortcut: None,
            params: r##"{"name":str} → {deleted}. Removes the folder. Its actions become ungrouped; they are not deleted."##,
            enabled: always,
            run: delete_set,
            journal: false,
        },
        CommandSpec {
            id: "actions.setFolder",
            label: "Move Action to Set",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "set":str} → {action, set}. set "" leaves the action ungrouped. A new non-empty name creates the folder."##,
            enabled: always,
            run: set_folder,
            journal: false,
        },
        CommandSpec {
            id: "actions.setStep",
            label: "Set Action Step",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "step":index, "params":object} → {action, step}. Replaces that step's parameters. Numbers, strings and booleans are what the Actions panel edits."##,
            enabled: always,
            run: set_step,
            journal: false,
        },
        CommandSpec {
            id: "actions.setEnabled",
            label: "Enable Action or Step",
            menu: &[],
            shortcut: None,
            params: r##"{"enabled":bool, "set":str?} or {"action":name|index, "step":index?, "enabled":bool}. A set or action checkbox turns every nested step on or off. A step checkbox leaves the others alone. Unchecked steps are skipped on playback."##,
            enabled: always,
            run: set_enabled,
            journal: false,
        },
        CommandSpec {
            id: "actions.setDialog",
            label: "Toggle Action Dialog",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "step":index?, "dialog":bool}. When dialog is true, desktop playback opens the step's dialog. Headless playback still uses the recorded values."##,
            enabled: always,
            run: set_dialog,
            journal: false,
        },
        CommandSpec {
            id: "actions.setColor",
            label: "Set Action Button Color",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "color":""|"red"|"orange"|"yellow"|"green"|"blue"|"violet"|"gray"} → {action, color}. Empty is None."##,
            enabled: always,
            run: set_color,
            journal: false,
        },
        CommandSpec {
            id: "actions.rename",
            label: "Rename Action",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "name":str} → {action}. Refuses an empty name, a name over 256 characters, or a duplicate."##,
            enabled: always,
            run: set_name,
            journal: false,
        },
        CommandSpec {
            id: "actions.duplicate",
            label: "Duplicate Action or Step",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "step":index?} → {action, index|step}. A step is copied just after itself. An action is copied in the same set as "Name copy"."##,
            enabled: always,
            run: duplicate,
            journal: false,
        },
        CommandSpec {
            id: "actions.insertStop",
            label: "Insert Stop",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "message":str?="", "allowContinue":bool=true, "after":index?} → {action, step}. Playback shows the message. Without allowContinue, playback stops there."##,
            enabled: always,
            run: insert_stop,
            journal: false,
        },
        CommandSpec {
            id: "actions.insertMenuItem",
            label: "Insert Menu Item",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "command":id, "params":object?={}, "after":index?} → {action, step, command}. Inserts a command without running it. The command must be one recording would keep."##,
            enabled: always,
            run: insert_menu_item,
            journal: false,
        },
        CommandSpec {
            id: "actions.message",
            label: "Action Stop",
            menu: &[],
            shortcut: None,
            params: r##"{"message":str?="", "allowContinue":bool=true} → {message, continued}. Inserted by actions.insertStop. Refuses to continue when allowContinue is false."##,
            enabled: always,
            run: message,
            journal: false,
        },
        CommandSpec {
            id: "actions.clear",
            label: "Clear All Actions",
            menu: &[],
            shortcut: None,
            params: "{} → {cleared}. Removes every action and set. Refuses while recording or playing.",
            enabled: always,
            run: clear_actions,
            journal: false,
        },
        CommandSpec {
            id: "actions.reset",
            label: "Reset Actions",
            menu: &[],
            shortcut: None,
            params: "{} → {actions}. Clears the list and restores the built-in DTF action.",
            enabled: always,
            run: reset_actions,
            journal: false,
        },
        CommandSpec {
            id: "actions.importAtn",
            label: "Import Photoshop Actions",
            menu: &[],
            shortcut: None,
            params: r##"{"base64":str|"bytes":[0..255…], "replace":bool=false} → {set, skipped:[…], shortcuts:[{action, shortcut}], actions}. Loads a .atn set. Steps PhotoCraft cannot play are listed in skipped and omitted. replace clears the current list first."##,
            enabled: always,
            run: crate::atn_import::import_atn,
            journal: false,
        },
        CommandSpec {
            id: "actions.importJson",
            label: "Import Actions JSON",
            menu: &[],
            shortcut: None,
            params: r##"{"text":str, "replace":bool=false} → {actions}. Loads a PhotoCraft actions file (the JSON actions.exportJson writes)."##,
            enabled: always,
            run: crate::atn_import::import_json,
            journal: false,
        },
        CommandSpec {
            id: "actions.exportJson",
            label: "Export Actions JSON",
            menu: &[],
            shortcut: None,
            params: r##"{"set":str?} → {text}. Writes one set, or every action when set is omitted, as a PhotoCraft actions file."##,
            enabled: always,
            run: crate::atn_import::export_json,
            journal: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pixel(s: &Session, layer: usize, x: i32, y: i32) -> Vec<f32> {
        s.active().unwrap().doc.layers[layer].surface().unwrap().pixel(x, y)
    }

    fn record_red(s: &mut Session, depth: u32) {
        s.execute("file.new", json!({"width": 40, "height": 40, "depth": depth})).unwrap();
        s.execute("actions.record", json!({"name": "Red"})).unwrap();
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 10, "height": 10})).unwrap();
        s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
        let _ = s.execute("document.pixel", json!({"x": 1, "y": 1}));
        let _ = s.execute("actions.list", json!({}));
        s.execute("actions.stop", json!({})).unwrap();
    }

    fn translated_square(s: &mut Session, depth: u32) {
        s.execute("file.new", json!({"width": 20, "height": 8, "depth": depth, "background": "transparent"})).unwrap();
        s.edit("red square", |doc, _| {
            doc.layers[0].surface_mut().unwrap().fill_rect(photocraft_geom::Rect::new(0, 2, 2, 4), &[1.0, 0.0, 0.0, 1.0]);
            Ok(())
        })
        .unwrap();
        s.execute("edit.transform", json!({"matrix": [1, 0, 0, 1, 4, 0], "interpolation": "nearest"})).unwrap();
        assert_eq!(pixel(s, 0, 4, 2), vec![1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn record_transform_again_replays_one_concrete_transform_at_each_depth() {
        for depth in [8, 16, 32] {
            let mut s = Session::new();
            translated_square(&mut s, depth);
            let from = s.journal.len();
            s.execute("actions.record", json!({"name": "Again"})).unwrap();
            s.execute("edit.transform.again", json!({})).unwrap();
            let ids: Vec<&str> = s.journal.iter().skip(from).map(|(id, _)| id.as_str()).collect();
            assert_eq!(ids, ["edit.transform", "edit.transform.again"], "both commands stay in the journal during recording");
            let live_count = s.journal.iter().skip(from).filter(|(id, _)| replayable(id)).count();
            s.execute("actions.stop", json!({})).unwrap();
            let recorded = s.execute("actions.get", json!({"action": "Again"})).unwrap();
            let steps = recorded["steps"].as_array().unwrap();
            assert_eq!(steps.len(), 1, "depth {depth}: record the concrete transform once");
            assert_eq!(live_count, 1, "depth {depth}: the live Actions count agrees");
            assert_eq!(steps[0][0], "edit.transform");
            assert_eq!(steps[0][1]["matrix"], json!([1, 0, 0, 1, 4, 0]));
            assert_eq!(steps[0][1]["interpolation"], "nearest");

            translated_square(&mut s, depth);
            let before = s.active().unwrap().history.past_len();
            let played = s.execute("actions.play", json!({"action": "Again"})).unwrap();
            assert!(played.get("failed").is_none(), "depth {depth}: {played}");
            assert_eq!(played["ran"], 1, "depth {depth}: apply the recorded transform once");
            assert_eq!(s.active().unwrap().history.past_len(), before + 1, "depth {depth}: one undo step");
            assert_eq!(pixel(&s, 0, 8, 2), vec![1.0, 0.0, 0.0, 1.0], "depth {depth}: moved four pixels");
            assert_eq!(pixel(&s, 0, 12, 2), vec![0.0; 4], "depth {depth}: no second transform");
            assert!(s.undo());
            assert_eq!(pixel(&s, 0, 4, 2), vec![1.0, 0.0, 0.0, 1.0]);
            assert_eq!(pixel(&s, 0, 8, 2), vec![0.0; 4]);
            assert!(s.redo());
            assert_eq!(pixel(&s, 0, 8, 2), vec![1.0, 0.0, 0.0, 1.0]);
            assert_eq!(pixel(&s, 0, 4, 2), vec![0.0; 4]);
        }
    }

    #[test]
    fn record_explicit_transform_keeps_one_replayable_step() {
        let mut s = Session::new();
        translated_square(&mut s, 8);
        s.execute("actions.record", json!({"name": "Move"})).unwrap();
        s.execute("edit.transform", json!({"matrix": [1, 0, 0, 1, 4, 0], "interpolation": "nearest"})).unwrap();
        assert_eq!(s.execute("actions.stop", json!({})).unwrap()["steps"], 1);
        let recorded = s.execute("actions.get", json!({"action": "Move"})).unwrap();
        assert_eq!(recorded["steps"][0][0], "edit.transform");

        translated_square(&mut s, 8);
        let before = s.active().unwrap().history.past_len();
        let played = s.execute("actions.play", json!({"action": "Move"})).unwrap();
        assert!(played.get("failed").is_none(), "{played}");
        assert_eq!(played["ran"], 1);
        assert_eq!(s.active().unwrap().history.past_len(), before + 1);
        assert_eq!(pixel(&s, 0, 8, 2), vec![1.0, 0.0, 0.0, 1.0]);
        assert_eq!(pixel(&s, 0, 12, 2), vec![0.0; 4]);
    }

    #[test]
    fn record_keeps_only_replayable_steps() {
        let mut s = Session::new();
        record_red(&mut s, 8);
        let got = s.execute("actions.get", json!({"action": "Red"})).unwrap();
        let ids: Vec<&str> = got["steps"].as_array().unwrap().iter().map(|st| st[0].as_str().unwrap()).collect();
        assert_eq!(ids, ["layer.new.layer", "select.rect", "edit.fill"]);
        assert!(s.journal.iter().all(|(id, _)| !id.starts_with("actions.")));
        let listed = s.execute("actions.list", json!({})).unwrap();
        assert_eq!(listed["actions"][0]["steps"], 3);
        assert!(listed["recording"].is_null());
    }

    #[test]
    fn play_on_another_document_matches_pixels_at_each_depth() {
        for depth in [8, 16, 32] {
            let mut s = Session::new();
            record_red(&mut s, depth);
            s.execute("file.new", json!({"width": 40, "height": 40, "depth": depth})).unwrap();
            let before = s.active().unwrap().history.past_len();
            let r = s.execute("actions.play", json!({"action": 0})).unwrap();
            assert!(r.get("failed").is_none(), "depth {depth}: {r}");
            assert_eq!(r["ran"], 3, "depth {depth}");
            assert_eq!(s.active().unwrap().doc.layers.len(), 2, "depth {depth}");
            assert_eq!(s.active().unwrap().history.past_len(), before + 3, "depth {depth}: one history step per played step");
            let px = pixel(&s, 1, 5, 5);
            assert!(px.iter().zip([1.0, 0.0, 0.0, 1.0]).all(|(a, b)| (a - b).abs() < 1.0e-5), "depth {depth}: {px:?}");
            assert_eq!(s.actions.playing, 0);
        }
    }

    #[test]
    fn play_stops_at_the_failing_step_and_reports_it() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 16, "height": 16})).unwrap();
        s.execute("actions.record", json!({})).unwrap();
        s.execute("layer.new.layer", json!({"name": "One"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        s.actions.list[0].steps.push(("not.a.command".into(), json!({})));
        s.execute("file.new", json!({"width": 16, "height": 16})).unwrap();
        let r = s.execute("actions.play", json!({"action": "Action 1"})).unwrap();
        assert_eq!(r["ran"], 1);
        assert_eq!(r["failed"]["step"], 1);
        assert_eq!(r["failed"]["id"], "not.a.command");
        assert!(r["failed"]["error"].as_str().unwrap().contains("unknown command"));
        assert_eq!(s.active().unwrap().doc.layers.len(), 2, "the step before the failure is kept");
        assert_eq!(s.actions.playing, 0);
        // A later play still runs.
        assert_eq!(s.execute("actions.play", json!({"action": 0, "from": 1})).unwrap()["ran"], 0);
    }

    #[test]
    fn play_refuses_recursion() {
        let mut s = Session::new();
        s.actions.list.push(Action::new("Loop".into(), vec![("actions.play".into(), json!({"action": "Loop"}))]));
        let r = s.execute("actions.play", json!({"action": "Loop"})).unwrap();
        assert_eq!(r["ran"], 0);
        assert_eq!(r["failed"]["id"], "actions.play");
        assert!(r["failed"]["error"].as_str().unwrap().contains("recursive action"), "{r}");
        assert_eq!(s.actions.playing, 0);
    }

    #[test]
    fn play_checks_each_step_with_the_authorize_hook() {
        fn deny_file(id: &str, _: &Value) -> Result<()> {
            if id.starts_with("file.") { Err(EngineError::Other(format!("automation command `{id}` is disabled"))) } else { Ok(()) }
        }
        let mut s = Session::new();
        s.authorize = Some(deny_file);
        s.actions.list.push(Action::new("Open".into(), vec![("file.open".into(), json!({"path": "/etc/passwd"})), ("layer.new.layer".into(), json!({}))]));
        let r = s.execute("actions.play", json!({"action": "Open"})).unwrap();
        assert_eq!(r["ran"], 0);
        assert_eq!(r["failed"]["step"], 0);
        assert_eq!(r["failed"]["id"], "file.open");
        assert!(r["failed"]["error"].as_str().unwrap().contains("disabled"), "{r}");
        assert!(s.documents().is_empty(), "the refused step does not open a document");
    }

    #[test]
    fn append_delete_and_from_step() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8, "background": "white"})).unwrap();
        let started = s.execute("actions.record", json!({"name": "Grow"})).unwrap();
        assert_eq!(started["index"], 0);
        s.execute("layer.new.layer", json!({"name": "One"})).unwrap();
        assert!(s.execute("actions.record", json!({})).is_err(), "already recording");
        assert!(s.execute("actions.delete", json!({"action": 0})).is_err(), "delete while recording");
        let stopped = s.execute("actions.stop", json!({})).unwrap();
        assert_eq!(stopped["steps"], 1);
        s.execute("actions.record", json!({"action": "Grow"})).unwrap();
        s.execute("layer.new.layer", json!({"name": "Two"})).unwrap();
        assert_eq!(s.execute("actions.stop", json!({})).unwrap()["steps"], 2);
        assert!(s.execute("actions.stop", json!({})).is_err(), "not recording");
        let before = s.active().unwrap().doc.layers.len();
        let r = s.execute("actions.play", json!({"action": "Grow", "from": 1})).unwrap();
        assert_eq!(r["ran"], 1);
        assert_eq!(s.active().unwrap().doc.layers.len(), before + 1);
        assert_eq!(s.execute("actions.delete", json!({"action": "Grow"})).unwrap()["deleted"], "Grow");
        assert!(s.actions.list.is_empty());
        assert!(s.execute("actions.get", json!({"action": "Grow"})).is_err());
    }

    #[test]
    fn delete_one_step_while_recording_keeps_journal_and_remaining_steps() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        s.execute("actions.record", json!({"name": "Edit"})).unwrap();
        s.execute("layer.new.layer", json!({"name": "Kept"})).unwrap();
        s.execute("layer.new.layer", json!({"name": "Removed"})).unwrap();
        let journal = s.journal.clone();
        let rev = s.actions.rev;
        let before = s.actions.clone();
        for step in [json!(-1), json!(2), json!(0.5), json!(null), json!("0"), json!(u64::MAX)] {
            assert!(s.execute("actions.delete", json!({"action": 0, "step": step})).is_err());
            assert_eq!(s.actions, before, "invalid deletion is atomic");
        }
        assert_eq!(pending_steps(&s, 0).count(), 2);
        let result = s.execute("actions.delete", json!({"action": 0, "step": 1})).unwrap();
        assert_eq!(result["steps"], 1);
        assert_eq!(s.journal, journal, "deleting a recording does not erase executed commands");
        assert_eq!(s.active().unwrap().doc.layers.len(), 3, "it does not undo document edits");
        assert!(s.actions.recording.is_some());
        assert!(s.actions.rev > rev);
        s.execute("layer.new.layer", json!({"name": "After"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        assert_eq!(s.actions.list[0].steps.iter().map(|(_, p)| p["name"].as_str().unwrap()).collect::<Vec<_>>(), ["Kept", "After"]);
        s.execute("actions.delete", json!({"action": "Edit", "step": 0})).unwrap();
        assert_eq!(s.actions.list[0].steps[0].1["name"], "After");
        let saved = serde_json::to_string(&s.actions.list).unwrap();
        s.actions.list = serde_json::from_str(&saved).unwrap();
        s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        assert_eq!(s.execute("actions.play", json!({"action": 0})).unwrap()["ran"], 1);
        assert_eq!(s.active().unwrap().doc.layers[1].name, "After");
        s.execute("actions.delete", json!({"action": 0, "step": 0})).unwrap();
        assert_eq!(s.actions.list.len(), 1, "an empty action still exists");
        assert!(s.actions.list[0].steps.is_empty());
    }

    #[test]
    fn nested_call_records_one_named_step_and_tracks_child_edits() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 4, "height": 4})).unwrap();
        s.actions.list.push(Action::new("Child".into(), vec![("layer.new.layer".into(), json!({"name": "Child layer"}))]));
        s.execute("actions.record", json!({"name": "Parent"})).unwrap();
        s.execute("layer.new.layer", json!({"name": "Before"})).unwrap();
        s.execute("actions.play", json!({"action": 0})).unwrap();
        assert_eq!(pending_steps(&s, 1).cloned().collect::<Vec<_>>(), [("actions.play".into(), json!({"action": "Child"}))]);
        s.execute("layer.new.layer", json!({"name": "After"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        assert_eq!(s.actions.list[1].steps.iter().map(|st| st.0.as_str()).collect::<Vec<_>>(), ["layer.new.layer", "actions.play", "layer.new.layer"]);
        s.actions.list[0].steps.push(("layer.new.layer".into(), json!({"name": "Added later"})));
        s.execute("actions.move", json!({"action": "Child", "to": 1})).unwrap();
        let saved = serde_json::to_string(&s.actions.list).unwrap();
        s.actions.list = serde_json::from_str(&saved).unwrap();
        s.execute("file.new", json!({"width": 4, "height": 4})).unwrap();
        let result = s.execute("actions.play", json!({"action": "Parent"})).unwrap();
        assert!(result.get("failed").is_none(), "{result}");
        assert_eq!(
            s.active().unwrap().doc.layers.iter().skip(1).map(|l| l.name.as_str()).collect::<Vec<_>>(),
            ["Before", "Child layer", "Added later", "After"]
        );
        assert!(s.actions.playback_stack.is_empty());
    }

    #[test]
    fn nested_failure_cycles_depth_and_authorization_stop_parent() {
        let mut s = Session::new();
        s.actions
            .list
            .push(Action::new("A".into(), vec![("actions.play".into(), json!({"action": "B"})), ("file.new".into(), json!({"width": 2,"height": 2}))]));
        s.actions.list.push(Action::new("B".into(), vec![("actions.play".into(), json!({"action": "A"}))]));
        let r = s.execute("actions.play", json!({"action": "A"})).unwrap();
        assert!(r["failed"]["error"].as_str().unwrap().contains("recursive"));
        assert!(s.documents().is_empty());
        s.actions.list[1].steps = vec![("file.new".into(), json!({"width": 2,"height": 2}))];
        s.authorize = Some(|id, _| if id == "file.new" { Err(EngineError::Other("denied nested file".into())) } else { Ok(()) });
        let r = s.execute("actions.play", json!({"action": "A"})).unwrap();
        assert!(r["failed"]["error"].as_str().unwrap().contains("denied nested file"));
        assert!(s.documents().is_empty());
        s.authorize = None;
        s.actions.list.clear();
        for i in 0..17 {
            s.actions
                .list
                .push(Action::new(format!("N{i}"), if i < 16 { vec![("actions.play".into(), json!({"action":format!("N{}", i + 1)}))] } else { vec![] }));
        }
        let r = s.execute("actions.play", json!({"action": "N0"})).unwrap();
        assert!(r["failed"]["error"].as_str().unwrap().contains("16 levels"));
        assert_eq!(s.actions.playing, 0);
        assert!(s.actions.playback_stack.is_empty());
        s.actions.list[15].steps.clear();
        assert!(s.execute("actions.play", json!({"action": "N0"})).unwrap().get("failed").is_none());
    }

    #[test]
    fn move_pending_steps_is_atomic_persists_and_changes_playback_order() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 4, "height": 4})).unwrap();
        s.execute("actions.record", json!({"name": "Order"})).unwrap();
        for name in ["One", "Two", "Three"] {
            s.execute("layer.new.layer", json!({"name": name})).unwrap();
        }
        let before = s.actions.clone();
        for params in [
            json!({"action":0,"step":0,"to":3}),
            json!({"action":0,"step":99,"to":0}),
            json!({"action":0,"step":0.5,"to":0}),
            json!({"action":0,"to":-1}),
            json!({"action":0,"to":null}),
        ] {
            assert!(s.execute("actions.move", params).is_err());
            assert_eq!(s.actions, before);
        }
        let journal = s.journal.clone();
        s.execute("actions.move", json!({"action":0,"step":0,"to":2})).unwrap();
        s.execute("actions.move", json!({"action":0,"step":1,"to":0})).unwrap();
        assert_eq!(s.journal, journal);
        s.execute("layer.new.layer", json!({"name":"Four"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        assert_eq!(s.actions.list[0].steps.iter().map(|st| st.1["name"].as_str().unwrap()).collect::<Vec<_>>(), ["Three", "Two", "One", "Four"]);
        s.execute("file.new", json!({"width":4,"height":4})).unwrap();
        s.execute("actions.play", json!({"action":0})).unwrap();
        assert_eq!(s.active().unwrap().doc.layers.iter().skip(1).map(|l| l.name.as_str()).collect::<Vec<_>>(), ["Three", "Two", "One", "Four"]);
    }

    #[test]
    fn new_names_do_not_collide_with_named_calls() {
        let mut s = Session::new();
        s.actions.list.push(Action::new("Action 2".into(), vec![]));
        assert_eq!(s.execute("actions.record", json!({})).unwrap()["action"], "Action 3");
        s.execute("actions.stop", json!({})).unwrap();
        assert!(s.execute("actions.record", json!({"name":"Action 2"})).is_err());
        assert!(s.actions.recording.is_none());
    }

    #[test]
    fn moving_action_keeps_recording_attached_to_it() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width":4,"height":4})).unwrap();
        for name in ["A", "B", "C"] {
            s.actions.list.push(Action::new(name.into(), vec![]));
        }
        s.execute("actions.record", json!({"action":"B"})).unwrap();
        s.execute("layer.new.layer", json!({"name":"before"})).unwrap();
        s.execute("actions.move", json!({"action":"B","to":2})).unwrap();
        s.execute("actions.move", json!({"action":"A","to":2})).unwrap();
        s.execute("layer.new.layer", json!({"name":"after"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        assert_eq!(s.actions.list[1].name, "B");
        assert_eq!(s.actions.list[1].steps.len(), 2);
    }

    #[test]
    fn bad_params_do_not_panic() {
        let mut s = Session::new();
        let junk = [
            Value::Null,
            json!([]),
            json!("x"),
            json!(1),
            json!({"action": true}),
            json!({"action": -1}),
            json!({"action": 99}),
            json!({"action": ""}),
            json!({"name": 1}),
            json!({"name": ""}),
            json!({"from": "start"}),
            json!({"from": -3}),
            json!({"action": "missing", "from": 4}),
        ];
        for id in [
            "actions.list",
            "actions.get",
            "actions.record",
            "actions.stop",
            "actions.play",
            "actions.delete",
            "actions.move",
            "actions.newSet",
            "actions.deleteSet",
            "actions.setFolder",
            "actions.setStep",
            "actions.setEnabled",
            "actions.setDialog",
            "actions.setColor",
            "actions.rename",
            "actions.duplicate",
            "actions.insertStop",
            "actions.insertMenuItem",
            "actions.message",
            "actions.clear",
            "actions.reset",
            "actions.importAtn",
            "actions.importJson",
            "actions.exportJson",
        ] {
            for p in &junk {
                let _ = s.execute(id, p.clone());
            }
        }
        assert_eq!(s.actions.playing, 0);
        assert!(crate::commands::command_specs().iter().any(|c| c.id == "actions.play" && !c.journal && c.menu.is_empty()));
    }

    #[test]
    fn disabled_steps_are_skipped_and_a_disabled_action_can_be_honored() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        s.actions.list.push(Action::new("Two".into(), vec![("layer.new.layer".into(), json!({})), ("layer.new.layer".into(), json!({}))]));
        s.execute("actions.setEnabled", json!({"action": 0, "step": 1, "enabled": false})).unwrap();
        let played = s.execute("actions.play", json!({"action": 0})).unwrap();
        assert_eq!(played["ran"], 1);
        assert_eq!(s.active().unwrap().doc.layers.len(), 2);
        s.execute("actions.setEnabled", json!({"action": 0, "enabled": false})).unwrap();
        let skipped = s.execute("actions.play", json!({"action": 0, "honorEnabled": true})).unwrap();
        assert_eq!(skipped["ran"], 0);
        assert_eq!(skipped["skipped"], true);
        assert_eq!(s.active().unwrap().doc.layers.len(), 2);
        assert!(s.execute("actions.setColor", json!({"action": 0, "color": "pink"})).is_err());
        assert!(s.execute("actions.rename", json!({"action": 0})).is_err());
        assert!(s.execute("actions.insertMenuItem", json!({"action": 0, "command": "actions.delete"})).is_err());
        assert!(s.execute("actions.importAtn", json!({"base64": "!!!!"})).is_err());
        assert!(s.execute("actions.message", json!({"allowContinue": false, "message": "stop here"})).is_err());
        let stopped = s.execute("actions.insertStop", json!({"action": 0, "message": "check the spot", "allowContinue": true})).unwrap();
        assert_eq!(stopped["step"], 2);
        assert!(s.actions.list[0].step_dialog_on(2));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn actions_persist_with_the_preset_store() {
        let dir = std::env::temp_dir().join(format!("pc-actions-{}-persist", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new();
        let warnings = s.attach_preset_store(crate::preset_store::open_dir(&dir));
        assert!(warnings.iter().all(|w| !w.contains("actions")), "{warnings:?}");
        s.execute("file.new", json!({"width": 4, "height": 4})).unwrap();
        s.execute("actions.record", json!({"name": "Kept"})).unwrap();
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        drop(s);

        let mut t = Session::new();
        t.attach_preset_store(crate::preset_store::open_dir(&dir));
        let listed = t.execute("actions.list", json!({})).unwrap();
        let kept = listed["actions"].as_array().and_then(|a| a.iter().find(|e| e["name"] == "Kept")).expect("Kept");
        assert_eq!(kept["steps"], 1);
        assert_eq!(t.execute("actions.get", json!({"action": "Kept"})).unwrap()["steps"][0][0], "layer.new.layer");
        assert!(listed["actions"].as_array().is_some_and(|a| a.iter().any(|e| e["name"] == "DTF")));

        // An action created before the store finishes loading is kept, and a different one on
        // disk is merged in.
        let mut early = Session::new();
        early.execute("actions.record", json!({"name": "Early"})).unwrap();
        early.execute("actions.stop", json!({})).unwrap();
        early.attach_preset_store(crate::preset_store::open_dir(&dir));
        let names: Vec<String> = early.actions.list.iter().map(|a| a.name.clone()).collect();
        assert!(names.contains(&"Early".into()) && names.contains(&"Kept".into()), "{names:?}");

        std::fs::write(dir.join("actions.json"), b"{").unwrap();
        let mut bad = Session::new();
        let warnings = bad.attach_preset_store(crate::preset_store::open_dir(&dir));
        assert!(warnings.iter().any(|w| w.contains("actions")), "{warnings:?}");
        assert_eq!(bad.actions.list.len(), 1);
        assert_eq!(bad.actions.list[0].name, "DTF");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_round_trip_and_version_1_stays_ungrouped() {
        let mut s = Session::new();
        s.execute("actions.newSet", json!({"name": "DTF"})).unwrap();
        s.execute("actions.record", json!({"name": "White", "set": "DTF"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        s.actions.list[0].steps.push(("image.dtf".into(), json!({"contract": 1, "name": "White"})));
        s.actions.touch();
        assert_eq!(s.execute("actions.setStep", json!({"action": "White", "step": 0, "params": {"contract": 3, "name": "Gloss"}})).unwrap()["step"], 0);
        assert_eq!(s.actions.list[0].steps[0].1["name"], "Gloss");
        assert_eq!(s.actions.folder(0), "DTF");
        s.execute("actions.setFolder", json!({"action": "White", "set": ""})).unwrap();
        assert!(s.actions.folder(0).is_empty());
        s.execute("actions.setFolder", json!({"action": 0, "set": "DTF"})).unwrap();
        s.execute("actions.newSet", json!({"name": "Empty"})).unwrap();
        s.execute("actions.deleteSet", json!({"name": "DTF"})).unwrap();
        assert!(s.actions.folder(0).is_empty(), "deleting a folder ungroups its actions");
        assert!(s.actions.sets.iter().any(|n| n == "Empty"));
        assert!(s.execute("actions.newSet", json!({"name": ""})).is_err());
        assert!(s.execute("actions.setStep", json!({"action": 0, "step": 5, "params": {}})).is_err());
        assert!(s.execute("actions.setStep", json!({"action": 0, "step": 0, "params": [1]})).is_err());
        assert!(s.execute("actions.deleteSet", json!({})).is_err());

        #[cfg(not(target_arch = "wasm32"))]
        {
            let dir = std::env::temp_dir().join(format!("pc-actions-{}-v1", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("actions.json"), r#"{"version":1,"actions":[{"name":"Old","steps":[["layer.new.layer",{}]]}]}"#).unwrap();
            let mut loaded = Session::new();
            loaded.attach_preset_store(crate::preset_store::open_dir(&dir));
            let old = loaded.actions.list.iter().position(|a| a.name == "Old").unwrap();
            assert!(loaded.actions.folder(old).is_empty(), "version 1 actions stay ungrouped");
            let dtf = loaded.actions.list.iter().position(|a| a.name == "DTF").unwrap();
            assert_eq!(loaded.actions.folder(dtf), "DTF");
            assert!(loaded.actions.sets.iter().any(|n| n == "DTF"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
