//! Photoshop actions files (`.atn`).
//!
//! A `.atn` file is one action set: a Unicode name, then each action's function key, button
//! colour and commands. A command is an event id plus an optional ActionDescriptor in the same
//! object layout as [`crate::descriptor::Descriptor`] (no version word in front of it).
//!
//! The layout follows the public notes at
//! <https://tonton-pixel.codeberg.page/photoshop-file-formats/actions-file-format.html>,
//! which fill in the Actions section of Adobe's Photoshop file-format specification.
//! PhotoCraft maps the events it understands; this module only reads the file.

use crate::descriptor::Descriptor;
use crate::error::{PsdError, Result};
use crate::io::Reader;

/// Commands in one file. Action files are small; this rejects hostile counts before allocating.
const MAX_COUNT: u32 = 100_000;
/// A name or event id longer than this is not a Photoshop action file.
const MAX_STRING: u32 = 1_000_000;

/// One `.atn` file: a single action set.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionFile {
    /// File version. Photoshop writes 16.
    pub version: u32,
    /// Set name, without a trailing NUL.
    pub name: String,
    /// Whether the set was expanded in the Actions panel.
    pub expanded: bool,
    /// Actions in panel order.
    pub actions: Vec<ActionItem>,
}

/// One action inside a set.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionItem {
    /// Function key number. 0 means none; 1 is F1.
    pub function_key: u16,
    /// Shift is part of the shortcut.
    pub shift: bool,
    /// Command (macOS) or Ctrl (Windows) is part of the shortcut.
    pub command: bool,
    /// Button-mode colour. 0 none, 1 red, 2 orange, 3 yellow, 4 green, 5 blue, 6 violet, 7 gray.
    pub color_index: u16,
    /// Action name, without a trailing NUL.
    pub name: String,
    /// Whether the action was expanded.
    pub expanded: bool,
    /// Recorded commands.
    pub commands: Vec<ActionCommand>,
}

/// One recorded command.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionCommand {
    /// Whether the command was expanded to show its parameters.
    pub expanded: bool,
    /// Included when the action plays.
    pub enabled: bool,
    /// The dialog toggle in the Actions panel is on.
    pub with_dialog: bool,
    /// Dialog options recorded with the command (0 optional, 1 required, 2 none).
    pub dialog_options: u8,
    /// Event id: a 4-character code (spaces preserved, so `Mk  ` stays four characters) or a string id.
    pub event: String,
    /// Dictionary name stored with the event. Often empty.
    pub dictionary_name: String,
    /// Parameters, when the command has a descriptor.
    pub descriptor: Option<Descriptor>,
}

/// Parse a `.atn` file. Trailing bytes are an error.
pub fn parse(data: &[u8]) -> Result<ActionFile> {
    let mut r = Reader::new(data);
    let version = r.u32()?;
    if version != 16 {
        return Err(PsdError::invalid(format!("action file version {version}, expected 16")));
    }
    let file = read_set(&mut r)?;
    if !r.is_empty() {
        return Err(PsdError::invalid("trailing bytes after the action set"));
    }
    Ok(file)
}

fn read_set(r: &mut Reader<'_>) -> Result<ActionFile> {
    let name = read_unicode(r)?;
    let expanded = read_bool(r)?;
    let count = r.u32()?;
    if count > MAX_COUNT {
        return Err(PsdError::LimitExceeded("too many actions"));
    }
    r.check_count(u64::from(count), 8)?;
    let mut actions = Vec::with_capacity(count as usize);
    for _ in 0..count {
        actions.push(read_action(r)?);
    }
    Ok(ActionFile { version: 16, name, expanded, actions })
}

fn read_action(r: &mut Reader<'_>) -> Result<ActionItem> {
    let function_key = r.u16()?;
    let shift = read_bool(r)?;
    let command = read_bool(r)?;
    let color_index = r.u16()?;
    let name = read_unicode(r)?;
    let expanded = read_bool(r)?;
    let count = r.u32()?;
    if count > MAX_COUNT {
        return Err(PsdError::LimitExceeded("too many action commands"));
    }
    r.check_count(u64::from(count), 8)?;
    let mut commands = Vec::with_capacity(count as usize);
    for _ in 0..count {
        commands.push(read_command(r)?);
    }
    Ok(ActionItem { function_key, shift, command, color_index, name, expanded, commands })
}

fn read_command(r: &mut Reader<'_>) -> Result<ActionCommand> {
    let expanded = read_bool(r)?;
    let enabled = read_bool(r)?;
    let with_dialog = read_bool(r)?;
    let dialog_options = r.u8()?;
    let kind = r.array::<4>()?;
    let event = match &kind {
        b"long" => {
            let code = r.array::<4>()?;
            String::from_utf8_lossy(&code).into_owned()
        }
        b"TEXT" => read_byte_string(r)?,
        _ => {
            return Err(PsdError::invalid(format!("action event id selector {}", String::from_utf8_lossy(&kind))));
        }
    };
    let dictionary_name = read_byte_string(r)?;
    let has = r.i32()?;
    let descriptor = match has {
        0 => None,
        -1 => Some(Descriptor::read(r)?),
        _ => return Err(PsdError::invalid(format!("action descriptor flag {has}"))),
    };
    Ok(ActionCommand { expanded, enabled, with_dialog, dialog_options, event, dictionary_name, descriptor })
}

fn read_bool(r: &mut Reader<'_>) -> Result<bool> {
    Ok(r.u8()? != 0)
}

/// Photoshop Unicode string: a count of UTF-16 code units, including the terminating NUL.
fn read_unicode(r: &mut Reader<'_>) -> Result<String> {
    let n = r.u32()?;
    if n > MAX_STRING {
        return Err(PsdError::LimitExceeded("action name is too long"));
    }
    r.check_count(u64::from(n), 2)?;
    let mut units = Vec::with_capacity(n as usize);
    for _ in 0..n {
        units.push(r.u16()?);
    }
    while units.last() == Some(&0) {
        units.pop();
    }
    Ok(String::from_utf16_lossy(&units))
}

/// Byte string: a count, then that many bytes. Not NUL-terminated.
fn read_byte_string(r: &mut Reader<'_>) -> Result<String> {
    let n = r.u32()?;
    if n > MAX_STRING {
        return Err(PsdError::LimitExceeded("action string is too long"));
    }
    let bytes = r.bytes(n as usize)?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{Descriptor, Value};
    use crate::io::WriteExt;

    fn put_unicode(out: &mut Vec<u8>, text: &str) {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        units.push(0);
        out.put_u32(units.len() as u32);
        for unit in units {
            out.put_u16(unit);
        }
    }

    fn put_bytes(out: &mut Vec<u8>, text: &str) {
        out.put_u32(text.len() as u32);
        out.extend_from_slice(text.as_bytes());
    }

    /// A one-command set whose descriptor is a contract of 1 pixel.
    fn sample() -> Vec<u8> {
        let desc = Descriptor::new("null").with("By  ", Value::UnitFloat { unit: *b"#Pxl", value: 1.0 });
        let mut out = Vec::new();
        out.put_u32(16);
        put_unicode(&mut out, "DTF");
        out.push(1);
        out.put_u32(1);
        out.put_u16(2);
        out.push(1);
        out.push(0);
        out.put_u16(1);
        put_unicode(&mut out, "Choke");
        out.push(1);
        out.put_u32(1);
        out.push(1);
        out.push(1);
        out.push(0);
        out.push(0);
        out.extend_from_slice(b"long");
        out.extend_from_slice(b"Cntc");
        put_bytes(&mut out, "");
        out.put_i32(-1);
        desc.write(&mut out);
        out
    }

    #[test]
    fn reads_a_set_action_and_descriptor() {
        let file = parse(&sample()).unwrap();
        assert_eq!(file.name, "DTF");
        assert!(file.expanded);
        assert_eq!(file.actions.len(), 1);
        let action = &file.actions[0];
        assert_eq!(action.name, "Choke");
        assert_eq!(action.function_key, 2);
        assert!(action.shift);
        assert!(!action.command);
        assert_eq!(action.color_index, 1);
        assert_eq!(action.commands.len(), 1);
        let command = &action.commands[0];
        assert_eq!(command.event, "Cntc");
        assert!(command.enabled);
        assert!(!command.with_dialog);
        let desc = command.descriptor.as_ref().unwrap();
        match desc.get("By  ") {
            Some(Value::UnitFloat { unit, value }) => {
                assert_eq!(unit, b"#Pxl");
                assert_eq!(*value, 1.0);
            }
            other => panic!("By: {other:?}"),
        }
    }

    #[test]
    fn rejects_a_short_file_and_a_bad_version() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0, 0, 0, 15]).is_err());
        let mut bytes = sample();
        bytes.push(0);
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn string_event_ids_and_a_missing_descriptor() {
        let mut out = Vec::new();
        out.put_u32(16);
        put_unicode(&mut out, "Set");
        out.push(0);
        out.put_u32(1);
        out.put_u16(0);
        out.push(0);
        out.push(0);
        out.put_u16(0);
        put_unicode(&mut out, "Stop");
        out.push(0);
        out.put_u32(1);
        out.push(0);
        out.push(0);
        out.push(1);
        out.push(1);
        out.extend_from_slice(b"TEXT");
        put_bytes(&mut out, "stop");
        put_bytes(&mut out, "Stop");
        out.put_i32(0);
        let file = parse(&out).unwrap();
        let command = &file.actions[0].commands[0];
        assert_eq!(command.event, "stop");
        assert_eq!(command.dictionary_name, "Stop");
        assert!(command.descriptor.is_none());
        assert!(!command.enabled);
        assert!(command.with_dialog);
        assert_eq!(command.dialog_options, 1);
    }
}
