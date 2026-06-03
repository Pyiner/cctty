pub(super) fn compact_tty_output(output: &str) -> String {
    output.split_whitespace().collect()
}

pub(super) fn plain_tty_output(output: &str) -> String {
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
    let mut line = Vec::<char>::new();
    let mut column = 0_usize;
    let mut lines = Vec::<String>::new();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            apply_ansi_sequence_to_line(&mut chars, &mut line, &mut column);
        } else if ch == '\r' {
            column = 0;
        } else if ch == '\r' || ch == '\n' {
            push_visible_line(&mut lines, &mut line, collapse_spacing);
            column = 0;
        } else if ch.is_control() {
            write_visible_char(&mut line, &mut column, ' ');
        } else {
            write_visible_char(&mut line, &mut column, ch);
        }
    }
    push_visible_line(&mut lines, &mut line, collapse_spacing);
    lines
}

fn push_visible_line(lines: &mut Vec<String>, line: &mut Vec<char>, collapse_spacing: bool) {
    let raw = line.iter().collect::<String>();
    let rendered = if collapse_spacing {
        normalize_visible_line(&raw)
    } else {
        raw.trim().to_owned()
    };
    if !rendered.is_empty() {
        lines.push(rendered);
    }
    line.clear();
}

fn normalize_visible_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn write_visible_char(line: &mut Vec<char>, column: &mut usize, ch: char) {
    if *column >= line.len() {
        line.resize(*column + 1, ' ');
    }
    line[*column] = ch;
    *column += 1;
}

fn apply_ansi_sequence_to_line<I>(
    chars: &mut std::iter::Peekable<I>,
    line: &mut Vec<char>,
    column: &mut usize,
) where
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
            apply_csi_sequence_to_line(&sequence, line, column);
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

fn apply_csi_sequence_to_line(sequence: &str, line: &mut Vec<char>, column: &mut usize) {
    let Some(command) = sequence.chars().last() else {
        return;
    };
    let amount = sequence
        .trim_end_matches(command)
        .split(';')
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    match command {
        'G' | '`' => {
            *column = amount.saturating_sub(1);
        }
        'C' => {
            *column += amount;
        }
        'D' => {
            *column = column.saturating_sub(amount);
        }
        'K' => {
            line.truncate((*column).min(line.len()));
        }
        _ => {}
    }
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
