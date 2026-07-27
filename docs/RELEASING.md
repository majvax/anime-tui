# Releasing

How a new version reaches users. Distribution channels: **GitHub Releases** (static
`x86_64-unknown-linux-musl` binary), **AUR `anime-tui`** (builds from source), and
**AUR `anime-tui-bin`** (installs the prebuilt binary).

## One-time setup

The repository has no remote until you create it:

```bash
gh repo create majvax/anime-tui --public --source . --remote origin --push
```

Create the two AUR repos once (`ssh aur@aur.archlinux.org setup-repo anime-tui` and
`… anime-tui-bin`, or push an initial commit) and clone them somewhere locally.

## Cutting a release

1. **Bump the version** in `Cargo.toml` (`version = "X.Y.Z"`); `cargo build` to update
   `Cargo.lock`. Commit.
2. **Verify locally:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`. (CI runs the same on push.)
3. **Tag and push:**

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

   The `Release` workflow (`.github/workflows/release.yml`) builds the static musl
   binary, tars it as `anime-tui-x86_64-unknown-linux-musl.tar.gz` with a `.sha256`
   sidecar, and creates the GitHub Release with both attached.
4. **Update the AUR packages** (on an Arch box, after the Release is live):

   ```bash
   packaging/update-aur.sh X.Y.Z
   ```

   This bumps `pkgver` in both PKGBUILDs, pins the `-bin` package's `sha256sums` from
   the release artifact, and regenerates both `.SRCINFO`s. Review the diffs, copy each
   `packaging/aur/<pkg>/` into its AUR git clone, commit, and `git push`.

## Notes

- The source PKGBUILD builds with `cargo build --frozen` against the committed
  `Cargo.lock`, so package builds are reproducible.
- The binary is fully static (rusqlite `bundled` + reqwest `rustls-tls`), so
  `anime-tui-bin` has no glibc/version constraints beyond a Linux `x86_64` kernel.
- Runtime dependencies (`mpv`, `yt-dlp`) are declared in both PKGBUILDs; `ffmpeg` is
  an optdepend used only by `scripts/gen_test_media.sh`.
- crates.io is intentionally **not** a release channel.
