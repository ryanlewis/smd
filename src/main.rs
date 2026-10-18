mod app;
mod doc;
mod heading;
mod images;
mod kitty;
mod maths;
mod mermaid;
mod raster;
mod render;
mod table;
mod theme;
mod watch;

use std::ffi::OsString;
use std::path::Path;
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
    let [path] = args.as_slice() else {
        eprintln!("usage: smd [--no-graphics] <file.md>");
        return ExitCode::from(2);
    };
    let watcher = watch::Watcher::new(path.into());
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("smd: {}: {err}", Path::new(path).display());
            return ExitCode::FAILURE;
        }
    };
    // Relative image paths are read from the markdown file's directory.
    let base = match Path::new(path).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => ".".into(),
    };
    // Asked before the terminal is set up, while nothing else reads its replies.
    let probe = (!no_graphics).then(kitty::probe);
    // Images are drawn for a dark background unless the terminal says otherwise.
    theme::set(probe.and_then(|p| p.theme).unwrap_or(theme::Theme::Dark));
    let images = match probe {
        Some(probe) if probe.graphics => {
            heading::set_cell(probe.cell);
            images::Images::new(probe.cell)
        }
        _ => images::Images::text(),
    };
    let view = app::View::new(doc::parse(&source), base, images);

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            eprintln!("smd: cannot open the terminal: {err}");
            return ExitCode::FAILURE;
        }
    };
    let result = app::run(&mut terminal, view, Path::new(path), watcher);
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
