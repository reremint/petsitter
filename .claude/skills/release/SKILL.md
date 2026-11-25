---
name: release
description: Release a new version of petsitter - choose the version, bump it, move the changelog, regenerate the man pages, check everything, then tag so cargo-dist publishes the GitHub release. Use when the maintainer asks to release, cut or tag a version.
disable-model-invocation: true
---

# Releasing petsitter

A release is a `vX.Y.Z` tag on `main`. Pushing it runs
`.github/workflows/release.yml` (cargo-dist), which builds the archives and
the shell installer and creates the GitHub release, using the version's
section of `CHANGELOG.md` as its notes.

Tags, pushes and `cargo publish` cannot be taken back. **Stop and ask the
maintainer before each commit, push, merge, tag and publish**, even when an
earlier step was approved.

## 1. Check the starting point

```sh
git fetch --tags origin
git status --short                      # must be empty
git log --oneline -1 origin/main
gh run list --branch main --limit 3     # CI must be green on origin/main
git describe --tags --abbrev=0 2>/dev/null || echo "no release yet"
```

Stop if the tree is dirty or CI on `main` is not green. Ask whether any open
pull request should land first (`gh pr list`).

## 2. Choose the version

Read `## [Unreleased]` in `CHANGELOG.md` and the commits since the last tag
(`git log --oneline <last tag>..origin/main`, or `git log --oneline
origin/main` when there is no tag yet). Anything a user will notice
must be in the changelog: add the missing entries first, in the existing
style (short, from the user's point of view, under `### Added`, `### Changed`,
`### Fixed` or `### Removed`).

Following [Semantic Versioning](https://semver.org) while the major version
is 0: a change that breaks how people use petsitter (a removed or renamed
option, command or key) bumps the minor version, anything else bumps the
patch version. Propose the version with that reasoning, and wait for the
maintainer to confirm it.

If the newest version in the changelog was never tagged (`git tag -l`
does not list it), it was never released: ask whether to release it as is,
with the `Unreleased` entries moved into it, rather than skipping to the next
number.

## 3. Prepare the release branch

```sh
git switch -c release-vX.Y.Z origin/main
```

When releasing a version that is already in the changelog but was never
tagged (see step 2), skip item 1 and do item 2 this way instead: merge the
`Unreleased` entries into that version's section, add today's date to its
heading (`## [X.Y.Z] - YYYY-MM-DD`), leave `## [Unreleased]` empty above
it, and keep its link at the bottom as it is.

1. `Cargo.toml`: set `version = "X.Y.Z"`, then run `cargo check` so
   `Cargo.lock` follows. `git diff Cargo.lock` must show only the
   `petsitter` entry changing.
2. `CHANGELOG.md`:
   - Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` (today's date),
     and add a new empty `## [Unreleased]` above it.
   - At the bottom, point `[Unreleased]` at
     `https://github.com/reremint/petsitter/compare/vX.Y.Z...HEAD`, and add
     `[X.Y.Z]: https://github.com/reremint/petsitter/compare/vPREVIOUS...vX.Y.Z`
     (or `.../releases/tag/vX.Y.Z` for the first release).
3. Regenerate the man pages, which carry the version:
   `rm -rf man && cargo run -- man man`.
4. Regenerate `THIRD-PARTY-LICENSES`, the notices of the crates built into
   the archives (`cargo fetch` first gets the Linux-only crates on macOS;
   generation itself runs offline):

   ```sh
   cargo fetch --locked
   cargo about generate --frozen --fail about.hbs -o THIRD-PARTY-LICENSES
   ```

   `cargo-about` installs with
   `cargo install cargo-about --locked --features cli --version 0.9.3`.

## 4. Check everything

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --locked
cargo package --locked --allow-dirty  # builds the crate as crates.io would
dist generate --check     # release.yml matches dist-workspace.toml
dist plan                 # every archive lists man/ and THIRD-PARTY-LICENSES
test -s THIRD-PARTY-LICENSES
git diff --stat THIRD-PARTY-LICENSES  # what changed in the dependencies
grep -c '<copyright holders>' THIRD-PARTY-LICENSES  # must print 1
grep -nE '/Users/|/home/|/private/|\.cargo/registry|&quot;|&lt;' THIRD-PARTY-LICENSES  # must print nothing
```

`dist plan` only repeats the configuration: it lists `THIRD-PARTY-LICENSES`
even when the file is missing, hence `test -s`. The one
`<copyright holders>` placeholder is the objc2 section's, which ship no
copyright notice (see the file's header); the last grep catches local paths
and HTML escaping.

`--allow-dirty` is needed because nothing is committed yet; step 6 runs
`cargo publish --dry-run` on the merged commit before tagging it.

`dist` is cargo-dist, at the version in `dist-workspace.toml`
(`cargo-dist-version`); install it from its GitHub releases if missing.
`cargo-about` is needed too (step 3).
The release notes come from the `## [X.Y.Z]` section of `CHANGELOG.md`:
keep that heading in exactly this form.

Then run the `oss-compliance` agent on the branch, and `tui-checker` if
anything under `src/ui/`, `src/app.rs`, `src/tree.rs`, `src/view.rs` or
`src/demo.rs` changed since the last tag. Fix and check again until they have
nothing left.

## 5. Land it

After the maintainer approves each step:

1. Commit everything as `petsitter X.Y.Z`, with a body that sums up the
   release in a sentence or two.
2. `git push -u origin release-vX.Y.Z` and open a pull request
   (`gh pr create`). The release workflow also runs `dist plan` on pull
   requests: wait for it and for CI to be green.
3. The maintainer merges it.

## 6. Tag

On the merged commit, never on the release branch:

```sh
git fetch origin && git switch --detach origin/main
git log --oneline -1                    # must be the `petsitter X.Y.Z` commit
cargo publish --locked --dry-run        # must pass before the tag exists
git tag -a vX.Y.Z -m "petsitter X.Y.Z"
git push origin vX.Y.Z                  # starts the release workflow
gh run list --workflow release.yml --event push --branch vX.Y.Z --limit 1
gh run watch <run id>
```

The workflow also runs on pull requests, so filter on the tag's push. If
the list is still empty, the run is not registered yet: wait a few seconds
and list again.

## 7. Check the release

- `gh release view vX.Y.Z`: notes from the changelog, four archives (macOS
  and Linux, arm64 and x86_64) with their `.sha256`, and
  `petsitter-installer.sh`.
- Download one archive into a scratch directory and check it holds the
  binary, `README.md`, `CHANGELOG.md`, `LICENSE`, `THIRD-PARTY-LICENSES`
  and `man/`.
- Run the installer into a scratch directory, which also leaves `PATH` and
  the shell profiles alone:
  `PETSITTER_UNMANAGED_INSTALL=<scratch>/bin sh petsitter-installer.sh`.
  Then check `<scratch>/bin/petsitter --version` and
  `<scratch>/bin/petsitter man <scratch>/man1`.

## 8. crates.io

The README offers `cargo install petsitter --locked`, so each release also
goes to crates.io. Ask first: it needs the maintainer's token, and a
published version can never be replaced. The dry run already passed in
step 6.

```sh
cargo publish --locked
```

Finally, tell the maintainer what was released, with the links to the GitHub
release and the crates.io page.
