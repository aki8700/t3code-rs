const WINDOW_BYTES: usize = 8 * 1024;
const MIN_PERIOD: usize = 48;
const MIN_LINE_CHARS: usize = 32;
const LINE_REPEATS: usize = 8;
const MAX_PERIOD: usize = 512;
const BLOCK_REPEATS: usize = 6;

#[derive(Default)]
pub(crate) struct RepetitionWatchdog {
    window: String,
    pending: String,
    pending_overflow: bool,
    last_line: String,
    equal_lines: usize,
    fence: Option<(char, usize)>,
}

impl RepetitionWatchdog {
    pub(crate) fn push(&mut self, text: &str) -> bool {
        for fragment in text.split_inclusive('\n') {
            let body = fragment.strip_suffix('\n').unwrap_or(fragment);
            let take = body.floor_char_boundary(WINDOW_BYTES - self.pending.len());
            self.pending.push_str(&body[..take]);
            self.pending_overflow |= take < body.len();

            let trimmed = self.pending.trim();
            let marker = fence_marker(trimmed)
                .map(|(character, count, suffix)| (character, count, suffix.is_empty()));
            let exempt = self.fence.is_some()
                || marker.is_some()
                || markdown_row(trimmed)
                || self.pending.starts_with("    ")
                || self.pending.starts_with('\t')
                || (!self.pending_overflow
                    && !trimmed.is_empty()
                    && !trimmed.chars().any(char::is_alphanumeric));
            if exempt {
                self.window.clear();
            } else if self.push_blocks(fragment) {
                return true;
            }

            let trimmed = self.pending.trim();
            let eligible = !exempt
                && !self.pending_overflow
                && trimmed.chars().nth(MIN_LINE_CHARS - 1).is_some();
            if eligible && trimmed == self.last_line && self.equal_lines + 1 == LINE_REPEATS {
                return true;
            }
            if fragment.ends_with('\n') {
                if let Some((character, count, closed)) = marker {
                    match self.fence {
                        None => self.fence = Some((character, count)),
                        Some((open, width)) if character == open && count >= width && closed => {
                            self.fence = None;
                        }
                        _ => {}
                    }
                }
                if eligible {
                    if trimmed == self.last_line {
                        self.equal_lines += 1;
                    } else {
                        self.last_line.clear();
                        self.last_line.push_str(trimmed);
                        self.equal_lines = 1;
                    }
                } else {
                    self.equal_lines = 0;
                }
                self.pending.clear();
                self.pending_overflow = false;
            }
        }
        false
    }

    fn push_blocks(&mut self, mut text: &str) -> bool {
        while !text.is_empty() {
            let end = text.floor_char_boundary(WINDOW_BYTES);
            let (chunk, rest) = text.split_at(end);
            let previous = self.window.len();
            self.window.push_str(chunk);
            for (offset, ch) in chunk.char_indices() {
                let end = previous + offset + ch.len_utf8();
                let start = self
                    .window
                    .ceil_char_boundary(end.saturating_sub(WINDOW_BYTES));
                let tail = &self.window[start..end];
                if repeated_block(tail, ch) {
                    return true;
                }
            }
            let keep = self.window.len().saturating_sub(WINDOW_BYTES);
            self.window.drain(..self.window.ceil_char_boundary(keep));
            text = rest;
        }
        false
    }
}

fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let character = line.chars().next().filter(|ch| matches!(ch, '`' | '~'))?;
    let width = line.chars().take_while(|&ch| ch == character).count();
    (width >= 3).then(|| (character, width, line[width..].trim()))
}

fn markdown_row(line: &str) -> bool {
    line.starts_with('|') || line.ends_with('|') || line.contains(" | ")
}

fn repeated_block(tail: &str, last: char) -> bool {
    let end = tail.len();
    let max_period = MAX_PERIOD.min(end / BLOCK_REPEATS);
    if max_period < MIN_PERIOD
        || !tail[tail.ceil_char_boundary(end - max_period)..]
            .chars()
            .rev()
            .any(char::is_alphanumeric)
    {
        return false;
    }
    // Candidate periods must end at a previous occurrence of the last character.
    let start = tail.ceil_char_boundary(end - max_period - last.len_utf8());
    let stop = tail.floor_char_boundary(end - MIN_PERIOD);
    for (offset, _) in tail
        .get(start..stop)
        .unwrap_or_default()
        .rmatch_indices(last)
    {
        let period = end - (start + offset + last.len_utf8());
        let repeated = &tail.as_bytes()[end - period * BLOCK_REPEATS..];
        if repeated[..period * (BLOCK_REPEATS - 1)] != repeated[period..] {
            continue;
        }
        let block = &tail[end - period..];
        // A repeated single line belongs to the line detector. Longer multiples
        // of an exempt short period are also exempt at this position.
        return block.chars().any(char::is_alphanumeric)
            && block.bytes().filter(|&b| b == b'\n').count() != 1
            && !short_pattern(block.as_bytes());
    }
    false
}

fn short_pattern(block: &[u8]) -> bool {
    // A longer multiple of a repeated short token is still a short pattern.
    (1..MIN_PERIOD).any(|period| {
        block.len().is_multiple_of(period) && block[..block.len() - period] == block[period..]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_eighth_trimmed_line_without_a_final_newline() {
        let mut watchdog = RepetitionWatchdog::default();
        let line = "Repeating this exact line until stopped!";
        for _ in 0..7 {
            assert!(!watchdog.push(&format!("  {line} \r\n")));
        }
        assert!(!watchdog.push("Repeating this exact "));
        assert!(watchdog.push("line until stopped!"));
    }

    #[test]
    fn detects_a_block_split_across_chunks_and_the_end_of_a_large_chunk() {
        let block = "もう一度同じファイルを確認します。";
        let mut watchdog = RepetitionWatchdog::default();
        assert!(!watchdog.push(&"🎨".repeat(WINDOW_BYTES / 4 + 1)));
        for _ in 0..5 {
            assert!(!watchdog.push(block));
        }
        assert!(watchdog.push(&format!("{block}{}", "🎨".repeat(WINDOW_BYTES))));

        let mut watchdog = RepetitionWatchdog::default();
        let final_chunk = format!("{}{}", "🎨".repeat(WINDOW_BYTES), block.repeat(6));
        assert!(watchdog.push(&final_chunk));
    }

    #[test]
    fn normal_code_tables_dividers_and_short_tokens_do_not_trigger() {
        for text in [
            (0..150)
                .map(|i| format!("    let value_{i} = {i};\n"))
                .collect(),
            format!(
                "| Name | Value |\n| :--- | ---: |\n{}",
                "| ok | 1 |\n".repeat(80)
            ),
            "| A long markdown table label that appears more than once | value |\n".repeat(16),
            format!(
                "```rust\n{}```\n",
                "    println!(\"a repeated source statement\");\n".repeat(16)
            ),
            "---".repeat(100),
            "===\n".repeat(100),
            "|:------------------------|------------------------:|\n".repeat(16),
            "ok ".repeat(100),
            format!("{}a", "🎨".repeat(40)),
        ] {
            let mut watchdog = RepetitionWatchdog::default();
            assert!(!watchdog.push(&text));
        }
    }
}
