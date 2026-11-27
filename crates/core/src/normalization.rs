/// Normalizes volatile fragments (timestamps, elapsed durations, hex addresses) in failure text.
pub fn normalize_failure_output(text: &str) -> String {
    let mut lines = Vec::new();
    for raw_line in text.replace("\r\n", "\n").lines() {
        let trimmed = raw_line.trim_end();
        let mut normalized_tokens = Vec::new();
        for token in trimmed.split_whitespace() {
            normalized_tokens.push(normalize_failure_token(token));
        }
        lines.push(strip_rust_thread_id(normalized_tokens.join(" ")));
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn strip_rust_thread_id(mut line: String) -> String {
    if line.starts_with("thread '")
        && let Some(close) = line.find(") panicked at")
        && let Some(open) = line[..close].rfind(" (")
        && open + 2 < close
        && line[open + 2..close].bytes().all(|b| b.is_ascii_digit())
    {
        line.replace_range(open..=close, "");
    }
    line
}

fn normalize_failure_token(token: &str) -> String {
    let stripped =
        token.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | ',' | ';' | '.'));
    if is_iso_timestamp(stripped) {
        return "<TS>".to_owned();
    }
    if is_duration_token(stripped) {
        return "<DUR>".to_owned();
    }
    if is_hex_pointer(stripped) {
        return "<HEX>".to_owned();
    }
    token.to_owned()
}

fn is_iso_timestamp(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 19
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && matches!(bytes[10], b'T' | b' ')
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b':'
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b':'
        && bytes[17..19].iter().all(u8::is_ascii_digit)
}

fn is_duration_token(s: &str) -> bool {
    let Some(num) = s
        .strip_suffix("ms")
        .or_else(|| s.strip_suffix("us"))
        .or_else(|| s.strip_suffix("ns"))
        .or_else(|| s.strip_suffix('s'))
    else {
        return false;
    };
    if num.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for b in num.bytes() {
        if b.is_ascii_digit() {
            seen_digit = true;
        } else if b == b'.' && !seen_dot {
            seen_dot = true;
        } else {
            return false;
        }
    }
    seen_digit
}

fn is_hex_pointer(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) else {
        return false;
    };
    rest.len() >= 6 && rest.bytes().all(|b| b.is_ascii_hexdigit())
}
