const WINDOW_BYTES: usize = 8 * 1024;
const MIN_PERIOD: usize = 32;
const MAX_PERIOD: usize = 512;
const BLOCK_REPEATS: usize = 5;

#[derive(Default)]
pub(crate) struct RepetitionWatchdog {
    window: String,
}

impl RepetitionWatchdog {
    pub(crate) fn push(&mut self, mut text: &str) -> bool {
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
                if (ch == '\n' && repeated_line(&tail[..tail.len() - 1]))
                    || repeated_block(tail, ch)
                {
                    return true;
                }
            }
            let keep = self.window.len().saturating_sub(WINDOW_BYTES);
            self.window.drain(..self.window.ceil_char_boundary(keep));
            text = rest;
        }
        !self.window.ends_with('\n') && repeated_line(&self.window)
    }
}

fn repeated_line(tail: &str) -> bool {
    let mut lines = tail.rsplit('\n').map(str::trim);
    let line = lines.next().unwrap_or_default();
    line.chars().take(24).count() == 24
        && line.chars().any(char::is_alphanumeric)
        && (0..5).all(|_| lines.next() == Some(line))
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
        if block.chars().any(char::is_alphanumeric) && !short_pattern(block.as_bytes()) {
            return true;
        }
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
    fn detects_the_sixth_trimmed_line_without_a_final_newline() {
        let mut watchdog = RepetitionWatchdog::default();
        let line = "Repeating this exact line!";
        for _ in 0..5 {
            assert!(!watchdog.push(&format!("  {line} \r\n")));
        }
        assert!(!watchdog.push("Repeating this exact "));
        assert!(watchdog.push("line!"));
    }

    #[test]
    fn detects_a_block_split_across_chunks_and_the_end_of_a_large_chunk() {
        let block = "もう一度同じファイルを確認します。";
        let mut watchdog = RepetitionWatchdog::default();
        assert!(!watchdog.push(&"🎨".repeat(WINDOW_BYTES / 4 + 1)));
        for _ in 0..4 {
            assert!(!watchdog.push(block));
        }
        assert!(watchdog.push(&format!("{block}{}", "🎨".repeat(WINDOW_BYTES))));

        let mut watchdog = RepetitionWatchdog::default();
        let final_chunk = format!("{}{}", "🎨".repeat(WINDOW_BYTES), block.repeat(5));
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
