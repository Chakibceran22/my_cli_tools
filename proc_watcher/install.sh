#!/usr/bin/env bash
# Builds procwatch in release mode and installs it to ~/.local/bin as
# `procwatch` and `pw` (macOS only).
#
#   ./install.sh            build and install
#   JOBS=4 ./install.sh     allow more parallel compile jobs (default 2)
#
# Builds are incremental: the existing target/ directory is reused.

set -euo pipefail

cd "$(dirname "$0")"

dest="$HOME/.local/bin"
jobs="${JOBS:-2}"
binary="target/release/procwatch"

[[ "$(uname -s)" == "Darwin" ]] || { echo "error: procwatch is a macOS tool" >&2; exit 1; }
command -v cargo >/dev/null || { echo "error: cargo not found, install Rust from https://rustup.rs" >&2; exit 1; }

echo "==> Building release with $jobs jobs"
cargo build --release --locked -j "$jobs"

echo "==> Installing to $dest"
mkdir -p "$dest"
# install(1) unlinks the old file first, so a running copy is never patched in
# place (which would invalidate its macOS code signature).
install -m 0755 "$binary" "$dest/procwatch"

size=$(du -h "$dest/procwatch" | cut -f1 | tr -d ' ')
echo "==> Installed $dest/procwatch ($size)"

# Short name. A symlink rather than a shell alias, so it also works in pipes
# and scripts. Never replaces a `pw` that is not ours.
if [[ -e "$dest/pw" && "$(readlink "$dest/pw")" != "procwatch" ]]; then
  echo "note: $dest/pw already exists and is not procwatch; left it alone"
else
  ln -sfn procwatch "$dest/pw"
  echo "==> Linked $dest/pw -> procwatch"
fi

case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "note: $dest is not on your PATH. Add this to ~/.zshrc:"
     echo "      export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
