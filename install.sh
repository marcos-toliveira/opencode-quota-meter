#!/bin/sh
# Instala o quota-meter em ~/.local/bin (ou $PREFIX/bin).
set -eu
RAW="https://raw.githubusercontent.com/marcos-toliveira/opencode-quota-meter/main/quota-meter"
DEST="${PREFIX:-$HOME/.local/bin}"
DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

mkdir -p "$DEST"
if [ -f "$DIR/quota-meter" ]; then
  install -m 755 "$DIR/quota-meter" "$DEST/quota-meter"
else
  command -v curl >/dev/null 2>&1 || { echo "quota-meter: preciso de curl para baixar o script" >&2; exit 1; }
  curl -fsSL "$RAW" -o "$DEST/quota-meter"
  chmod 755 "$DEST/quota-meter"
fi

echo "quota-meter instalado em $DEST/quota-meter"
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "aviso: $DEST não está no PATH — adicione ao seu shell." ;;
esac
