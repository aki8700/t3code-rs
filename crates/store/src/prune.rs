use serde_json::{Value, json};
use t3_core::{CoreError, CoreResult};

use super::encode;

pub(super) fn prune_payload(
    mut payload: Value,
    original: String,
    max_bytes: usize,
) -> CoreResult<(Value, String)> {
    debug_assert!(original.len() > max_bytes);
    // Reserve space for short metadata values before sharing the remaining budget.
    const LEAF_RESERVE: usize = 64;
    let mut string_bytes = 0;
    let mut reserved = 0;
    visit_strings(&mut payload, &mut |text| {
        let size = escaped_bytes(text);
        string_bytes += size;
        reserved += size.min(LEAF_RESERVE);
    });
    let structure_bytes = original.len() - string_bytes;
    if let Some(budget) = max_bytes.checked_sub(structure_bytes) {
        let (reserve_per_leaf, reserved) = if reserved <= budget {
            (LEAF_RESERVE, reserved)
        } else {
            (0, 0)
        };
        visit_strings(&mut payload, &mut |text| {
            let size = escaped_bytes(text);
            let guaranteed = size.min(reserve_per_leaf);
            // The structure fits and the original is oversized, so there are
            // always string bytes left to distribute after reserving space.
            let share = ((size - guaranteed) as u128 * (budget - reserved) as u128
                / (string_bytes - reserved) as u128) as usize;
            let limit = guaranteed + share;
            if size > limit {
                *text = head_tail(text, limit);
            }
        });
        let encoded = encode(&payload)?;
        if encoded.len() <= max_bytes {
            return Ok((payload, encoded));
        }
    }

    let mut preview = json!({
        "truncated": true,
        "original_bytes": original.len(),
        "preview": "",
    });
    let overhead = encode(&preview)?.len();
    if let Some(budget) = max_bytes.checked_sub(overhead) {
        preview["preview"] = Value::String(head_tail(&original, budget));
    } else {
        preview = match max_bytes {
            0 => {
                return Err(CoreError::InvalidArgument(
                    "payload byte limit must be positive".into(),
                ));
            }
            1 => Value::Number(0.into()),
            _ => Value::String(head_tail(&original, max_bytes - 2)),
        };
    }
    let encoded = encode(&preview)?;
    Ok((preview, encoded))
}

fn visit_strings(value: &mut Value, visit: &mut impl FnMut(&mut String)) {
    match value {
        Value::String(text) => visit(text),
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| visit_strings(value, visit)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|value| visit_strings(value, visit)),
        _ => {}
    }
}

fn escaped_bytes(text: &str) -> usize {
    text.bytes().map(escaped_byte_width).sum()
}

// The width of a byte in serde_json's compact string encoding.
fn escaped_byte_width(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
        0x00..=0x1f => 6,
        _ => 1,
    }
}

fn head_tail(text: &str, budget: usize) -> String {
    // Reserve for the longest possible marker including its escaped newlines.
    let marker_bytes =
        escaped_bytes("\n...[pruned  chars]...\n") + text.len().ilog10() as usize + 1;
    let has_marker = budget >= marker_bytes;
    let budget = if has_marker {
        budget - marker_bytes
    } else {
        budget
    };
    let head_budget = budget / 5 * 3 + budget % 5 * 3 / 5;
    let tail_budget = budget - head_budget;
    let (head, head_chars) = fitting_chars(text.chars(), head_budget);
    let (tail, tail_chars) = fitting_chars(text.chars().rev(), tail_budget);
    let head = &text[..head];
    let tail = &text[text.len() - tail..];
    if has_marker {
        let removed = text.chars().count() - head_chars - tail_chars;
        format!("{head}\n...[pruned {removed} chars]...\n{tail}")
    } else {
        format!("{head}{tail}")
    }
}

fn fitting_chars(chars: impl Iterator<Item = char>, mut budget: usize) -> (usize, usize) {
    let mut bytes = 0;
    let mut count = 0;
    for ch in chars {
        let width = if ch.is_ascii() {
            escaped_byte_width(ch as u8)
        } else {
            ch.len_utf8()
        };
        if width > budget {
            break;
        }
        budget -= width;
        bytes += ch.len_utf8();
        count += 1;
    }
    (bytes, count)
}
