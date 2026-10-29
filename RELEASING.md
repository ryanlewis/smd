# Releasing

Pushing a `v*` tag runs `.github/workflows/release.yml`. It uses GoReleaser to:

1. Build `smd` for macOS (Apple silicon) and Linux (x86_64 and arm64) on one Linux runner, with `cargo-zigbuild`.
2. Sign and notarize the macOS binary.
3. Publish a GitHub release with the archives and `checksums.txt`, and attest their build provenance.
4. Write `Casks/smd.rb` to [ryanlewis/homebrew-tap](https://github.com/ryanlewis/homebrew-tap).

## Cutting a release

1. Set `version` in `Cargo.toml`, run `cargo check` to update `Cargo.lock`, and commit to `main`.
2. Tag and push:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

To try the build locally without publishing (needs `zig` and `cargo-zigbuild` 0.23 or later):

```sh
goreleaser release --snapshot --clean --skip=publish
```

## Secrets

All of these live in the `release` environment, which only `v*` tags can use. The workflow stops before building if any of them is missing.

| Secret | Contents |
| --- | --- |
| `HOMEBREW_TAP_GITHUB_TOKEN` | Token that can push to `ryanlewis/homebrew-tap` |
| `MACOS_SIGN_P12` | base64 of the Developer ID Application certificate and key (`.p12`) |
| `MACOS_SIGN_PASSWORD` | Password for the `.p12` |
| `MACOS_NOTARY_ISSUER_ID` | App Store Connect API issuer ID |
| `MACOS_NOTARY_KEY_ID` | App Store Connect API key ID |
| `MACOS_NOTARY_KEY` | base64 of the App Store Connect API key (`.p8`) |

The signing credentials are the same as things-cli's. How to create and rotate them is in that repo's `.github/SIGNING.md`.
