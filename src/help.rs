//! The help page: a note smd makes itself, with a table of the keys as
//! they are bound now.

use std::path::Path;

use crate::config::{self, Action, Config};

/// The help page's text, with `{keys}` and `{config}` to fill in.
const PAGE: &str = include_str!("help.md");

/// The help page for `config`, as markdown.
pub fn page(config: &Config) -> String {
    let where_ = match config::path() {
        Some(path) => config_line(&path),
        None => "smd found no home folder, so it reads no config file.".into(),
    };
    PAGE.replace("{keys}", &table(config))
        .replace("{config}", &where_)
}

fn config_line(path: &Path) -> String {
    let state = if path.is_file() {
        "it is there"
    } else {
        "there is none yet, so smd uses the defaults"
    };
    format!(
        "smd reads its config from {}; {state}.",
        code(&path.display().to_string())
    )
}

/// The table of actions and the keys bound to them.
fn table(config: &Config) -> String {
    let mut out = String::from("| Keys | Action | What it does |\n| --- | --- | --- |\n");
    for action in Action::ALL {
        let names = config.key_names(action);
        let keys = if names.is_empty() {
            "none".to_string()
        } else {
            names.iter().map(|n| code(n)).collect::<Vec<_>>().join(", ")
        };
        out.push_str(&format!(
            "| {keys} | {} | {} |\n",
            code(&name(action)),
            what(action)
        ));
    }
    out
}

/// `text` as inline code, with a pipe escaped so it stays in its cell.
fn code(text: &str) -> String {
    let text = text.replace('|', "\\|");
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

/// The action's name as the config's `[keys]` writes it.
pub fn name(action: Action) -> String {
    let mut out = String::new();
    for (i, c) in format!("{action:?}").chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// What an action does, in a few words.
fn what(action: Action) -> &'static str {
    match action {
        Action::Quit => "Quit",
        Action::Down => "Scroll down a line",
        Action::Up => "Scroll up a line",
        Action::HalfDown => "Scroll down half a page",
        Action::HalfUp => "Scroll up half a page",
        Action::PageDown => "Scroll down a page",
        Action::PageUp => "Scroll up a page",
        Action::Top => "Go to the top, or with a number to that line",
        Action::Bottom => "Go to the bottom, or with a number to that line",
        Action::NextLink => "Select the next link, checkbox or folding title",
        Action::PrevLink => "Select the previous link, checkbox or folding title",
        Action::Follow => "Follow the selected link, tick the checkbox or fold the title",
        Action::Toggle => "Tick or untick the selected checkbox",
        Action::Back => "Go back",
        Action::Forward => "Go forward",
        Action::Search => "Search the note",
        Action::NextMatch => "Go to the next match",
        Action::PrevMatch => "Go to the previous match",
        Action::Reload => "Reload the file",
        Action::Help => "Show this page",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_is_in_the_table_with_its_keys() {
        let page = page(&Config::default());
        assert!(!page.contains("{keys}") && !page.contains("{config}"));
        for action in Action::ALL {
            let row = format!("| {} | {} |", code(&name(action)), what(action));
            assert!(page.contains(&row), "the help has no row for {action:?}");
        }
        assert!(page.contains("| `?` | `help` | Show this page |"));
        assert!(page.contains("| `q`, `esc`, `ctrl-c` | `quit` |"));
    }

    #[test]
    fn names_are_as_the_config_writes_them() {
        assert_eq!(name(Action::Quit), "quit");
        assert_eq!(name(Action::HalfDown), "half_down");
        assert_eq!(name(Action::NextMatch), "next_match");
    }

    #[test]
    fn a_pipe_or_backtick_key_stays_in_its_cell() {
        assert_eq!(code("|"), "`\\|`");
        assert_eq!(code("`"), "`` ` ``");
    }
}
