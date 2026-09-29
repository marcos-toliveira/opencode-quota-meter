# opencode-quota-meter

Medidor de cotas do **OpenCode Go** por modelo — janelas rolantes de **5h / 7d / 30d**,
com ajuste de pico para modelos DeepSeek. Saída para CLI, JSON, TUI e widget do
[tclock](https://github.com/akitaonrails/clock-tui).

> **English:** single-file, dependency-free Python 3 utility that reads your local OpenCode v2
> database (read-only) and shows per-model OpenCode Go quota windows (5h/7d/30d), with peak-hour
> pricing doubled for DeepSeek models. Includes a curses TUI and a compact text mode for status
> bars/tclock. MIT.

```text
$ quota-meter
OC Go · 29/09 00:37
DeepSeek 4.1F 5h █░░░░░░░   11%  7d ████████  113%  30d █████░░░   58% ⚠
MiMo 2.6F     5h ░░░░░░░░    0%  7d ░░░░░░░░    0%  30d ░░░░░░░░    0%
```

## Requisitos

- **Python 3.8+**
- **OpenCode v2** com dados locais (o medidor lê o `opencode.db`, somente leitura)
- TUI: `curses` (POSIX). No Windows: `pip install windows-curses` (modos texto/JSON funcionam sem)

## Instalação

```sh
# um comando (baixa o script):
curl -fsSL https://raw.githubusercontent.com/marcos-toliveira/opencode-quota-meter/main/install.sh | sh

# ou, dentro do checkout do repo:
./install.sh          # instala em ~/.local/bin (use PREFIX=/usr/local ./install.sh p/ outro destino)
```

## Uso

| Comando | Saída |
|---|---|
| `quota-meter` | resumo compacto multi-linha — ideal p/ widget do tclock |
| `quota-meter --line` | resumo em uma linha |
| `quota-meter --details` | tabela com $ usado/teto por janela |
| `quota-meter --json` | JSON (scriptável; `--indent` formata) |
| `quota-meter --tui` | interface interativa com a **tabela completa** ($ usado/teto por janela; `q` sair · `r` atualizar · `a` ajuste de pico). Adaptativa: ≥118 colunas mostra teto e valores completos; telas estreitas mostram versão resumida |
| `quota-meter --watch 60` | repete o modo texto a cada 60s |

Opções: `--flat` (não ajusta pico) · `--bar N` · `--no-color` · `--db PATH` · `--config PATH` · `--version`.

O banco do OpenCode é resolvido automaticamente:
`--db` → `QUOTA_METER_DB` → `opencode debug paths db` → caminhos padrão (XDG/Windows).

## Configuração (limites do seu plano)

O medidor carrega `~/.config/quota-meter.json` por cima dos defaults (plano **Go**).
Ajuste para o seu plano/valores vigentes (confira <https://opencode.ai/docs/go>):

```json
{
  "limits": { "deepseek-v4.1-flash": 120, "mimo-v2.6-flash": 120 },
  "peak_doubled": ["deepseek-v4.1-flash", "deepseek-v4-flash"],
  "short_names": { "deepseek-v4.1-flash": "DeepSeek 4.1F" }
}
```

Veja também `examples/quota-meter.json`.

## tclock (widget)

```toml
# Quotas OpenCode Go (medidor — quota-meter)
[[clock.widgets]]
title = "Quotas OpenCode Go"
command = ["quota-meter"]
refresh_secs = 120
[[clock.widgets.popup_actions]]
key = "m"
label = "detalhes"
args = ["--details"]
```

## Como funciona / privacidade

- Lê **apenas** o banco local do OpenCode (`session_message`), em modo somente-leitura. Nada sai da sua máquina.
- Custo **ajustado 2×** para modelos DeepSeek em horário de pico (seg–sex, 01–04h e 06–10h UTC),
  porque o cliente grava tarifa off-peak fixa; os tetos exibidos seguem as janelas deslizantes
  do plano (5h = 20%, 7d = 50%, 30d = 100% do limite mensal do modelo).
- Schema interno do OpenCode v2: se uma atualização quebrar a leitura, abra uma issue.

## Licença

MIT — veja [LICENSE](LICENSE).
