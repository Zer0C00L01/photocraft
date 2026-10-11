//! Load a Photoshop `.atn` action set into the session.
//!
//! Only events PhotoCraft can play are kept. The DTF recording (set the selection to the
//! transparency channel, contract it, make a spot channel) is one of those. Everything else is
//! reported and left out of the action.

use serde_json::{Value, json};

use photocraft_psd::atn::{ActionCommand, ActionFile};
use photocraft_psd::descriptor::{Descriptor, ReferenceItem, Value as DescValue};

use crate::actions_cmds::{self, Action, color_from_index};
use crate::{EngineError, Result, Session};

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: "actions.importAtn".into(), msg: msg.into() }
}

fn id_text(id: &photocraft_psd::descriptor::Id) -> String {
    String::from_utf8_lossy(id.as_bytes()).into_owned()
}

fn key_is(id: &photocraft_psd::descriptor::Id, names: &[&str]) -> bool {
    let text = id_text(id);
    let trimmed = text.trim();
    names.iter().any(|name| text == *name || trimmed.eq_ignore_ascii_case(name))
}

fn event_is(event: &str, names: &[&str]) -> bool {
    let trimmed = event.trim();
    names.iter().any(|name| event == *name || trimmed.eq_ignore_ascii_case(name))
}

fn find<'a>(desc: &'a Descriptor, keys: &[&str]) -> Option<&'a DescValue> {
    desc.items.iter().find(|(key, _)| key_is(key, keys)).map(|(_, value)| value)
}

fn number(value: &DescValue) -> Option<f64> {
    match value {
        DescValue::UnitFloat { value, .. } => value.is_finite().then_some(*value),
        DescValue::Double(value) => value.is_finite().then_some(*value),
        DescValue::Integer(value) => Some(f64::from(*value)),
        DescValue::LargeInteger(value) => Some(*value as f64),
        _ => None,
    }
}

fn text(value: &DescValue) -> Option<String> {
    match value {
        DescValue::Text(units) => Some(units.to_string_lossy()),
        _ => None,
    }
}

fn flag(value: &DescValue) -> Option<bool> {
    match value {
        DescValue::Boolean(on) => Some(*on),
        _ => None,
    }
}

fn object(value: &DescValue) -> Option<&Descriptor> {
    match value {
        DescValue::Descriptor(desc) | DescValue::GlobalObject(desc) => Some(desc),
        _ => None,
    }
}

fn class_is(desc: &Descriptor, names: &[&str]) -> bool {
    event_is(&id_text(&desc.class_id), names)
}

fn walk(value: &DescValue, visit: &mut impl FnMut(&ReferenceItem)) {
    match value {
        DescValue::Reference(items) => {
            for item in items {
                visit(item);
            }
        }
        DescValue::Descriptor(desc) | DescValue::GlobalObject(desc) => {
            for (_, child) in &desc.items {
                walk(child, visit);
            }
        }
        DescValue::List(items) => {
            for child in items {
                walk(child, visit);
            }
        }
        _ => {}
    }
}

fn mentions(desc: &Descriptor, needle: &[&str]) -> bool {
    let mut found = false;
    for (_, value) in &desc.items {
        walk(value, &mut |item| {
            let hit = match item {
                ReferenceItem::Property { key, .. } => event_is(&id_text(key), needle),
                ReferenceItem::Enumerated { value, type_id, .. } => event_is(&id_text(value), needle) || event_is(&id_text(type_id), needle),
                ReferenceItem::Class(class) | ReferenceItem::Name { class, .. } | ReferenceItem::Offset { class, .. } => {
                    event_is(&id_text(&class.class_id), needle)
                }
                _ => false,
            };
            found |= hit;
        });
    }
    found
}

fn rgb_hex(desc: &Descriptor) -> Option<String> {
    let color = find(desc, &["Clr ", "color"]).and_then(object)?;
    let channel = |keys: &[&str]| find(color, keys).and_then(number);
    let (r, g, b) = (channel(&["Rd  ", "red"])?, channel(&["Grn ", "green"])?, channel(&["Bl  ", "blue"])?);
    let byte = |c: f64| -> u8 {
        let c = if (0.0..=1.0).contains(&c) { c * 255.0 } else { c };
        c.round().clamp(0.0, 255.0) as u8
    };
    Some(format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b)))
}

fn dialog_on(command: &ActionCommand) -> bool {
    match command.dialog_options {
        1 => true,
        2 => false,
        _ => command.with_dialog,
    }
}

/// A step PhotoCraft can play, or a reason it was left out.
fn map_command(command: &ActionCommand) -> std::result::Result<(String, Value, bool), String> {
    let desc = command.descriptor.as_ref();
    let event = command.event.as_str();
    let mapped = if event_is(event, &["setd", "set"]) {
        if desc.is_some_and(|d| mentions(d, &["Trsp", "transparencyEnum", "transparency"])) {
            Some(("select.loadSelection".to_string(), json!({"channel": "transparency"})))
        } else {
            None
        }
    } else if event_is(event, &["Cntc", "contract"]) {
        Some(modify(desc, "select.modify.contract"))
    } else if event_is(event, &["Expn", "expand"]) {
        Some(modify(desc, "select.modify.expand"))
    } else if event_is(event, &["Fthr", "feather"]) {
        Some(modify(desc, "select.modify.feather"))
    } else if event_is(event, &["Invs", "inverse"]) {
        Some(("select.inverse".to_string(), json!({})))
    } else if event_is(event, &["Mk  ", "Mk", "make"]) {
        desc.and_then(map_make)
    } else if event_is(event, &["stop"]) {
        Some(map_stop(desc))
    } else {
        None
    };
    match mapped {
        Some((id, params)) => Ok((id, params, dialog_on(command))),
        None => {
            let name = if command.dictionary_name.is_empty() { event.trim().to_string() } else { command.dictionary_name.clone() };
            Err(name)
        }
    }
}

fn modify(desc: Option<&Descriptor>, id: &str) -> (String, Value) {
    let radius = desc.and_then(|d| find(d, &["By  ", "by", "Rds "]).and_then(number)).unwrap_or(1.0);
    let bounds = desc.and_then(|d| find(d, &["selectionModifyEffectAtCanvasBounds", "AtCb"]).and_then(flag)).unwrap_or(false);
    (id.to_string(), json!({"radius": radius, "applyAtCanvasBounds": bounds}))
}

fn map_make(desc: &Descriptor) -> Option<(String, Value)> {
    let new = find(desc, &["Nw  ", "new", "using"]).and_then(object)?;
    if class_is(new, &["SCch", "spotColorChannel", "SptC"]) {
        let mut params = json!({"fromSelection": true});
        if let Some(name) = find(new, &["Nm  ", "name"]).and_then(text) {
            params["name"] = json!(name);
        }
        if let Some(color) = rgb_hex(new) {
            params["color"] = json!(color);
        }
        if let Some(solidity) = find(new, &["Opct", "solidity", "Scld"]).and_then(number) {
            params["solidity"] = json!(solidity);
        }
        return Some(("channel.newSpot".to_string(), params));
    }
    if class_is(new, &["Lyr ", "layer"]) {
        let mut params = json!({});
        if let Some(name) = find(new, &["Nm  ", "name"]).and_then(text) {
            params["name"] = json!(name);
        }
        return Some(("layer.new.layer".to_string(), params));
    }
    None
}

fn map_stop(desc: Option<&Descriptor>) -> (String, Value) {
    let message = desc.and_then(|d| find(d, &["Msg ", "message"]).and_then(text)).unwrap_or_default();
    let allow = desc.and_then(|d| find(d, &["Cont", "continue"]).and_then(flag)).unwrap_or(true);
    ("actions.message".to_string(), json!({"message": message, "allowContinue": allow}))
}

fn unique_name(list: &[Action], name: &str) -> Result<String> {
    let base = if name.is_empty() { "Action".to_string() } else { name.to_string() };
    if !list.iter().any(|action| action.name == base) {
        return Ok(base);
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base} {n}");
        if !list.iter().any(|action| action.name == candidate) {
            return Ok(candidate);
        }
        n = n.checked_add(1).ok_or_else(|| bad("could not allocate a unique action name"))?;
        if n > 100_000 {
            return Err(bad("could not allocate a unique action name"));
        }
    }
}

fn shortcut_of(key: u16, shift: bool, command: bool) -> String {
    if !(1..=12).contains(&key) {
        return String::new();
    }
    let key = format!("F{key}");
    match (command, shift) {
        (true, true) => format!("Ctrl+Shift+{key}"),
        (true, false) => format!("Ctrl+{key}"),
        (false, true) => format!("Shift+{key}"),
        (false, false) => key,
    }
}

struct Built {
    action: Action,
    shortcut: String,
}

fn build(file: &ActionFile, list: &[Action]) -> Result<(Vec<Built>, Vec<String>)> {
    let mut built = Vec::new();
    let mut skipped = Vec::new();
    let mut names: Vec<Action> = list.to_vec();
    for item in &file.actions {
        let mut steps = Vec::new();
        let mut enabled = Vec::new();
        let mut dialogs = Vec::new();
        for (index, command) in item.commands.iter().enumerate() {
            match map_command(command) {
                Ok((id, params, dialog)) => {
                    enabled.push(command.enabled);
                    dialogs.push(dialog);
                    steps.push((id, params));
                }
                Err(name) => {
                    skipped.push(format!("{} / {} — {name}", item.name, index + 1));
                }
            }
        }
        let name = unique_name(&names, &item.name)?;
        let action = Action {
            name: name.clone(),
            steps,
            enabled: true,
            color: color_from_index(item.color_index).to_string(),
            step_enabled: enabled,
            step_dialog: dialogs,
        };
        names.push(Action::new(name, Vec::new()));
        built.push(Built { shortcut: shortcut_of(item.function_key, item.shift, item.command), action });
    }
    Ok((built, skipped))
}

fn decoded_bytes(p: &Value) -> std::result::Result<Vec<u8>, EngineError> {
    if let Some(values) = p.get("bytes").and_then(Value::as_array) {
        if values.len() > 8_000_000 {
            return Err(bad("action file is too large"));
        }
        let mut out = Vec::with_capacity(values.len());
        for value in values {
            let Some(n) = value.as_u64() else { return Err(bad("bytes must be integers 0..255")) };
            if n > 255 {
                return Err(bad("bytes must be integers 0..255"));
            }
            out.push(n as u8);
        }
        return Ok(out);
    }
    let Some(text) = p.get("base64").and_then(Value::as_str) else {
        return Err(bad("needs \"base64\" or \"bytes\""));
    };
    if text.len() > 12_000_000 {
        return Err(bad("action file is too large"));
    }
    decode_base64(text).map_err(bad)
}

fn decode_base64(text: &str) -> std::result::Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 length is not a multiple of 4".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let chunk = chunk.get(..4).ok_or("base64 length is not a multiple of 4")?;
        let pad = chunk.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 {
            return Err("base64 padding is invalid".into());
        }
        let mut acc = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            let digit = if *byte == b'=' { 0 } else { val(*byte).ok_or("base64 contains a character that is not base64")? };
            acc |= u32::from(digit) << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
    }
    Ok(out)
}

/// Replace or append the actions in a `.atn` payload.
pub(crate) fn import_atn(s: &mut Session, p: &Value) -> Result<Value> {
    if s.actions.playing > 0 || s.actions.recording.is_some() {
        return Err(bad("stop playback and recording before loading actions"));
    }
    let bytes = decoded_bytes(p)?;
    let file = photocraft_psd::atn::parse(&bytes).map_err(|e| bad(e.to_string()))?;
    let replace = p.get("replace").and_then(Value::as_bool).unwrap_or(false);
    if replace {
        s.actions.list.clear();
        s.actions.sets.clear();
        s.actions.folders.clear();
    }
    let (built, skipped) = build(&file, &s.actions.list)?;
    let set = file.name.clone();
    let mut shortcuts = Vec::new();
    for item in built {
        if !item.shortcut.is_empty() {
            shortcuts.push(json!({"action": item.action.name, "shortcut": item.shortcut}));
        }
        actions_cmds::push_action(s, item.action, &set)?;
    }
    Ok(json!({"set": set, "skipped": skipped, "shortcuts": shortcuts, "actions": s.actions.list.len()}))
}

/// Append or replace from a PhotoCraft actions JSON document (`ActionsFile`).
pub(crate) fn import_json(s: &mut Session, p: &Value) -> Result<Value> {
    if s.actions.playing > 0 || s.actions.recording.is_some() {
        return Err(EngineError::BadParams { cmd: "actions.importJson".into(), msg: "stop playback and recording before loading actions".into() });
    }
    let Some(text) = p.get("text").and_then(Value::as_str) else {
        return Err(EngineError::BadParams { cmd: "actions.importJson".into(), msg: "needs \"text\"".into() });
    };
    if text.len() > 8_000_000 {
        return Err(EngineError::BadParams { cmd: "actions.importJson".into(), msg: "action file is too large".into() });
    }
    let file: actions_cmds::ActionsFile = serde_json::from_str(text)
        .map_err(|e| EngineError::BadParams { cmd: "actions.importJson".into(), msg: format!("not a PhotoCraft actions file: {e}") })?;
    if file.version != 1 && file.version != actions_cmds::ACTIONS_VERSION {
        return Err(EngineError::BadParams { cmd: "actions.importJson".into(), msg: format!("unsupported actions file version {}", file.version) });
    }
    let replace = p.get("replace").and_then(Value::as_bool).unwrap_or(false);
    if replace {
        s.actions.list.clear();
        s.actions.sets.clear();
        s.actions.folders.clear();
    }
    let mut folders = file.folders;
    if folders.len() < file.actions.len() {
        folders.resize(file.actions.len(), String::new());
    }
    for (action, folder) in file.actions.into_iter().zip(folders) {
        let name = unique_name(&s.actions.list, &action.name).map_err(|e| EngineError::BadParams { cmd: "actions.importJson".into(), msg: e.to_string() })?;
        let mut action = action;
        action.name = name;
        actions_cmds::push_action(s, action, &folder).map_err(|e| EngineError::BadParams { cmd: "actions.importJson".into(), msg: e.to_string() })?;
    }
    for set in file.sets {
        actions_cmds::ensure_set(&mut s.actions, &set).map_err(|e| EngineError::BadParams { cmd: "actions.importJson".into(), msg: e.to_string() })?;
    }
    Ok(json!({"actions": s.actions.list.len()}))
}

/// The on-disk JSON for one set, or for every action when `set` is omitted.
pub(crate) fn export_json(s: &mut Session, p: &Value) -> Result<Value> {
    s.actions.align_folders();
    let set = p.get("set").and_then(Value::as_str);
    let mut actions = Vec::new();
    let mut folders = Vec::new();
    let mut sets = Vec::new();
    for (i, action) in s.actions.list.iter().enumerate() {
        let folder = s.actions.folder(i);
        if set.is_some_and(|name| name != folder) {
            continue;
        }
        if !folder.is_empty() && !sets.iter().any(|have: &String| have == folder) {
            sets.push(folder.to_string());
        }
        actions.push(action.clone());
        folders.push(folder.to_string());
    }
    if let Some(name) = set.filter(|name| !name.is_empty())
        && !sets.iter().any(|have| have == name)
        && s.actions.sets.iter().any(|have| have == name)
    {
        sets.push(name.to_string());
    }
    let file = actions_cmds::ActionsFile { version: actions_cmds::ACTIONS_VERSION, actions, sets, folders };
    let text = serde_json::to_string_pretty(&file).map_err(|e| EngineError::BadParams { cmd: "actions.exportJson".into(), msg: e.to_string() })?;
    Ok(json!({"text": text}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_psd::descriptor::{Descriptor, Value as DescValue};

    fn put_u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn put_u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn put_i32(out: &mut Vec<u8>, v: i32) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    fn put_unicode(out: &mut Vec<u8>, text: &str) {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        units.push(0);
        put_u32(out, units.len() as u32);
        for unit in units {
            put_u16(out, unit);
        }
    }

    fn put_bytes(out: &mut Vec<u8>, text: &str) {
        put_u32(out, text.len() as u32);
        out.extend_from_slice(text.as_bytes());
    }

    fn command(out: &mut Vec<u8>, event: &[u8; 4], enabled: bool, dialog: bool, desc: Option<&Descriptor>) {
        out.push(1);
        out.push(u8::from(enabled));
        out.push(u8::from(dialog));
        out.push(0);
        out.extend_from_slice(b"long");
        out.extend_from_slice(event);
        put_bytes(out, "");
        match desc {
            Some(desc) => {
                put_i32(out, -1);
                out.extend_from_slice(&desc.to_bytes());
            }
            None => put_i32(out, 0),
        }
    }

    /// The user's DTF action: transparency selection, contract 1 px, red spot at solidity 60,
    /// plus a Gaussian Blur that PhotoCraft does not import from an action file.
    fn dtf_atn() -> Vec<u8> {
        let set = Descriptor::new("null")
            .with(
                "null",
                DescValue::Reference(vec![photocraft_psd::descriptor::ReferenceItem::Property {
                    class: photocraft_psd::descriptor::Class { name: Default::default(), class_id: photocraft_psd::descriptor::Id::new("Chnl") },
                    key: photocraft_psd::descriptor::Id::new("fsel"),
                }]),
            )
            .with(
                "T   ",
                DescValue::Reference(vec![photocraft_psd::descriptor::ReferenceItem::Enumerated {
                    class: photocraft_psd::descriptor::Class { name: Default::default(), class_id: photocraft_psd::descriptor::Id::new("Chnl") },
                    type_id: photocraft_psd::descriptor::Id::new("Chnl"),
                    value: photocraft_psd::descriptor::Id::new("Trsp"),
                }]),
            );
        let contract = Descriptor::new("null")
            .with("By  ", DescValue::UnitFloat { unit: *b"#Pxl", value: 1.0 })
            .with("selectionModifyEffectAtCanvasBounds", DescValue::Boolean(false));
        let spot = Descriptor::new("null").with(
            "Nw  ",
            DescValue::Descriptor(
                Descriptor::new("SCch")
                    .with("Nm  ", DescValue::Text(photocraft_psd::descriptor::UnicodeString::new_nul("Spot Color 1")))
                    .with(
                        "Clr ",
                        DescValue::Descriptor(
                            Descriptor::new("RGBC")
                                .with("Rd  ", DescValue::Double(255.0))
                                .with("Grn ", DescValue::Double(0.0))
                                .with("Bl  ", DescValue::Double(0.0)),
                        ),
                    )
                    .with("Opct", DescValue::Integer(60)),
            ),
        );
        let mut out = Vec::new();
        put_u32(&mut out, 16);
        put_unicode(&mut out, "DTF");
        out.push(1);
        put_u32(&mut out, 1);
        put_u16(&mut out, 3);
        out.push(0);
        out.push(1);
        put_u16(&mut out, 5);
        put_unicode(&mut out, "DTF");
        out.push(1);
        put_u32(&mut out, 4);
        command(&mut out, b"setd", true, false, Some(&set));
        command(&mut out, b"Cntc", true, false, Some(&contract));
        command(&mut out, b"Mk  ", false, true, Some(&spot));
        command(&mut out, b"GsnB", true, false, None);
        out
    }

    #[test]
    fn dtf_atn_keeps_the_three_supported_steps_and_reports_the_blur() {
        let mut s = Session::new();
        let bytes: Vec<Value> = dtf_atn().into_iter().map(|b| json!(b)).collect();
        let v = s.execute("actions.importAtn", json!({"bytes": bytes})).unwrap();
        assert_eq!(v["set"], "DTF");
        assert_eq!(v["skipped"], json!(["DTF / 4 — GsnB"]));
        assert_eq!(v["shortcuts"][0]["shortcut"], "Ctrl+F3");
        assert_eq!(s.actions.folder(0), "DTF");
        let action = &s.actions.list[0];
        assert_eq!(action.name, "DTF");
        assert_eq!(action.color, "blue");
        assert_eq!(action.steps.iter().map(|step| step.0.as_str()).collect::<Vec<_>>(), ["select.loadSelection", "select.modify.contract", "channel.newSpot"]);
        assert_eq!(action.steps[0].1["channel"], "transparency");
        assert_eq!(action.steps[1].1["radius"].as_f64(), Some(1.0));
        assert_eq!(action.steps[1].1["applyAtCanvasBounds"], false);
        assert_eq!(action.steps[2].1["name"], "Spot Color 1");
        assert_eq!(action.steps[2].1["color"], "#ff0000");
        assert_eq!(action.steps[2].1["solidity"].as_f64(), Some(60.0));
        assert!(!action.step_on(2), "the Make step was unchecked in the file");
        assert!(action.step_dialog_on(2));
        assert!(s.execute("actions.importAtn", json!({"base64": "@@@@"})).is_err());
        assert!(s.execute("actions.importAtn", json!({})).is_err());
    }

    #[test]
    fn json_round_trip_keeps_checks_dialogs_and_colour() {
        let mut s = Session::new();
        s.execute("actions.record", json!({"name": "Paint", "set": "Studio"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        s.execute("actions.setColor", json!({"action": "Paint", "color": "green"})).unwrap();
        let text = s.execute("actions.exportJson", json!({"set": "Studio"})).unwrap();
        let mut other = Session::new();
        other.execute("actions.importJson", json!({"text": text["text"], "replace": true})).unwrap();
        assert_eq!(other.actions.list.len(), 1);
        assert_eq!(other.actions.list[0].name, "Paint");
        assert_eq!(other.actions.list[0].color, "green");
        assert_eq!(other.actions.folder(0), "Studio");
        assert!(other.execute("actions.importJson", json!({"text": "{"})).is_err());
    }
}
