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
