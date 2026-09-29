//! Text that leaves the node as text (spec §7.1): what a script prints or a log holds may carry terminal control
//! sequences —colours, a window's title— that mean nothing to the model and can rewrite what a client's terminal shows.

use std::iter::Peekable;
use std::ops::RangeInclusive;
use std::str::Chars;

const ESCAPE: char = '\u{1b}';
const BELL: char = '\u{7}';

/// [text] without escape sequences (`ESC [ … final` and the strings `ESC ] … BEL`, `ESC ] … ESC \`), and without any
/// other control character but the newline, the tab and the carriage return. Redaction and filters work on this, so
/// a sequence can't split a secret's name or hide a line from `grep`.
pub fn printable(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            ESCAPE => skip_sequence(&mut chars),
            '\n' | '\t' | '\r' => kept.push(character),
            other if other.is_control() => {}
            other => kept.push(other),
        }
    }
    kept
}

/// What follows an `ESC` already read. A sequence cut short by the end of the text, or by a character that can't be
/// part of it, ends there: that character is text, and a newline is never swallowed.
fn skip_sequence(chars: &mut Peekable<Chars>) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            skip_while_in(chars, ' '..='?');
            chars.next_if(|next| ('@'..='~').contains(next));
        }
        Some(']' | 'P' | 'X' | '^' | '_') => {
            chars.next();
            skip_string(chars);
        }
        _ => {
            skip_while_in(chars, ' '..='/');
            chars.next_if(|next| ('0'..='~').contains(next));
        }
    }
}

fn skip_while_in(chars: &mut Peekable<Chars>, range: RangeInclusive<char>) {
    while chars.next_if(|next| range.contains(next)).is_some() {}
}

/// The body of an OSC (or DCS, SOS, PM, APC) string, up to its terminator: `BEL` or `ESC \`. Another `ESC` starts a
/// sequence of its own, and a string still open at the end of the line ends there.
fn skip_string(chars: &mut Peekable<Chars>) {
    while let Some(&next) = chars.peek() {
        match next {
            BELL => {
                chars.next();
                return;
            }
            ESCAPE => {
                let mut ahead = chars.clone();
                ahead.next();
                if ahead.next() == Some('\\') {
                    chars.next();
                    chars.next();
                }
                return;
            }
            '\n' => return,
            _ => {
                chars.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_cursor_movements_go() {
        assert_eq!(printable("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(printable("\x1b[1;38;5;208mwarm\x1b[K\x1b[?25l done"), "warm done");
        assert_eq!(printable("a\x1b[2Ab\x1b[10;20Hc"), "abc");
    }

    #[test]
    fn a_title_or_a_link_goes_with_its_terminator() {
        assert_eq!(printable("a\x1b]0;my title\x07b"), "ab");
        assert_eq!(printable("a\x1b]2;my title\x1b\\b"), "ab");
        assert_eq!(printable("\x1b]8;;http://example.com\x07link\x1b]8;;\x07"), "link");
        assert_eq!(printable("a\x1bPq#0;2;0;0;0\x1b\\b"), "ab");
    }

    #[test]
    fn a_sequence_left_open_ends_where_the_text_does_or_at_the_line() {
        assert_eq!(printable("text\x1b"), "text");
        assert_eq!(printable("text\x1b["), "text");
        assert_eq!(printable("text\x1b[31"), "text");
        assert_eq!(printable("text\x1b]0;never closed"), "text");
        assert_eq!(printable("first\x1b]0;never closed\nsecond"), "first\nsecond");
        assert_eq!(printable("first\x1b[\nsecond"), "first\nsecond");
        assert_eq!(printable("first\x1b\nsecond"), "first\nsecond");
    }

    #[test]
    fn an_escape_inside_a_sequence_starts_another() {
        assert_eq!(printable("\x1b]0;title\x1b[31mred"), "red");
        assert_eq!(printable("\x1b\x1b[31mred"), "red");
        assert_eq!(printable("\x1b]0;title\x1b]0;other\x07after"), "after");
    }

    #[test]
    fn other_escapes_go_with_their_final_character() {
        assert_eq!(printable("\x1b(Bplain"), "plain");
        assert_eq!(printable("\x1bcreset"), "reset");
        assert_eq!(printable("\x1b7saved\x1b8"), "saved");
    }

    #[test]
    fn a_wall_of_open_strings_takes_no_stack() {
        assert_eq!(printable(&"\x1b]".repeat(200_000)), "");
        assert_eq!(printable(&format!("{}\nx", "\x1b[".repeat(200_000))), "\nx");
    }

    #[test]
    fn other_control_characters_go() {
        assert_eq!(printable("a\0b\x07c\x08d\x0Be\x0Cf\x7Fg"), "abcdefg");
        assert_eq!(printable("a\u{85}b\u{9b}31mc"), "ab31mc");
    }

    #[test]
    fn newline_tab_and_carriage_return_stay() {
        assert_eq!(printable("a\tb\r\nc\n"), "a\tb\r\nc\n");
        assert_eq!(printable("\x1b[32mok\x1b[0m\n\tnext"), "ok\n\tnext");
    }

    #[test]
    fn text_is_left_alone() {
        let text = "café ñandú 日本語 🙂 — “quotes” [not a sequence] ]0;x \\";
        assert_eq!(printable(text), text);
        assert_eq!(printable(""), "");
    }
}
