#!/bin/sh
set -eu

case $# in
  0) target=$(rustc -vV | sed -n 's/^host: //p') ;;
  1) target=$1 ;;
  *)
    echo "usage: $0 [rust-target]" >&2
    exit 2
    ;;
esac
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
name="worktree-gc-$target"
stage="$root/dist/$name"
archive="$root/dist/$name.tar.gz"

cargo build --manifest-path "$root/Cargo.toml" --release --locked --target "$target"
rm -rf "$stage"
mkdir -p "$stage/bin"
cp "$root/target/$target/release/worktree-gc" "$stage/bin/worktree-gc"
chmod 755 "$stage/bin/worktree-gc"
cp "$root/herdr-plugin.toml" "$root/config.example.toml" "$root/README.md" "$stage/"

tar -C "$root/dist" -czf "$archive" "$name"
echo "$archive"
