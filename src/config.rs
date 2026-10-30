//! The config file, `~/.config/smd/config.toml` (or under
//! `$XDG_CONFIG_HOME`). Every setting is optional:
//!
//! ```toml
//! # Opened when smd is run with no file.
//! home = "~/notes/HOME.md"
//! # What following a web link does: "open", "copy" or "ignore".
//! web_links = "open"
//! # The colour code blocks are drawn on when the terminal does not say
//! # what its background is.
//! code_background = "#20242a"
//! # Whether smd takes the mouse, to follow clicked links and scroll with
//! # the wheel. Without it, links are hyperlinks the terminal opens.
//! mouse = true
//!
//! # Keys for each action. Naming an action replaces its default keys.
//! [keys]
//! next_link = ["tab", "n"]
//! back = ["backspace", "h"]
//! ```

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

use crate::mermaid::Renderer;
use crate::theme::Rgb;

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
    Forward,
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
            Action::Back => &["backspace", "H", "alt-left"],
            Action::Forward => &["L", "alt-right"],
        }
    }

    /// Every default key name, for checking the help lists them.
    #[cfg(test)]
    pub fn all_default_keys() -> Vec<&'static str> {
        Action::ALL
            .into_iter()
            .flat_map(|a| a.defaults().iter().copied())
            .collect()
    }

    const ALL: [Action; 14] = [
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
        Action::Forward,
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

/// The named keys, as the config spells them in lower case.
const NAMED_KEYS: &[(&str, KeyCode)] = &[
    ("enter", KeyCode::Enter),
    ("return", KeyCode::Enter),
    ("tab", KeyCode::Tab),
    ("shift-tab", KeyCode::BackTab),
    ("backtab", KeyCode::BackTab),
    ("backspace", KeyCode::Backspace),
    ("esc", KeyCode::Esc),
    ("escape", KeyCode::Esc),
    ("space", KeyCode::Char(' ')),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
    ("delete", KeyCode::Delete),
    ("insert", KeyCode::Insert),
];

/// Whether a key name starts with `ctrl-` and `alt-`, in any order, and the
/// rest of it.
fn modifiers(name: &str) -> (bool, bool, &str) {
    let (mut ctrl, mut alt, mut rest) = (false, false, name);
    loop {
        if let Some(r) = rest.strip_prefix("ctrl-") {
            ctrl = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("alt-") {
            alt = true;
            rest = r;
        } else {
            return (ctrl, alt, rest);
        }
    }
}

/// The key code a name without modifiers stands for: a character, a named
/// key in any case, or `f1` to `f24`.
fn key_code(name: &str) -> Option<KeyCode> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyCode::Char(c));
    }
    let lower = name.to_ascii_lowercase();
    if let Some(&(_, code)) = NAMED_KEYS.iter().find(|(n, _)| *n == lower) {
        return Some(code);
    }
    match lower.strip_prefix('f')?.parse::<u8>().ok()? {
        n @ 1..=24 => Some(KeyCode::F(n)),
        _ => None,
    }
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
        let (ctrl, alt, rest) = modifiers(name);
        let code = key_code(rest).ok_or_else(|| format!("unknown key \"{name}\""))?;
        Ok(Key::new(code, ctrl, alt))
    }

    /// The key a key press is. Shift is part of the character it makes, so
    /// it is not kept.
    fn of(event: KeyEvent) -> Key {
        Key::new(
            event.code,
            event.modifiers.contains(KeyModifiers::CONTROL),
            event.modifiers.contains(KeyModifiers::ALT),
        )
    }

    /// A key, with a control letter in lower case, so a config's `ctrl-C`
    /// and a terminal's ctrl-c sent as `c` or `C` are all the same key.
    fn new(code: KeyCode, ctrl: bool, alt: bool) -> Key {
        let code = match code {
            KeyCode::Char(c) if ctrl => KeyCode::Char(c.to_ascii_lowercase()),
            code => code,
        };
        Key { code, ctrl, alt }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    home: Option<String>,
    #[serde(default)]
    web_links: WebLinks,
    code_background: Option<String>,
    mouse: Option<bool>,
    #[serde(default)]
    mermaid: Renderer,
    #[serde(default)]
    keys: HashMap<Action, Vec<String>>,
}

#[derive(Debug)]
pub struct Config {
    /// The note to open when smd is given no file.
    pub home: Option<PathBuf>,
    pub web_links: WebLinks,
    /// The colour to draw code blocks on when the terminal does not say
    /// what its background is.
    pub code_background: Option<Rgb>,
    /// Whether smd takes the mouse, to follow clicked links and scroll.
    pub mouse: bool,
    /// What draws mermaid diagrams.
    pub mermaid: Renderer,
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
        Config::load_from(path(), |p| std::fs::read_to_string(p))
    }

    /// `load` for a given path and a way to read it.
    fn load_from(
        path: Option<PathBuf>,
        read: impl FnOnce(&Path) -> std::io::Result<String>,
    ) -> Result<Config, String> {
        let Some(path) = path else {
            return Ok(Config::default());
        };
        match read(&path) {
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
        let code_background = match file.code_background.as_deref() {
            Some(colour) => Some(parse_colour(colour).ok_or_else(|| {
                format!("code_background \"{colour}\" is not a colour like \"#20242a\"")
            })?),
            None => None,
        };
        Ok(Config {
            home: file.home.as_deref().map(expand_home),
            web_links: file.web_links,
            code_background,
            mouse: file.mouse.unwrap_or(true),
            mermaid: file.mermaid,
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
    path_in(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// `path` for given values of `$XDG_CONFIG_HOME` and `$HOME`.
fn path_in(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg_config_home {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(home?).join(".config"),
    };
    Some(base.join("smd").join("config.toml"))
}

/// A colour written as `#rrggbb`.
fn parse_colour(s: &str) -> Option<Rgb> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let part = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((part(0)?, part(2)?, part(4)?))
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
        assert_eq!(c.mermaid, Renderer::Builtin);
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
        assert_eq!(c.code_background, None);
        assert!(c.mouse);
    }

    #[test]
    fn a_control_letter_matches_in_either_case() {
        let c = Config::default();
        let ctrl = KeyModifiers::CONTROL;
        let shift = KeyModifiers::SHIFT;
        // Some terminals send ctrl-c, and ctrl-shift-c, as an upper-case C.
        assert_eq!(
            c.action(press(KeyCode::Char('C'), ctrl)),
            Some(Action::Quit)
        );
        assert_eq!(
            c.action(press(KeyCode::Char('C'), ctrl | shift)),
            Some(Action::Quit)
        );
        // Without control, the case is the key.
        assert_eq!(
            c.action(press(KeyCode::Char('G'), shift)),
            Some(Action::Bottom)
        );
        assert_eq!(
            c.action(press(KeyCode::Char('g'), KeyModifiers::NONE)),
            Some(Action::Top)
        );
        assert_eq!(c.action(press(KeyCode::Char('G'), KeyModifiers::ALT)), None);
        // With alt as well.
        let c = Config::from_str("[keys]\nback = [\"ctrl-alt-x\"]\n").unwrap();
        let alt = KeyModifiers::ALT;
        assert_eq!(
            c.action(press(KeyCode::Char('X'), ctrl | alt)),
            Some(Action::Back)
        );
        assert_eq!(
            c.action(press(KeyCode::Char('x'), ctrl | alt)),
            Some(Action::Back)
        );
        assert_eq!(c.action(press(KeyCode::Char('X'), alt)), None);
    }

    #[test]
    fn naming_an_action_replaces_its_keys() {
        let c = Config::from_str(
            "home = \"/n/HOME.md\"\nweb_links = \"copy\"\ncode_background = \"#20242A\"\nmouse = false\nmermaid = \"mmdc\"\n[keys]\nback = [\"h\", \"ctrl-o\"]\n",
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
        assert_eq!(c.code_background, Some((0x20, 0x24, 0x2a)));
        assert!(!c.mouse);
        assert_eq!(c.mermaid, Renderer::Mmdc);
    }

    #[test]
    fn mistakes_are_errors() {
        for (text, wanted) in [
            ("[keys]\nback = [\"hyper-x\"]\n", "unknown key"),
            ("[keys]\nback = [\"j\"]\n", "both"),
            ("[keys]\nfly = [\"x\"]\n", "fly"),
            ("colour = 1\n", "colour"),
            ("web_links = \"shout\"\n", "shout"),
            ("mermaid = \"chrome\"\n", "chrome"),
            ("code_background = \"grey\"\n", "grey"),
            ("code_background = \"#12345\"\n", "#12345"),
        ] {
            let err = Config::from_str(text).unwrap_err();
            assert!(err.contains(wanted), "{text}: {err}");
        }
    }

    fn key(code: KeyCode, ctrl: bool, alt: bool) -> Result<Key, String> {
        Ok(Key { code, ctrl, alt })
    }

    #[test]
    fn every_key_name_is_read() {
        for (name, code) in [
            ("enter", KeyCode::Enter),
            ("return", KeyCode::Enter),
            ("tab", KeyCode::Tab),
            ("shift-tab", KeyCode::BackTab),
            ("backtab", KeyCode::BackTab),
            ("backspace", KeyCode::Backspace),
            ("esc", KeyCode::Esc),
            ("escape", KeyCode::Esc),
            ("space", KeyCode::Char(' ')),
            ("up", KeyCode::Up),
            ("down", KeyCode::Down),
            ("left", KeyCode::Left),
            ("right", KeyCode::Right),
            ("home", KeyCode::Home),
            ("end", KeyCode::End),
            ("pageup", KeyCode::PageUp),
            ("pagedown", KeyCode::PageDown),
            ("delete", KeyCode::Delete),
            ("insert", KeyCode::Insert),
            ("f1", KeyCode::F(1)),
            ("f24", KeyCode::F(24)),
            ("PageDown", KeyCode::PageDown),
            ("F5", KeyCode::F(5)),
            ("f", KeyCode::Char('f')),
            ("j", KeyCode::Char('j')),
            ("G", KeyCode::Char('G')),
            ("?", KeyCode::Char('?')),
            ("-", KeyCode::Char('-')),
            ("é", KeyCode::Char('é')),
        ] {
            assert_eq!(Key::parse(name), key(code, false, false), "{name}");
        }
    }

    #[test]
    fn modifiers_come_before_the_key_in_any_order() {
        assert_eq!(Key::parse("ctrl-o"), key(KeyCode::Char('o'), true, false));
        // Control letters are kept in lower case, others as written.
        assert_eq!(Key::parse("ctrl-O"), key(KeyCode::Char('o'), true, false));
        assert_eq!(Key::parse("alt-O"), key(KeyCode::Char('O'), false, true));
        assert_eq!(Key::parse("alt-left"), key(KeyCode::Left, false, true));
        assert_eq!(
            Key::parse("ctrl-alt-x"),
            key(KeyCode::Char('x'), true, true)
        );
        assert_eq!(
            Key::parse("alt-ctrl-X"),
            key(KeyCode::Char('x'), true, true)
        );
        assert_eq!(Key::parse("ctrl-ctrl-f2"), key(KeyCode::F(2), true, false));
        assert_eq!(
            Key::parse("ctrl-shift-tab"),
            key(KeyCode::BackTab, true, false)
        );
        assert_eq!(Key::parse("ctrl--"), key(KeyCode::Char('-'), true, false));
    }

    #[test]
    fn an_unknown_key_is_named_in_the_error() {
        for name in [
            "", "ctrl-", "hyper-x", "f0", "f25", "fx", "f-1", "Ctrl-x", "shift-x",
        ] {
            assert_eq!(
                Key::parse(name),
                Err(format!("unknown key \"{name}\"")),
                "{name}"
            );
        }
    }

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn the_path_is_under_xdg_config_home_else_home() {
        let want = |p: &str| Some(PathBuf::from(p));
        assert_eq!(path_in(os("/x"), os("/h")), want("/x/smd/config.toml"));
        assert_eq!(path_in(None, os("/h")), want("/h/.config/smd/config.toml"));
        assert_eq!(
            path_in(os(""), os("/h")),
            want("/h/.config/smd/config.toml")
        );
        assert_eq!(path_in(os("/x"), None), want("/x/smd/config.toml"));
        assert_eq!(path_in(None, None), None);
        assert_eq!(path_in(os(""), None), None);
    }

    #[test]
    fn no_path_gives_the_defaults() {
        let c = Config::load_from(None, |_| panic!("nothing to read")).unwrap();
        assert!(c.mouse);
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let c = Config::load_from(Some("/c".into()), |_| {
            Err(std::io::ErrorKind::NotFound.into())
        })
        .unwrap();
        assert_eq!(c.home, None);
    }

    #[test]
    fn a_file_is_read_from_its_path() {
        let c = Config::load_from(Some("/c/config.toml".into()), |p| {
            assert_eq!(p, Path::new("/c/config.toml"));
            Ok("mouse = false\n".into())
        })
        .unwrap();
        assert!(!c.mouse);
    }

    #[test]
    fn a_mistake_or_an_unreadable_file_names_the_path() {
        let err = Config::load_from(Some("/c/config.toml".into()), |_| Ok("mouse = 1\n".into()))
            .unwrap_err();
        assert!(err.starts_with("/c/config.toml: "), "{err}");
        let err = Config::load_from(Some("/c/config.toml".into()), |_| {
            Err(std::io::ErrorKind::PermissionDenied.into())
        })
        .unwrap_err();
        assert!(err.starts_with("/c/config.toml: "), "{err}");
    }
}
