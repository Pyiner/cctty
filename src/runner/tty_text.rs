pub(super) fn compact_tty_output(output: &str) -> String {
    output.split_whitespace().collect()
}

pub(crate) fn plain_tty_output(output: &str) -> String {
    let mut plain = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            strip_ansi_sequence(&mut chars);
            plain.push(' ');
        } else if ch.is_control() {
            plain.push(' ');
        } else {
            plain.push(ch);
        }
    }
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn recent_tty_log_text(output: &str, max_chars: usize) -> String {
    plain_tty_output(output)
        .chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
}

pub(super) fn single_line_log_text(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            other if other.is_control() => format!("\\u{{{:x}}}", other as u32).chars().collect(),
            other => vec![other],
        })
        .collect()
}

pub(super) fn visible_tty_lines(output: &str) -> Vec<String> {
    render_visible_tty_lines(output, true)
}

pub(super) fn visible_tty_lines_preserving_spacing(output: &str) -> Vec<String> {
    render_visible_tty_lines(output, false)
}

fn render_visible_tty_lines(output: &str, collapse_spacing: bool) -> Vec<String> {
    let mut chars = output.chars().peekable();
    let mut screen = TerminalScreen::default();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            apply_ansi_sequence_to_screen(&mut chars, &mut screen);
        } else if ch == '\r' {
            screen.carriage_return();
        } else if ch == '\n' {
            screen.newline();
        } else if ch.is_control() {
            screen.write_char(' ');
        } else {
            screen.write_char(ch);
        }
    }
    screen.lines(collapse_spacing)
}

#[derive(Default)]
struct TerminalScreen {
    rows: Vec<Vec<char>>,
    row: usize,
    column: usize,
}

impl TerminalScreen {
    fn ensure_row(&mut self) {
        if self.rows.len() <= self.row {
            self.rows.resize_with(self.row + 1, Vec::new);
        }
    }

    fn write_char(&mut self, ch: char) {
        self.ensure_row();
        let line = &mut self.rows[self.row];
        if self.column >= line.len() {
            line.resize(self.column + 1, ' ');
        }
        line[self.column] = ch;
        self.column += 1;
    }

    fn carriage_return(&mut self) {
        self.column = 0;
    }

    fn newline(&mut self) {
        self.row += 1;
        self.column = 0;
        self.ensure_row();
    }

    fn cursor_up(&mut self, amount: usize) {
        self.row = self.row.saturating_sub(amount.max(1));
    }

    fn cursor_down(&mut self, amount: usize) {
        self.row += amount.max(1);
        self.ensure_row();
    }

    fn cursor_forward(&mut self, amount: usize) {
        self.column += amount.max(1);
    }

    fn cursor_back(&mut self, amount: usize) {
        self.column = self.column.saturating_sub(amount.max(1));
    }

    fn cursor_next_line(&mut self, amount: usize) {
        self.row += amount.max(1);
        self.column = 0;
        self.ensure_row();
    }

    fn cursor_previous_line(&mut self, amount: usize) {
        self.row = self.row.saturating_sub(amount.max(1));
        self.column = 0;
    }

    fn set_column_1_based(&mut self, column: usize) {
        self.column = column.max(1) - 1;
    }

    fn set_position_1_based(&mut self, row: usize, column: usize) {
        self.row = row.max(1) - 1;
        self.column = column.max(1) - 1;
        self.ensure_row();
    }

    fn erase_in_display(&mut self, mode: usize) {
        self.ensure_row();
        match mode {
            0 => {
                if let Some(line) = self.rows.get_mut(self.row) {
                    line.truncate(self.column.min(line.len()));
                }
                self.rows.truncate(self.row + 1);
            }
            1 => {
                for row in 0..self.row {
                    if let Some(line) = self.rows.get_mut(row) {
                        line.clear();
                    }
                }
                if let Some(line) = self.rows.get_mut(self.row) {
                    let end = self.column.saturating_add(1).min(line.len());
                    for ch in line.iter_mut().take(end) {
                        *ch = ' ';
                    }
                }
            }
            2 | 3 => {
                self.rows.clear();
                self.row = 0;
                self.column = 0;
                self.ensure_row();
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: usize) {
        self.ensure_row();
        let line = &mut self.rows[self.row];
        match mode {
            0 => line.truncate(self.column.min(line.len())),
            1 => {
                let end = self.column.saturating_add(1).min(line.len());
                for ch in line.iter_mut().take(end) {
                    *ch = ' ';
                }
            }
            2 => line.clear(),
            _ => {}
        }
    }

    fn lines(&self, collapse_spacing: bool) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|line| {
                let raw = line.iter().collect::<String>();
                let rendered = if collapse_spacing {
                    normalize_visible_line(&raw)
                } else {
                    raw.trim().to_owned()
                };
                (!rendered.is_empty()).then_some(rendered)
            })
            .collect()
    }
}

fn normalize_visible_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn apply_ansi_sequence_to_screen<I>(chars: &mut std::iter::Peekable<I>, screen: &mut TerminalScreen)
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            let mut sequence = String::new();
            for ch in chars.by_ref() {
                sequence.push(ch);
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
            apply_csi_sequence_to_screen(&sequence, screen);
        }
        Some(']') => {
            chars.next();
            for ch in chars.by_ref() {
                if ch == '\u{7}' {
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn apply_csi_sequence_to_screen(sequence: &str, screen: &mut TerminalScreen) {
    let Some(command) = sequence.chars().last() else {
        return;
    };
    let params = csi_params(sequence.trim_end_matches(command));
    let amount = params.first().copied().unwrap_or(1);
    match command {
        'A' => screen.cursor_up(amount),
        'B' => screen.cursor_down(amount),
        'C' => screen.cursor_forward(amount),
        'D' => screen.cursor_back(amount),
        'E' => screen.cursor_next_line(amount),
        'F' => screen.cursor_previous_line(amount),
        'G' | '`' => screen.set_column_1_based(amount),
        'H' | 'f' => screen.set_position_1_based(
            params.first().copied().unwrap_or(1),
            params.get(1).copied().unwrap_or(1),
        ),
        'J' => screen.erase_in_display(params.first().copied().unwrap_or(0)),
        'K' => screen.erase_in_line(params.first().copied().unwrap_or(0)),
        _ => {}
    }
}

fn csi_params(text: &str) -> Vec<usize> {
    let text = text.trim_start_matches('?');
    if text.is_empty() {
        return Vec::new();
    }
    text.split(';')
        .map(|value| value.parse::<usize>().unwrap_or(1))
        .collect()
}

fn strip_ansi_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        Some(']') => {
            chars.next();
            for ch in chars.by_ref() {
                if ch == '\u{7}' {
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_lines_apply_cursor_up_overwrite() {
        let output = "old\nline\u{1b}[1Anew";

        assert_eq!(visible_tty_lines(output), vec!["old new", "line"]);
    }

    #[test]
    fn visible_lines_apply_clear_screen() {
        let output = "old\ntext\u{1b}[2J\u{1b}[Hfresh";

        assert_eq!(visible_tty_lines(output), vec!["fresh"]);
    }

    #[test]
    fn visible_lines_apply_column_positioning() {
        let output = "abcdef\r\u{1b}[4GZ";

        assert_eq!(visible_tty_lines_preserving_spacing(output), vec!["abcZef"]);
    }
}
