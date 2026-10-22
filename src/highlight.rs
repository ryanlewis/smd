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
    if lang.eq_ignore_ascii_case("diff") || lang.eq_ignore_ascii_case("patch") {
        let lines = code.split('\n');
        return Some(lines.map(|l| vec![(l.to_string(), diff(l))]).collect());
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

/// The colour for a diff line: added green, removed red, hunk headers cyan
/// and file headers bold.
fn diff(line: &str) -> Style {
    let headers = ["diff ", "index ", "--- ", "+++ "];
    if headers.iter().any(|h| line.starts_with(h)) {
        Style::new().add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::new().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::new().fg(Color::Red)
    } else {
        Style::new()
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
}
