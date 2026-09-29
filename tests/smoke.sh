#!/bin/sh
# Smoke test do quota-meter — roda sem tocar em nada (somente leitura).
set -eu
DIR="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
BIN="$DIR/quota-meter"

echo "1) sintaxe"
python3 -m py_compile "$BIN"

echo "2) --line"
python3 "$BIN" --line | grep -q "OC Go" && echo "   ok"

echo "3) --json"
python3 "$BIN" --json | python3 -c "import json,sys; d=json.load(sys.stdin); assert 'models' in d and isinstance(d['models'], list); print('   models:', len(d['models']))"

echo "4) --details"
python3 "$BIN" --details | head -3 >/dev/null && echo "   ok"

echo "5) --version"
python3 "$BIN" --version | grep -q "quota-meter" && echo "   ok"

echo "6) banco inexistente (falha amigável, exit != 0)"
if QUOTA_METER_DB=/nonexistent/opencode.db python3 "$BIN" --line 2>/dev/null; then
  echo "   FALHOU: deveria sair com erro"; exit 1
else
  echo "   ok"
fi

echo "7) --tclock (sem rede)"
python3 "$BIN" --tclock --no-official | grep -q "Modelo" && echo "   ok"

echo "smoke ok"
