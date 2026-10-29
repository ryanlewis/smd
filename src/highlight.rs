//! Colours for code blocks, picked by the fence's language. Tokens get the
//! terminal's own ANSI colours rather than a theme's, so they suit its dark or
//! light background. The syntaxes load on a thread at start-up; until they
//! are ready, code is drawn plain.

use std::sync::{LazyLock, OnceLock};
use std::thread;

use ratatui::style::{Color, Modifier, Style};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

/// Start loading the syntaxes on another thread.
pub fn load_in_background() {
    thread::spawn(syntax_set);
}

/// The syntaxes, once they have loaded.
pub fn ready() -> Option<&'static SyntaxSet> {
    SYNTAXES.get()
}

/// The syntaxes, loading them here if no one has yet.
pub fn syntax_set() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// A line's text in pieces, each with its colour.
pub type Segments = Vec<(String, Style)>;

/// `code` split into lines of coloured pieces for the language `lang`, a
/// name or file extension, or nothing when that language is not known.
/// Diffs need no syntaxes.
pub fn highlight(set: Option<&SyntaxSet>, lang: &str, code: &str) -> Option<Vec<Segments>> {
    let code = code.trim_end_matches('\n');
    // Attributes such as `rust,no_run` follow the language after a comma.
    let lang = lang.split(',').next().unwrap_or(lang);
    if lang.eq_ignore_ascii_case("diff") || lang.eq_ignore_ascii_case("patch") {
        return Some(diff(code));
    }
    let set = set?;
    let syntax = set.find_syntax_by_token(lang)?;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for line in code.split('\n') {
        // The syntaxes expect each line to end with its newline, which the
        // pieces leave out.
        let text = format!("{line}\n");
        let end = line.len();
        let mut segments: Segments = Vec::new();
        let mut piece = |from: usize, to: usize, style| {
            if from.min(end) < to.min(end) {
                segments.push((line[from.min(end)..to.min(end)].to_string(), style));
            }
        };
        match state.parse_line(&text, set) {
            Ok(ops) => {
                let mut at = 0;
                for (i, op) in ops {
                    piece(at, i, style(&stack));
                    at = at.max(i);
                    if stack.apply(&op).is_err() {
                        break;
                    }
                }
                piece(at, end, style(&stack));
            }
            Err(_) => piece(0, end, Style::new()),
        }
        out.push(segments);
    }
    Some(out)
}

/// A diff's lines coloured: added green, removed red, hunk headers cyan and
/// file headers bold. Inside a hunk, as long as its header says, a line is
/// added or removed by its first character even when it looks like a header.
fn diff(code: &str) -> Vec<Segments> {
    let (added, removed) = (Style::new().fg(Color::Green), Style::new().fg(Color::Red));
    // Lines left in the current hunk, of the old file and the new.
    let (mut old, mut new) = (0usize, 0usize);
    let mut style = |line: &str| {
        if old + new > 0 {
            return match line.chars().next() {
                Some('+') => {
                    new = new.saturating_sub(1);
                    added
                }
                Some('-') => {
                    old = old.saturating_sub(1);
                    removed
                }
                // A note such as "\ No newline at end of file".
                Some('\\') => Style::new(),
                _ => {
                    old = old.saturating_sub(1);
                    new = new.saturating_sub(1);
                    Style::new()
                }
            };
        }
        let headers = ["diff ", "index ", "--- ", "+++ "];
        if line.starts_with("@@") {
            (old, new) = hunk_lengths(line);
            Style::new().fg(Color::Cyan)
        } else if headers.iter().any(|h| line.starts_with(h)) {
            Style::new().add_modifier(Modifier::BOLD)
        } else if line.starts_with('+') {
            added
        } else if line.starts_with('-') {
            removed
        } else {
            Style::new()
        }
    };
    let lines = code.split('\n');
    lines.map(|l| vec![(l.to_string(), style(l))]).collect()
}

/// How many lines of the old and new file a hunk header such as
/// `@@ -1,3 +1,4 @@` says follow, or none when it cannot be read.
fn hunk_lengths(header: &str) -> (usize, usize) {
    let length = |range: Option<&str>, sign: char| -> Option<usize> {
        let range = range?.strip_prefix(sign)?;
        match range.split_once(',') {
            Some((_, n)) => n.parse().ok(),
            None => range.parse::<usize>().ok().map(|_| 1),
        }
    };
    let mut words = header.split_whitespace().skip(1);
    match (length(words.next(), '-'), length(words.next(), '+')) {
        (Some(old), Some(new)) => (old, new),
        _ => (0, 0),
    }
}

/// Scopes and their colours. A token takes the colour of its innermost scope
/// that one of these starts, so an earlier, narrower entry wins over a later,
/// wider one. Anything else is the terminal's own colour.
static COLOURS: LazyLock<Vec<(Scope, Style)>> = LazyLock::new(|| {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let fg = |c| Style::new().fg(c);
    [
        ("comment", dim),
        ("string", fg(Color::Green)),
        ("constant", fg(Color::Cyan)),
        ("keyword.operator", Style::new()),
        ("keyword", fg(Color::Magenta)),
        ("storage", fg(Color::Magenta)),
        ("entity.name.function", fg(Color::Blue)),
        ("support.function", fg(Color::Blue)),
        ("variable.function", fg(Color::Blue)),
        ("entity.name.type", fg(Color::Yellow)),
        ("entity.name.class", fg(Color::Yellow)),
        ("entity.name.struct", fg(Color::Yellow)),
        ("entity.name.enum", fg(Color::Yellow)),
        ("entity.name.trait", fg(Color::Yellow)),
        ("entity.name.interface", fg(Color::Yellow)),
        ("support.type", fg(Color::Yellow)),
        ("support.class", fg(Color::Yellow)),
    ]
    .into_iter()
    .filter_map(|(s, style)| Some((Scope::new(s).ok()?, style)))
    .collect()
});

fn style(stack: &ScopeStack) -> Style {
    for scope in stack.as_slice().iter().rev() {
        if let Some((_, style)) = COLOURS.iter().find(|(s, _)| s.is_prefix_of(*scope)) {
            return *style;
        }
    }
    Style::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_keep_no_newline() {
        let lines = highlight(Some(syntax_set()), "rust", "// note\nlet x = 1;\n").unwrap();
        let text: Vec<&str> = lines.iter().flatten().map(|(s, _)| s.as_str()).collect();
        assert!(text.iter().all(|s| !s.contains('\n')), "{text:?}");
    }

    #[test]
    fn hunk_lines_that_look_like_headers_keep_their_colour() {
        let code = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n--- old note\n+++ x\n";
        let lines = highlight(None, "diff", code).unwrap();
        let fg: Vec<_> = lines.iter().map(|l| l[0].1.fg).collect();
        assert_eq!(
            fg,
            [
                None,
                None,
                Some(Color::Cyan),
                Some(Color::Red),
                Some(Color::Green)
            ]
        );
    }

    #[test]
    fn fences_with_attributes_are_coloured_as_their_language() {
        for md in [
            "```rust,no_run\nfn main() {}\n```\n",
            "```rust,ignore\nfn main() {}\n```\n",
        ] {
            let doc = crate::doc::parse(md);
            let crate::doc::BlockKind::Code {
                lang: Some(lang),
                code,
            } = &doc.blocks[0].kind
            else {
                panic!("expected code, got {:?}", doc.blocks);
            };
            let lines = highlight(Some(syntax_set()), lang, code).unwrap();
            assert_eq!(
                lines[0][0],
                ("fn".to_string(), Style::new().fg(Color::Magenta)),
                "{md:?}"
            );
        }
    }

    #[test]
    fn attributes_after_a_comma_are_not_part_of_the_language() {
        let lines = highlight(Some(syntax_set()), "rust,no_run", "fn main() {}").unwrap();
        assert_eq!(
            lines[0][0],
            ("fn".to_string(), Style::new().fg(Color::Magenta))
        );
    }
}
