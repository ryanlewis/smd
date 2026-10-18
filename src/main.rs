mod app;
mod doc;
mod images;
mod kitty;
mod maths;
mod mermaid;
mod render;
mod theme;
mod watch;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version_line() -> String {
    format!("smd {VERSION}")
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    let [path] = args.as_slice() else {
        eprintln!("usage: smd <file.md>");
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
    let probe = kitty::probe();
    // Images are drawn for a dark background unless the terminal says otherwise.
    theme::set(probe.theme.unwrap_or(theme::Theme::Dark));
    let images = images::Images::new(probe.cell);
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
}
