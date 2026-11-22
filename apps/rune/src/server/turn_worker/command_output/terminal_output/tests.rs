use super::*;

fn feed(chunks: &[&str], limit: usize) -> TerminalOutput {
    let mut output = TerminalOutput::default();
    for chunk in chunks {
        output.append_with_limit(chunk, limit);
    }
    output
}

#[test]
fn rewrites_display_columns_and_normalizes_styled_wide_characters() {
    for (input, expected) in [
        ("😀\x1b[1Gx", "x "),
        ("界a\x1b[3GZ", "界Z"),
        ("e\u{0301}a\x1b[2GZ", "e\u{0301}Z"),
        ("𐐀a\x1b[2GZ", "𐐀Z"),
        ("界a\x08Z", "界Z"),
        ("界a\x1b[2GZ", " Za"),
        ("a界b\rXY", "XY b"),
        ("e\u{0301}a\rZ", "Za"),
        ("界a\x1b[2G\x1b[K", " "),
        ("界a\x1b[1G\x1b[1K", "  a"),
        (
            "\x1b[31m界\x1b[32ma\x1b[3GZ",
            "\x1b[0;31m界\x1b[0;32mZ\x1b[0m",
        ),
        ("e\x1b[31m\u{0301}a\x1b[1GZ", "\x1b[0;31mZa\x1b[0m"),
    ] {
        assert_eq!(feed(&[input], TAIL_CHARS).text(), expected);
        for (cut, _) in input.char_indices() {
            let (prefix, rest) = input.split_at(cut);
            let resume = feed(&[prefix], TAIL_CHARS).resume_text();
            assert_eq!(feed(&[prefix, rest], TAIL_CHARS).text(), expected);
            assert_eq!(feed(&[&resume, rest], TAIL_CHARS).text(), expected);
        }
    }
}

#[test]
fn protects_visible_text_from_incomplete_and_unsupported_controls() {
    let mut output = TerminalOutput::default();
    output.append("red: \x1b[3");
    assert_eq!(output.text(), "red: ");
    assert_eq!(output.resume_text(), "red: \x1b[3");
    output.append("1mR\x1b[0m\r");
    output.append("\nnext\x1b]0;");
    output.append(&"z".repeat(200));
    output.append("\x1b");
    output.append("\\ done");
    assert_eq!(output.text(), "red: \x1b[0;31mR\x1b[0m\nnext done");

    for (sequence, expected) in [
        ("\x1b[1.5A", "a\nbX"),
        ("\x1b[3K", "a\nbX"),
        ("\x1b[4J", "a\nbX"),
        ("\x1b[1:2A", "a\nbX"),
        ("\x1b(B", "a\nbX"),
        ("\x1b#\n8", "a\nb\n8X"),
    ] {
        let text = format!("a\nb{sequence}X");
        for (cut, _) in text.char_indices() {
            let (first, last) = text.split_at(cut);
            assert_eq!(feed(&[first, last], TAIL_CHARS).text(), expected);
        }
    }
}

#[test]
fn canonicalizes_extended_colors_and_ignores_unused_underline_colors() {
    for (input, expected) in [
        ("\x1b[1;31m\x1b[4:3mX", "\x1b[0;1;4;31mX\x1b[0m"),
        ("\x1b[1;31m\x1b[4:0mX", "\x1b[0;1;31mX\x1b[0m"),
        ("\x1b[1;31m\x1b[58;2;255;0;0mX", "\x1b[0;1;31mX\x1b[0m"),
        ("\x1b[58;5;31mX", "X"),
        ("\x1b[38:2::12:34:56mX", "\x1b[0;38;2;12;34;56mX\x1b[0m"),
        ("\x1b[38:5:1:4mX", "\x1b[0;38;5;1mX\x1b[0m"),
        ("\x1b[38;2:255:0:0;1mX", "\x1b[0;1;38;2;255;0;0mX\x1b[0m"),
    ] {
        assert_eq!(feed(&[input], TAIL_CHARS).text(), expected);
    }
}

#[test]
fn bounded_tail_and_resume_preserve_saved_cursor_and_pending_style() {
    let source = feed(&["界界e\u{0301}\x1b7ab"], 4);
    assert!(source.truncated());
    assert_eq!(source.text(), "e\u{0301}ab");
    let resumed = feed(&[&source.resume_text(), "\x1b8Z"], 4);
    assert_eq!(resumed.text(), "e\u{0301}Zb");
    assert_eq!(feed(&["𐐀a"], 2).text(), "a");
    assert_eq!(feed(&["e\u{0301}ab"], 3).text(), "ab");
    assert_eq!(feed(&["abcdefgh\nij\nklmnop"], 8).text(), "klmnop");
    assert_eq!(feed(&["abcdefgh\nijk\nlmnop"], 8).text(), "jk\nlmnop");
    let chunks = ["a: 1%\nb: 1%\n", "\x1b[2A\x1b[2K\x1b[33ma: 5", "0%\x1b[0"];
    let mut source = feed(&chunks, 1_024);
    let mut resumed = feed(&[&source.resume_text()], 1_024);
    let rest = "m\n\x1b[2Kb: 50%\n";
    source.append_with_limit(rest, 1_024);
    resumed.append_with_limit(rest, 1_024);
    assert_eq!(source.text(), resumed.text());
    assert_eq!(resumed.text(), "\x1b[0;33ma: 50%\x1b[0m\nb: 50%\n");
}

#[test]
fn clears_scrollback_without_touching_the_addressable_lines() {
    let lines = (0..70)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = feed(&[&lines, "\x1b[3J"], TAIL_CHARS);
    assert!(output.truncated());
    assert!(output.text().starts_with("line 6\n"));
    assert!(output.text().ends_with("line 69"));
}
