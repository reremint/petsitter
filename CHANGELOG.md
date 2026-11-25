# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-11-25

First release.

- Live view of every Claude Code, Codex and opencode session under a folder,
  in a tree of their folders (or a flat list, `--view flat`), with their
  state, model, effort, context, tokens, background shells and sub-agents,
  each sub-agent under the one that started it.
- A `[bg]` tag on sessions that run in the background with no terminal; the
  details panel says "no terminal attached".
- Machine load and each account's quotas at the top.
- Details panel for a session, or for a git folder: the branches its sessions
  work on, and any commit's files and diff.
- Enter jumps to a session's terminal pane in Rex; `y` copies the command
  that resumes it; exited sessions can be deleted.
- Follows the terminal's light or dark theme, and honors `NO_COLOR`.
- `--once` prints the sessions as plain text; `petsitter doctor` checks the
  setup.
- Man pages, generated from `--help`: `petsitter man ~/.local/share/man/man1`
  installs them, then `man petsitter` works. They also ship in the release
  archives, with the licenses of the crates built into the binary.
- Optional hooks for fresher Claude quotas and Codex approval prompts.

[Unreleased]: https://github.com/reremint/petsitter/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/reremint/petsitter/releases/tag/v0.1.0
