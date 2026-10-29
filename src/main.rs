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

use std::io::{self, Write};
use std::panic;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

/// A terminal markdown reader
#[derive(Parser)]
#[command(version, after_help = KEYS)]
struct Args {
    /// Show images, diagrams and maths as their markdown source
    #[arg(long)]
    no_graphics: bool,
    /// The markdown file; with none, the `home` note from the config
    file: Option<PathBuf>,
}

const KEYS: &str = "\
The config file is ~/.config/smd/config.toml (or under $XDG_CONFIG_HOME).

Keys:
  j, k, arrows          scroll a line
  space, PgDn, PgUp     scroll a page
  ctrl-d, ctrl-u        scroll half a page
  g, G                  go to the top or bottom
  tab, shift-tab        select the next or previous link
  enter                 follow the selected link
  backspace, H          go back
  L                     go forward
  q, esc                quit

Click a link to follow it, and the toolbar's buttons to go back and forward.
The keys can be changed in the config file; see the README.";

/// The file to open: the one given, else the home note from the config.
fn pick_file(given: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf, &'static str> {
    given
        .or(home)
        .ok_or("no file given, and no `home` in ~/.config/smd/config.toml")
}

/// How the terminal and the config say things are drawn.
#[derive(Debug, PartialEq)]
struct Look {
    /// Images are drawn for a dark background unless the terminal says
    /// otherwise.
    theme: theme::Theme,
    /// Code blocks are shaded from it.
    background: Option<theme::Rgb>,
    /// Without a background, code blocks are drawn on the colour from the
    /// config, or indented if it has none.
    fallback_panel: Option<theme::Rgb>,
    /// The size of a cell, if the terminal can show images.
    cell: Option<(u16, u16)>,
}

impl Look {
    fn new(probe: Option<kitty::Probe>, code_background: Option<theme::Rgb>) -> Look {
        Look {
            theme: probe.and_then(|p| p.theme).unwrap_or(theme::Theme::Dark),
            background: probe.and_then(|p| p.background),
            fallback_panel: code_background,
            cell: probe.filter(|p| p.graphics).map(|p| p.cell),
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let no_graphics = args.no_graphics;
    let config = match config::Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("smd: {err}");
            return ExitCode::from(2);
        }
    };
    let path = match pick_file(args.file, config.home.clone()) {
        Ok(path) => path,
        Err(message) => Args::command()
            .error(ErrorKind::MissingRequiredArgument, message)
            .exit(),
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
    let look = Look::new(probe, config.code_background);
    theme::set(look.theme);
    if let Some(rgb) = look.background {
        theme::set_background(rgb);
    }
    if let Some(rgb) = look.fallback_panel {
        theme::set_fallback_panel(rgb);
    }
    let images = match look.cell {
        Some(cell) => {
            heading::set_cell(cell);
            images::Images::new(cell)
        }
        None => images::Images::text(),
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
    fn the_arguments_are_valid() {
        Args::command().debug_assert();
    }
}
