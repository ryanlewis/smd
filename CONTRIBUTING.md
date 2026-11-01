# Contributing

Issues and pull requests are welcome. smd is a small personal project, so please open an issue to talk about a large change before starting on it.

Before opening a pull request, run:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org): `fix:`, `feat:`, `docs:`, `test:`, `refactor:`.
- A bug fix comes with a test that fails without it.
- A new dependency is pinned to an exact version (`=1.2.3`).
- smd is tested in [Ghostty](https://ghostty.org). Say which terminal you tried a change in.

## Screenshot tests

The `screenshots` workflow opens each note in `tests/screenshots` with smd in [kitty](https://sw.kovidgoyal.net/kitty/), on a headless X server, and compares a screenshot of the window with the matching PNG in `tests/screenshots/reference`. This catches changes to how headings, diagrams, maths, images, code and tables are drawn that the unit tests cannot see. kitty and the DejaVu fonts are pinned in `tests/screenshots/install.sh`, and the window size, font and colours in `tests/screenshots/kitty.conf`.

A screenshot fails when more than 100 pixels differ from its reference. The run then uploads a `screenshots` artifact holding, for each failed note, `<note>.png` (this run), `<note>.reference.png` and `<note>.diff.png`, which shows the changed pixels in red.

When a change is meant to alter the screenshots, make new references in CI, not on your own machine, whose fonts and graphics differ:

1. Push the change to a branch.
2. Run the `screenshots` workflow on that branch from the Actions tab with **update** ticked, or with `gh workflow run screenshots.yml --ref <branch> -f update=true`.
3. Download the `screenshots` artifact (`gh run download <run id> -n screenshots -D tests/screenshots/reference`), check the images look right, and commit them.

The same applies when you change the pinned kitty or font version, or add a note to `tests/screenshots`.

To run the tests yourself on Debian or Ubuntu (this changes system fonts, so a container is best):

```sh
cargo build --locked --release
sudo tests/screenshots/install.sh
tests/screenshots/capture.sh target/release/smd /tmp/screenshots
tests/screenshots/compare.sh /tmp/screenshots
```
