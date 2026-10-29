//! The config file, `~/.config/smd/config.toml` (or under
//! `$XDG_CONFIG_HOME`). Every setting is optional:
//!
//! ```toml
//! # Opened when smd is run with no file.
//! home = "~/notes/HOME.md"
//! # What following a web link does: "open", "copy" or "ignore".
//! web_links = "open"
//!
//! # Keys for each action. Naming an action replaces its default keys.
//! [keys]
//! next_link = ["tab", "n"]
//! back = ["backspace", "h"]
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// What the reader can do from the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Quit,
    Down,
    Up,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    NextLink,
    PrevLink,
    Follow,
    Back,
}

impl Action {
    /// The keys an action has when the config does not name it.
    fn defaults(self) -> &'static [&'static str] {
        match self {
            Action::Quit => &["q", "esc", "ctrl-c"],
            Action::Down => &["j", "down"],
            Action::Up => &["k", "up"],
            Action::HalfDown => &["ctrl-d"],
            Action::HalfUp => &["ctrl-u"],
            Action::PageDown => &["space", "pagedown", "ctrl-f"],
            Action::PageUp => &["pageup", "ctrl-b"],
            Action::Top => &["g", "home"],
            Action::Bottom => &["G", "end"],
            Action::NextLink => &["tab"],
            Action::PrevLink => &["shift-tab"],
            Action::Follow => &["enter"],
            Action::Back => &["backspace"],
        }
    }

    const ALL: [Action; 13] = [
        Action::Quit,
        Action::Down,
        Action::Up,
        Action::HalfDown,
        Action::HalfUp,
        Action::PageDown,
        Action::PageUp,
        Action::Top,
        Action::Bottom,
        Action::NextLink,
        Action::PrevLink,
        Action::Follow,
        Action::Back,
    ];
}

/// What following a web link does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebLinks {
    /// Open it with the system's opener, `open` or `xdg-open`.
    #[default]
    Open,
    /// Copy it to the clipboard through the terminal (OSC 52).
    Copy,
    /// Do nothing.
    Ignore,
}

/// A key as the config names it: a key code and the modifiers that matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
}

impl Key {
    /// Read a key name: a character (`j`, `G`, `?`), a named key (`enter`,
    /// `tab`, `space`, `pagedown`, `f1`…), either after `ctrl-` or `alt-`,
    /// and `shift-tab`.
    fn parse(name: &str) -> Result<Key, String> {
        let mut rest = name;
        let (mut ctrl, mut alt) = (false, false);
        loop {
            if let Some(r) = rest.strip_prefix("ctrl-") {
                ctrl = true;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("alt-") {
                alt = true;
                rest = r;
            } else {
                break;
            }
        }
        let mut chars = rest.chars();
        let code = match (chars.next(), chars.next()) {
            (Some(c), None) => KeyCode::Char(c),
            _ => match rest.to_ascii_lowercase().as_str() {
                "enter" | "return" => KeyCode::Enter,
                "tab" => KeyCode::Tab,
                "shift-tab" | "backtab" => KeyCode::BackTab,
                "backspace" => KeyCode::Backspace,
                "esc" | "escape" => KeyCode::Esc,
                "space" => KeyCode::Char(' '),
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" => KeyCode::PageUp,
                "pagedown" => KeyCode::PageDown,
                "delete" => KeyCode::Delete,
                "insert" => KeyCode::Insert,
                f => match f.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                    Some(n @ 1..=24) => KeyCode::F(n),
                    _ => return Err(format!("unknown key \"{name}\"")),
                },
            },
        };
        // Control letters arrive in lower case.
        let code = match code {
            KeyCode::Char(c) if ctrl => KeyCode::Char(c.to_ascii_lowercase()),
            code => code,
        };
        Ok(Key { code, ctrl, alt })
    }

    /// The key a key press is. Shift is part of the character it makes, so
    /// it is not kept.
    fn of(event: KeyEvent) -> Key {
        Key {
            code: event.code,
            ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
            alt: event.modifiers.contains(KeyModifiers::ALT),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    home: Option<String>,
    #[serde(default)]
    web_links: WebLinks,
    #[serde(default)]
    keys: HashMap<Action, Vec<String>>,
}

#[derive(Debug)]
pub struct Config {
    /// The note to open when smd is given no file.
    pub home: Option<PathBuf>,
    pub web_links: WebLinks,
    keys: HashMap<Key, Action>,
}

impl Default for Config {
    fn default() -> Config {
        Config::from_str("").expect("the defaults are valid")
    }
}

impl Config {
    /// Read the config file. A missing file gives the defaults; one that
    /// cannot be read or has a mistake in it is an error that names it.
    pub fn load() -> Result<Config, String> {
        let Some(path) = path() else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    fn from_str(text: &str) -> Result<Config, String> {
        let file: File = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let mut keys = HashMap::new();
        for action in Action::ALL {
            let names: Vec<&str> = match file.keys.get(&action) {
                Some(names) => names.iter().map(String::as_str).collect(),
                None => action.defaults().to_vec(),
            };
            for name in names {
                let key = Key::parse(name)?;
                if let Some(other) = keys.insert(key, action)
                    && other != action
                {
                    return Err(format!(
                        "key \"{name}\" is set for both {other:?} and {action:?}"
                    ));
                }
            }
        }
        Ok(Config {
            home: file.home.as_deref().map(expand_home),
            web_links: file.web_links,
            keys,
        })
    }

    /// The action a key press is bound to.
    pub fn action(&self, event: KeyEvent) -> Option<Action> {
        self.keys.get(&Key::of(event)).copied()
    }
}

/// Where the config file is.
fn path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("smd").join("config.toml"))
}

/// `path` with a leading `~/` made the home folder.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn the_defaults_are_the_usual_keys() {
        let c = Config::default();
        let none = KeyModifiers::NONE;
        assert_eq!(
            c.action(press(KeyCode::Char('j'), none)),
            Some(Action::Down)
        );
        assert_eq!(
            c.action(press(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some(Action::Bottom)
        );
        assert_eq!(
            c.action(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            c.action(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Action::PrevLink)
        );
        assert_eq!(c.action(press(KeyCode::Enter, none)), Some(Action::Follow));
        assert_eq!(c.action(press(KeyCode::Char('x'), none)), None);
        assert_eq!(c.web_links, WebLinks::Open);
        assert_eq!(c.home, None);
    }

    #[test]
    fn naming_an_action_replaces_its_keys() {
        let c = Config::from_str(
            "home = \"/n/HOME.md\"\nweb_links = \"copy\"\n[keys]\nback = [\"h\", \"ctrl-o\"]\n",
        )
        .unwrap();
        let none = KeyModifiers::NONE;
        assert_eq!(
            c.action(press(KeyCode::Char('h'), none)),
            Some(Action::Back)
        );
        assert_eq!(
            c.action(press(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(Action::Back)
        );
        assert_eq!(c.action(press(KeyCode::Backspace, none)), None);
        assert_eq!(c.web_links, WebLinks::Copy);
        assert_eq!(c.home, Some(PathBuf::from("/n/HOME.md")));
    }

    #[test]
    fn mistakes_are_errors() {
        for (text, wanted) in [
            ("[keys]\nback = [\"hyper-x\"]\n", "unknown key"),
            ("[keys]\nback = [\"j\"]\n", "both"),
            ("[keys]\nfly = [\"x\"]\n", "fly"),
            ("colour = 1\n", "colour"),
            ("web_links = \"shout\"\n", "shout"),
        ] {
            let err = Config::from_str(text).unwrap_err();
            assert!(err.contains(wanted), "{text}: {err}");
        }
    }
}
