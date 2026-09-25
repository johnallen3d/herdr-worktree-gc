# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/johnallen3d/herdr-worktree-gc/releases/tag/v0.1.0) - 2026-09-25

### Added

- add native Herdr worktree cleanup plugin

### Fixed

- clean up orphaned linked-worktree workspaces
- close stale workspaces after worktree removal
- retry workspace discovery after creation events
- allow cleanup after workspace closes

### Other

- mark project as archived
- automate GitHub releases with release-plz ([#1](https://github.com/johnallen3d/herdr-worktree-gc/pull/1))
- consolidate project tooling
- install Rust linting components with mise
