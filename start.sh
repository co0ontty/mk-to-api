#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

export PATH="${HOME}/.cargo/bin:${PATH}"
BIN_DIR="${MK2API_BIN_DIR:-$HOME/.local/bin}"
DEST="$BIN_DIR/mk2api"
SOURCE="${DIRECT_GATEWAY_BIN:-$SCRIPT_DIR/target/release/mk2api}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found; install Rust first: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  exit 1
fi

if [ ! -x "$SOURCE" ] || [ src/main.rs -nt "$SOURCE" ] || [ src/cli.rs -nt "$SOURCE" ] || [ src/clients.rs -nt "$SOURCE" ] || [ src/anthropic.rs -nt "$SOURCE" ] || [ Cargo.toml -nt "$SOURCE" ]; then
  echo "building mk2api"
  cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"
fi

mkdir -p "$BIN_DIR"
install -m 755 "$SOURCE" "$DEST"
case ":${PATH}:" in
  *":$BIN_DIR:"*) ;;
  *) echo "note: add $BIN_DIR to PATH to use mk2api directly" >&2 ;;
esac

exec "$DEST" "$@"
