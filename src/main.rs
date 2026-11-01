mod app;
mod config;
mod doc;
mod embed;
mod heading;
mod help;
mod highlight;
mod html;
mod images;
mod kitty;
mod maths;
mod mermaid;
mod raster;
mod remote;
mod render;
mod search;
mod table;
mod theme;
mod vault;
mod watch;

use std::io::{self, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

/// A sexy markdown reader for the terminal
#[derive(Parser)]
#[command(version, after_help = KEYS)]
struct Args {
    /// Show images, diagrams and maths as their markdown source
    #[arg(long)]
    no_graphics: bool,
    /// Fetch images from the web (http and https), whatever the config says
    #[arg(long, overrides_with = "no_remote_images")]
    remote_images: bool,
    /// Do not fetch images from the web, whatever the config says
    #[arg(long, overrides_with = "remote_images")]
    no_remote_images: bool,
    /// The markdown file, or `-` for stdin; with none, stdin when markdown
    /// is piped in, else the `home` note from the config
    file: Option<PathBuf>,
}

const KEYS: &str = "\
The config file is ~/.config/smd/config.toml (or under $XDG_CONFIG_HOME).

Keys:
  j, k, arrows            scroll a line
  space, PgDn, ctrl-f     scroll a page down
  PgUp, ctrl-b            scroll a page up
  ctrl-d, ctrl-u          scroll half a page
  g, G, home, end         go to the top or bottom
  tab, shift-tab          select the next or previous link or checkbox
  enter                   follow the selected link, or tick the checkbox
  x                       tick or untick the selected checkbox
  backspace, H, alt-left  go back
  L, alt-right            go forward
  /, n, N                 search, go to the next or previous match
  r                       reload the file
  ?                       show the help page, with the keys as bound
  5j, 42G                 a number first repeats a key; with g or G, goes to
                          that line
  q, esc, ctrl-c          quit

Click a link to follow it, a checkbox to tick it, and the toolbar's buttons to
go back and forward.
The keys can be changed in the config file; see the README.";

/// The name a document read from stdin goes by. Its relative links and
/// images are read from the current directory.
const STDIN_NAME: &str = "stdin";

/// Where the markdown comes from.
#[derive(Debug, PartialEq)]
enum Source {
    File(PathBuf),
    Stdin,
}

/// Where to read the markdown from: stdin when `-` is given, or when no
/// file is and markdown is `piped_in`; else the file given, else the home
/// note from the config.
fn pick_source(
    given: Option<PathBuf>,
    home: Option<PathBuf>,
    piped_in: bool,
) -> Result<Source, &'static str> {
    match given {
        Some(path) if path.as_os_str() == "-" => Ok(Source::Stdin),
        Some(path) => Ok(Source::File(path)),
        None if piped_in => Ok(Source::Stdin),
        None => home
            .map(Source::File)
            .ok_or("no file given, and no `home` in ~/.config/smd/config.toml"),
    }
}

/// Whether web images are fetched: as the flags say, else as the config does.
fn remote_images(on: bool, off: bool, config: bool) -> bool {
    match (on, off) {
        (true, _) => true,
        (_, true) => false,
        _ => config,
    }
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

/// Why smd stopped early: what it prints after `smd: ` on stderr, and the
/// exit code.
#[derive(Debug, PartialEq)]
struct Failure {
    message: String,
    code: u8,
}

impl Failure {
    fn new(message: impl std::fmt::Display) -> Failure {
        Failure {
            message: message.to_string(),
            code: 1,
        }
    }
}

/// The config, or a failure with exit code 2 when it cannot be read.
fn loaded(config: Result<config::Config, String>) -> Result<config::Config, Failure> {
    config.map_err(|err| Failure {
        message: err,
        code: 2,
    })
}

/// The markdown file's text, or a failure naming the file.
fn source(path: &Path, read: io::Result<String>) -> Result<String, Failure> {
    read.map_err(|err| Failure::new(format!("{}: {err}", path.display())))
}

/// Whether `fd` is a pipe or a file, as stdin is with `cmd | smd` or
/// `smd < a.md`. Not when it is /dev/null or closed, as when a launcher
/// starts smd: the home note is opened then.
fn is_piped(fd: libc::c_int) -> bool {
    // SAFETY: a stat is plain data, and fstat only writes the one it is given.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return false;
    }
    let kind = st.st_mode & libc::S_IFMT;
    kind == libc::S_IFIFO || kind == libc::S_IFREG
}

/// The markdown read from stdin, or a failure saying it cannot be read.
fn piped(read: io::Result<String>) -> Result<String, Failure> {
    read.map_err(|err| Failure::new(format!("cannot read stdin: {err}")))
}

/// The terminal, or a failure saying it cannot be opened.
fn opened<T>(terminal: io::Result<T>) -> Result<T, Failure> {
    terminal.map_err(|err| Failure::new(format!("cannot open the terminal: {err}")))
}

/// How the browser's run ended, as smd's.
fn finished(result: io::Result<()>) -> Result<(), Failure> {
    result.map_err(Failure::new)
}

/// The exit code for how smd ended, after writing a failure's message to
/// `err`.
fn exit_code(result: Result<(), Failure>, err: &mut impl Write) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            // Panics when it cannot write, as `eprintln!` does.
            if let Err(e) = writeln!(err, "smd: {}", failure.message) {
                panic!("failed printing to stderr: {e}");
            }
            ExitCode::from(failure.code)
        }
    }
}

/// Images drawn at `cell` when the terminal can show them, else as text.
fn images_for(cell: Option<(u16, u16)>) -> images::Images {
    match cell {
        Some(cell) => images::Images::new(cell),
        None => images::Images::text(),
    }
}

/// Turn mouse reporting on or off, when the config lets smd take the mouse.
fn set_mouse(out: &mut impl Write, enabled: bool, on: bool) {
    if enabled {
        let bytes = if on { app::MOUSE_ON } else { app::MOUSE_OFF };
        let _ = out.write_all(bytes.as_bytes());
    }
}

/// Set the theme, code background and heading cell the look says.
fn apply(look: &Look) {
    theme::set(look.theme);
    if let Some(rgb) = look.background {
        theme::set_background(rgb);
    }
    if let Some(rgb) = look.fallback_panel {
        theme::set_fallback_panel(rgb);
    }
    if let Some(cell) = look.cell {
        heading::set_cell(cell);
    }
}

/// Set after ratatui's hook, which restores the terminal, so it runs first.
/// A job's panic is caught and shown as a failed image, so it prints
/// nothing; any other deletes the images before the terminal is restored.
fn set_panic_hook(uploaded: images::Uploaded, mouse: bool) {
    let restore = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if images::in_job() {
            return;
        }
        let _ = uploaded.delete_all(&mut io::stdout());
        set_mouse(&mut io::stdout(), mouse, false);
        restore(info);
    }));
}

fn main() -> ExitCode {
    let args = Args::parse();
    exit_code(smd(args), &mut io::stderr())
}

fn smd(args: Args) -> Result<(), Failure> {
    let config = loaded(config::Config::load())?;
    let picked = pick_source(args.file, config.home.clone(), is_piped(libc::STDIN_FILENO));
    let picked = picked.unwrap_or_else(|message| {
        Args::command()
            .error(ErrorKind::MissingRequiredArgument, message)
            .exit()
    });
    let from_stdin = picked == Source::Stdin;
    let (path, read) = match picked {
        Source::File(path) => {
            let read = std::fs::read_to_string(&path);
            (path, read)
        }
        // Keys are then read from the terminal itself, /dev/tty.
        Source::Stdin => (PathBuf::from(STDIN_NAME), io::read_to_string(io::stdin())),
    };
    let watcher = watch::Watcher::new(path.clone());
    let source = if from_stdin {
        piped(read)?
    } else {
        source(&path, read)?
    };
    let vault = vault::Vault::for_file(&path);
    // Asked before the terminal is set up, while nothing else reads its replies.
    let probe = (!args.no_graphics).then(kitty::probe);
    let look = Look::new(probe, config.code_background);
    apply(&look);
    let text_sizing = probe.is_some_and(|p| p.text_sizing);
    heading::set_drawn(heading::choose(config.headings, text_sizing));
    mermaid::set_renderer(config.mermaid);
    remote::set_enabled(remote_images(
        args.remote_images,
        args.no_remote_images,
        config.remote_images,
    ));
    let mut images = images_for(look.cell);
    images.use_shared_memory(probe.is_some_and(|p| p.shared_memory));

    let mut terminal = opened(ratatui::try_init())?;
    let mouse = config.mouse;
    set_mouse(&mut io::stdout(), mouse, true);
    // After ratatui's hook, so it runs first.
    set_panic_hook(images.uploaded(), mouse);
    // SIGTERM and SIGHUP now end the loop below, so the terminal is restored.
    app::stop_on_signals();
    // Started once the hook is set, like the jobs below.
    highlight::load_in_background();
    // Made once the hook is set, as this starts the jobs.
    // Relative image paths are read from the markdown file's directory.
    let doc = app::parse(&vault, &source, &path);
    let view = app::View::new(doc, app::folder(&path), images);
    let mut browser = app::Browser::new(view, path, watcher, vault, config);
    if from_stdin {
        browser = browser.piped();
    }
    let result = app::run(&mut terminal, browser);
    set_mouse(&mut io::stdout(), mouse, false);
    match app::stop_signal() {
        // After SIGHUP the terminal is gone and an error about it cannot be
        // printed, so none is. smd then ends by the signal it was sent.
        Some(signal) => {
            let _ = ratatui::try_restore();
            app::raise(signal);
        }
        None => ratatui::restore(),
    }
    finished(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn the_arguments_are_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn the_help_lists_every_default_key() {
        let words: Vec<&str> = KEYS
            .split(|c: char| c.is_whitespace() || c == ',')
            .collect();
        for key in config::Action::all_default_keys() {
            let shown = match key {
                "up" | "down" => "arrows",
                "pagedown" => "PgDn",
                "pageup" => "PgUp",
                other => other,
            };
            assert!(words.contains(&shown), "the help does not list {key}");
        }
    }

    #[test]
    fn a_given_file_wins_over_home() {
        let given = Some(PathBuf::from("a.md"));
        let home = Some(PathBuf::from("home.md"));
        let file = |name: &str| Ok(Source::File(PathBuf::from(name)));
        assert_eq!(
            pick_source(given.clone(), home.clone(), false),
            file("a.md")
        );
        assert_eq!(pick_source(given, home.clone(), true), file("a.md"));
        assert_eq!(pick_source(None, home, false), file("home.md"));
    }

    #[test]
    fn stdin_is_read_when_piped_or_given_as_a_dash() {
        let home = Some(PathBuf::from("home.md"));
        assert_eq!(pick_source(None, home.clone(), true), Ok(Source::Stdin));
        assert_eq!(pick_source(None, None, true), Ok(Source::Stdin));
        let dash = Some(PathBuf::from("-"));
        assert_eq!(pick_source(dash, home, false), Ok(Source::Stdin));
    }

    #[test]
    fn only_a_pipe_or_a_file_counts_as_piped_in() {
        use std::os::fd::AsRawFd;
        let mut fds = [0; 2];
        // SAFETY: pipe writes two descriptors into fds.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        assert!(is_piped(fds[0]));
        // SAFETY: both descriptors are ours and closed once.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        let file = std::fs::File::open("Cargo.toml").unwrap();
        assert!(is_piped(file.as_raw_fd()));
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!is_piped(null.as_raw_fd()));
        assert!(!is_piped(-1));
    }

    #[test]
    fn stdin_that_cannot_be_read_says_so() {
        assert_eq!(piped(Ok("# a".to_string())), Ok("# a".to_string()));
        assert_eq!(
            piped(Err(io_err("bad bytes"))),
            Err(Failure::new("cannot read stdin: bad bytes"))
        );
    }

    #[test]
    fn the_remote_images_flags_win_over_the_config() {
        assert!(!remote_images(false, false, false));
        assert!(remote_images(false, false, true));
        assert!(remote_images(true, false, false));
        assert!(!remote_images(false, true, true));
        // The last of the two flags given wins.
        let args = Args::parse_from(["smd", "--remote-images", "--no-remote-images", "a.md"]);
        assert!(!remote_images(
            args.remote_images,
            args.no_remote_images,
            true
        ));
        let args = Args::parse_from(["smd", "--no-remote-images", "--remote-images", "a.md"]);
        assert!(remote_images(
            args.remote_images,
            args.no_remote_images,
            false
        ));
    }

    #[test]
    fn no_file_and_no_home_says_so() {
        let err = pick_source(None, None, false).unwrap_err();
        assert!(err.contains("no file given") && err.contains("home"));
    }

    fn probe(theme: Option<Theme>, background: Option<theme::Rgb>, graphics: bool) -> kitty::Probe {
        kitty::Probe {
            cell: (9, 18),
            theme,
            background,
            graphics,
            shared_memory: false,
            text_sizing: false,
        }
    }

    #[test]
    fn without_a_probe_it_is_dark_and_text() {
        let look = Look::new(None, Some((1, 2, 3)));
        assert_eq!(
            look,
            Look {
                theme: Theme::Dark,
                background: None,
                fallback_panel: Some((1, 2, 3)),
                cell: None,
            }
        );
    }

    #[test]
    fn the_probe_sets_the_theme_background_and_cell() {
        let p = probe(Some(Theme::Light), Some((250, 250, 250)), true);
        let look = Look::new(Some(p), None);
        assert_eq!(look.theme, Theme::Light);
        assert_eq!(look.background, Some((250, 250, 250)));
        assert_eq!(look.fallback_panel, None);
        assert_eq!(look.cell, Some((9, 18)));
    }

    #[test]
    fn a_terminal_without_graphics_gets_text_images() {
        let look = Look::new(Some(probe(None, None, false)), None);
        assert_eq!(look.theme, Theme::Dark);
        assert_eq!(look.cell, None);
    }

    fn io_err(text: &str) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, text)
    }

    #[test]
    fn a_config_that_cannot_be_read_exits_with_two() {
        let failure = loaded(Err("bad config".to_string())).unwrap_err();
        assert_eq!(
            failure,
            Failure {
                message: "bad config".to_string(),
                code: 2
            }
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_is_named() {
        let path = Path::new("notes/a.md");
        assert_eq!(source(path, Ok("# a".to_string())), Ok("# a".to_string()));
        let failure = source(path, Err(io_err("gone"))).unwrap_err();
        assert_eq!(failure, Failure::new("notes/a.md: gone"));
        assert_eq!(failure.code, 1);
    }

    #[test]
    fn a_terminal_that_cannot_be_opened_says_so() {
        assert_eq!(opened(Ok(3)), Ok(3));
        assert_eq!(
            opened::<()>(Err(io_err("no tty"))),
            Err(Failure::new("cannot open the terminal: no tty"))
        );
    }

    #[test]
    fn a_run_that_fails_is_a_failure() {
        assert_eq!(finished(Ok(())), Ok(()));
        assert_eq!(finished(Err(io_err("broken"))), Err(Failure::new("broken")));
    }

    #[test]
    fn the_exit_code_comes_with_the_message() {
        let mut err = Vec::new();
        assert_eq!(exit_code(Ok(()), &mut err), ExitCode::SUCCESS);
        assert!(err.is_empty());
        let failure = Failure {
            message: "bad config".to_string(),
            code: 2,
        };
        assert_eq!(exit_code(Err(failure), &mut err), ExitCode::from(2));
        assert_eq!(String::from_utf8(err).unwrap(), "smd: bad config\n");
        let mut err = Vec::new();
        assert_eq!(
            exit_code(Err(Failure::new("gone")), &mut err),
            ExitCode::FAILURE
        );
        assert_eq!(String::from_utf8(err).unwrap(), "smd: gone\n");
    }

    #[test]
    fn images_are_graphics_only_with_a_cell() {
        let images = images_for(Some((9, 18)));
        assert!(images.graphics());
        assert_eq!(images.cell(), (9, 18));
        assert!(!images_for(None).graphics());
    }

    #[test]
    fn the_mouse_is_set_only_when_the_config_lets_smd_take_it() {
        let mut out = Vec::new();
        set_mouse(&mut out, true, true);
        assert_eq!(out, app::MOUSE_ON.as_bytes());
        let mut out = Vec::new();
        set_mouse(&mut out, true, false);
        assert_eq!(out, app::MOUSE_OFF.as_bytes());
        let mut out = Vec::new();
        set_mouse(&mut out, false, true);
        set_mouse(&mut out, false, false);
        assert!(out.is_empty());
    }
}
