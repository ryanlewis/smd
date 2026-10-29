mod app;
mod config;
mod doc;
mod heading;
mod highlight;
mod images;
mod kitty;
mod maths;
mod mermaid;
mod raster;
mod render;
mod table;
mod theme;
mod vault;
mod watch;

use std::ffi::OsString;
use std::io::{self, Write};
use std::panic;
use std::path::PathBuf;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version_line() -> String {
    format!("smd {VERSION}")
}

/// Remove every `flag` from `args`, returning whether there was one.
fn take_flag(args: &mut Vec<OsString>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    let no_graphics = take_flag(&mut args, "--no-graphics");
    let config = match config::Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("smd: {err}");
            return ExitCode::from(2);
        }
    };
    // With no file, the home note from the config.
    let path: PathBuf = match (args.as_slice(), &config.home) {
        ([path], _) => path.into(),
        ([], Some(home)) => home.clone(),
        _ => {
            eprintln!("usage: smd [--no-graphics] [<file.md>]");
            eprintln!("With no file, smd opens `home` from ~/.config/smd/config.toml.");
            return ExitCode::from(2);
        }
    };
    let watcher = watch::Watcher::new(path.clone());
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("smd: {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let vault = vault::Vault::for_file(&path);
    // Asked before the terminal is set up, while nothing else reads its replies.
    let probe = (!no_graphics).then(kitty::probe);
    // Images are drawn for a dark background unless the terminal says otherwise.
    theme::set(probe.and_then(|p| p.theme).unwrap_or(theme::Theme::Dark));
    // Code blocks are shaded from it. Without it they are drawn on the
    // colour from the config, or indented if it has none.
    if let Some(rgb) = probe.and_then(|p| p.background) {
        theme::set_background(rgb);
    }
    if let Some(rgb) = config.code_background {
        theme::set_fallback_panel(rgb);
    }
    let images = match probe {
        Some(probe) if probe.graphics => {
            heading::set_cell(probe.cell);
            images::Images::new(probe.cell)
        }
        _ => images::Images::text(),
    };

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            eprintln!("smd: cannot open the terminal: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Set after ratatui's hook, which restores the terminal, so it runs first.
    // A job's panic is caught and shown as a failed image, so it prints
    // nothing; any other deletes the images before the terminal is restored.
    let mouse = config.mouse;
    if mouse {
        let _ = io::stdout().write_all(app::MOUSE_ON.as_bytes());
    }
    let uploaded = images.uploaded();
    let restore = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if images::in_job() {
            return;
        }
        let _ = uploaded.delete_all(&mut io::stdout());
        if mouse {
            let _ = io::stdout().write_all(app::MOUSE_OFF.as_bytes());
        }
        restore(info);
    }));
    // Started once the hook is set, like the jobs below.
    highlight::load_in_background();
    // Made once the hook is set, as this starts the jobs.
    // Relative image paths are read from the markdown file's directory.
    let doc = app::parse(&vault, &source, &path);
    let view = app::View::new(doc, app::folder(&path), images);
    let browser = app::Browser::new(view, path, watcher, vault, config);
    let result = app::run(&mut terminal, browser);
    if mouse {
        let _ = io::stdout().write_all(app::MOUSE_OFF.as_bytes());
    }
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("smd: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_has_name_and_version() {
        assert_eq!(version_line(), format!("smd {}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn take_flag_removes_the_flag_wherever_it_is() {
        let mut args: Vec<OsString> = vec!["--no-graphics".into(), "a.md".into()];
        assert!(take_flag(&mut args, "--no-graphics"));
        assert_eq!(args, vec![OsString::from("a.md")]);
        let mut args: Vec<OsString> = vec!["a.md".into()];
        assert!(!take_flag(&mut args, "--no-graphics"));
        assert_eq!(args.len(), 1);
    }
}
