# Herdr Worktree GC

A conservative [Herdr](https://herdr.dev) plugin that discovers repositories from the live Herdr session and removes linked Git worktrees after their configured upstream branch disappears.

The plugin reacts to Herdr startup and `workspace.created`, `workspace.focused`, and `workspace.closed`. Fetches are debounced per repository, and an inter-process lock prevents overlapping cleanup runs.

## Safety model

Worktree GC:

- fetches all remotes with `git fetch --all --prune` before making decisions;
- does not act when fetching fails;
- ignores the main checkout, detached worktrees, and branches without an upstream;
- skips the focused worktree, worktrees with a Herdr agent, and worktrees containing a process whose current directory is below the checkout;
- delegates removal to `wt remove --foreground` so Worktrunk checks dirty worktrees and branch integration;
- never passes Worktrunk's `--force`, `--force-delete`, or `--reap` flags;
- closes stale Herdr workspaces or legacy panes after successful removal;
- logs every fetch, candidate, skip, refusal, and removal through Herdr's plugin log.

Automatic event handling is **preview-only by default**. Enable removal only after reviewing the preview output.

## Requirements

Installed release archives contain a native binary; Python and a Rust toolchain are not required.

- Herdr 0.9.0 or newer
- [Worktrunk](https://worktrunk.dev) (`wt`) on `PATH`
- Git
- macOS or Linux (`lsof` is used for process detection on macOS; `/proc` is used on Linux)

Precompiled release archives are produced for:

- Apple Silicon and Intel macOS;
- ARM64 and x86-64 Linux.

## Install a release

Download the archive matching the host from the GitHub release, extract it, and link the extracted plugin directory:

```sh
tar -xzf worktree-gc-<target>.tar.gz
herdr plugin link "$PWD/worktree-gc-<target>"
```

Each archive places the native executable at `bin/worktree-gc`, which is the path used by the plugin manifest.

## Install for development

Building from source requires [mise](https://mise.jdx.dev/), which installs the pinned Rust toolchain from `mise.toml`. Build and place the host binary where the manifest expects it, then link the plugin:

```sh
mise install
mise run install-dev
herdr plugin link "$PWD"
```

Re-run `mise run install-dev` after changing Rust code. Relink after changing `herdr-plugin.toml`:

```sh
herdr plugin unlink worktree-gc
herdr plugin link "$PWD"
```

## Preview and clean up

Preview performs a fresh fetch and never removes anything:

```sh
herdr plugin action invoke worktree-gc.preview
```

Run one explicit cleanup pass:

```sh
herdr plugin action invoke worktree-gc.cleanup
```

Inspect decisions and failures:

```sh
herdr plugin log list --plugin worktree-gc
```

## Configuration

Find the plugin's managed configuration directory:

```sh
config_dir=$(herdr plugin config-dir worktree-gc)
mkdir -p "$config_dir"
cp config.example.toml "$config_dir/config.toml"
```

Available settings:

```toml
# Let startup and workspace events remove candidates. Default: false.
auto_remove = false

# Minimum time between successful fetches of the same repository.
debounce_seconds = 300

# Abort a repository pass if fetch takes longer than this.
fetch_timeout_seconds = 60

# Skip worktrees containing processes. Keep this enabled.
check_processes = true
```

Set `auto_remove = true` only after the preview log matches your intended workflow. Changes are read on every invocation and do not require relinking the plugin.

## What counts as a candidate?

A candidate is a non-main linked worktree whose local branch has a configured upstream ref that no longer exists after a successful prune. A missing upstream is only a signal to ask Worktrunk to remove the worktree; Worktrunk remains the authority on whether the checkout is clean and whether deleting the branch is safe. An unintegrated branch may be retained, but it is never force-deleted.

Repositories are derived from Herdr workspace and pane metadata, plus the final snapshot in a `workspace.closed` event. There is intentionally no manually maintained repository registry.

## Development

Run the test, formatting, and Clippy pedantic checks with:

```sh
mise run check
```

The Clippy task runs all targets with warnings denied and `clippy::pedantic` enabled.

Build a release archive for an installed Rust target with:

```sh
mise run package
```

Pass an explicit Rust target when cross-compiling, for example `mise run package -- aarch64-unknown-linux-gnu`.

## Releases

[release-plz](https://release-plz.dev/) manages versions, `CHANGELOG.md`, tags, and GitHub Releases from Conventional Commits. It runs in Git-only mode, and `publish = false` provides an additional guard against publishing to crates.io. Pushes to `main` that change Rust sources, Cargo manifests, or the plugin manifest update a release pull request when they contain a releasable `feat`, `fix`, `perf`, or `revert` commit. Documentation, tests, refactors, build changes, CI changes, and chores do not cut releases by themselves.

Merging the release pull request creates the GitHub Release. The release workflow then builds all four supported targets and attaches their archives to that release. Do not create release tags manually.

## License

Licensed under the [MIT License](LICENSE).
