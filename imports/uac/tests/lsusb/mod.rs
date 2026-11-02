// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Rebuilds descriptor bytes from `lsusb -v` text, so the tests run on published dumps of
//! real devices.
//!
//! lsusb prints every field of a descriptor in wire order, one per line, with the value as
//! the first token after the name. The width of a field follows from its name (b, i: 1
//! byte, w, id, bcd: 2, t: 3, dw: 4) except for bitmaps, which lsusb prints in hex with as
//! many digits as the field has. Every rebuilt descriptor has to add up to its bLength
//! and every configuration to its wTotalLength, which catches a misread width.

/// One configuration's bytes, with the device descriptor in front: what a host fetches.
pub struct Dump {
    pub bytes: Vec<u8>,
}

fn field_width(name: &str, value: &str) -> Option<usize> {
    let hex_width = || value.strip_prefix("0x").map(|digits| digits.len() / 2);
    if name == "MaxPower" {
        Some(1)
    } else if name == "bmFormats" && !value.starts_with("0x") {
        // UAC2 AS_GENERAL; older lsusb prints it in decimal.
        Some(4)
    } else if name.starts_with("bcd") || name.starts_with("id") || name.starts_with('w') {
        Some(2)
    } else if name.starts_with("dw") {
        Some(4)
    } else if name.starts_with('t') {
        Some(3)
    } else if name.starts_with("bm") {
        Some(hex_width().unwrap_or(1))
    } else if name.starts_with('b') || name.starts_with('i') || name.starts_with("Ba") {
        Some(1)
    } else {
        None
    }
}

fn field_value(name: &str, value: &str) -> Option<u64> {
    if name == "MaxPower" {
        // USB 2.0 counts in 2 mA units.
        return value.strip_suffix("mA")?.parse::<u64>().ok().map(|ma| ma / 2);
    }
    if name.starts_with("bcd") {
        let (major, minor) = value.split_once('.')?;
        return Some(u64::from_str_radix(major, 16).ok()? << 8 | u64::from_str_radix(minor, 16).ok()?);
    }
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

/// The fields of one descriptor, in order: (name, width, value).
type Fields = Vec<(String, usize, u64)>;

fn emit(fields: &Fields, out: &mut Vec<u8>, context: &str) {
    if fields.is_empty() || fields[0].0 != "bLength" {
        return;
    }
    let length = fields[0].2 as usize;
    let start = out.len();
    for (_, width, value) in fields {
        out.extend_from_slice(&value.to_le_bytes()[..*width]);
    }
    assert_eq!(out.len() - start, length, "fields of {context} do not add up to bLength: {fields:?}");
}

fn is_heading(line: &str) -> bool {
    let line = line.trim();
    line.ends_with(':') && (line.contains("Descriptor") || line.contains("Association"))
}

/// Rebuild the device descriptor and configuration `configuration` (0 based) of a dump.
pub fn parse(text: &str, configuration: usize) -> Dump {
    let mut all = Vec::new();
    let mut fields: Fields = Vec::new();
    let mut heading = String::new();
    let mut config_starts = Vec::new();
    for raw in text.lines() {
        let line = raw.replace("( ", "(").replace("[ ", "[");
        if is_heading(&line) {
            emit(&fields, &mut all, &heading);
            fields.clear();
            heading = line.trim().to_string();
            if heading == "Configuration Descriptor:" {
                config_starts.push(all.len());
            }
            continue;
        }
        if let Some(hex) = line.trim().strip_prefix("** UNRECOGNIZED:") {
            emit(&fields, &mut all, &heading);
            fields.clear();
            all.extend(hex.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()));
            continue;
        }
        let mut tokens = line.split_whitespace();
        let (Some(name), Some(value)) = (tokens.next(), tokens.next()) else { continue };
        if !name.starts_with(|c: char| c.is_ascii_lowercase()) && name != "MaxPower" && !name.starts_with("Ba") {
            continue;
        }
        let (Some(width), Some(value)) = (field_width(name, value), field_value(name, value)) else {
            continue;
        };
        fields.push((name.to_string(), width, value));
    }
    emit(&fields, &mut all, &heading);

    let device_len = all[0] as usize;
    let start = config_starts[configuration];
    let end = config_starts.get(configuration + 1).copied().unwrap_or(all.len());
    let total_length = u16::from_le_bytes([all[start + 2], all[start + 3]]) as usize;
    assert_eq!(end - start, total_length, "configuration {configuration} does not add up to wTotalLength");

    let mut bytes = all[..device_len].to_vec();
    bytes.extend_from_slice(&all[start..end]);
    Dump { bytes }
}
