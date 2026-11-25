# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Man pages, generated from `--help`: `petsitter man ~/.local/share/man/man1`
  installs them, then `man petsitter` works. They also ship in the release
  archives.

### Fixed

- Sub-agents started by another sub-agent are now nested under it, as in
  Claude Code's background view.
- Enter on a Claude Code background session no longer jumps to the pane it
  was started from, or to an unrelated pane once the terminal showing it was
  closed.

## [0.1.0]

First release.

- Live view of every Claude Code, Codex and opencode session under a folder,
  in a tree of their folders, with their state, model, effort, context,
  tokens, background shells and sub-agents.
- Machine load and each account's quotas at the top.
- Details panel for a session, or for a git folder: the branches its sessions
  work on, and any commit's files and diff.
- Optional hooks for fresher Claude quotas and Codex approval prompts.

[Unreleased]: https://github.com/reremint/petsitter/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/reremint/petsitter/releases/tag/v0.1.0
