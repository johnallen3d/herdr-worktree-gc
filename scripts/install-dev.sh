#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo build --manifest-path "$root/Cargo.toml" --release --locked
mkdir -p "$root/bin"
cp "$root/target/release/worktree-gc" "$root/bin/worktree-gc"
chmod 755 "$root/bin/worktree-gc"
printf 'Installed development binary at %s/bin/worktree-gc\n' "$root"
