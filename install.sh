#!/bin/sh
# Instala o quota-meter em ~/.local/bin (ou $PREFIX/bin).
#
# Esta é a implementação em Rust (zero dependências além da std). O script
# compila o binário e instala o artefato de `target/release/`. O script Python
# original (`quota-meter`) é mantido no repo apenas como referência de paridade.
set -eu
DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
DEST="${PREFIX:-$HOME/.local/bin}"
BIN="$DIR/target/release/quota-meter"

if [ ! -x "$BIN" ]; then
  echo "Compilando (cargo build --release)..."
  cargo build --release --manifest-path "$DIR/Cargo.toml"
fi

if [ ! -x "$BIN" ]; then
  echo "quota-meter: erro: $BIN não encontrado após a compilação." >&2
  exit 1
fi

mkdir -p "$DEST"
install -m 755 "$BIN" "$DEST/quota-meter"

echo "quota-meter instalado em $DEST/quota-meter"
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "aviso: $DEST não está no PATH — adicione ao seu shell." ;;
esac
