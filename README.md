# opencode-quota-meter

Medidor de cotas do **OpenCode Go** por modelo — janelas rolantes de **5h / 7d / 30d**,
com ajuste de pico para modelos DeepSeek. Saída para CLI, JSON, TUI e widget do
[tclock](https://github.com/akitaonrails/clock-tui).

> **English:** single-file, dependency-free Rust utility (std only, edition 2024) that reads your
> local OpenCode v2 database (read-only) and shows per-model OpenCode Go quota windows (5h/7d/30d),
> with peak-hour pricing doubled for DeepSeek models. The interactive TUI uses `stty` + ANSI
> (no curses), and there is a compact text mode for status bars/tclock. The original Python
> implementation is kept in the repo as the parity reference. MIT.

```text
$ quota-meter
OC Go · 29/09 00:37
DeepSeek 4.1F 5h █░░░░░░░   11%  7d ████████  113%  30d █████░░░   58% ⚠
MiMo 2.6F     5h ░░░░░░░░    0%  7d ░░░░░░░░    0%  30d ░░░░░░░░    0%
```

## Requisitos

- **Rust 1.85+** (edition 2024) para compilar — o utilitário é escrito em **Rust com zero
  dependências além da std**
- **sqlite3** no `PATH` (a leitura do banco é feita por subprocesso `sqlite3 -readonly -json`)
- **OpenCode v2** com dados locais (o medidor lê o `opencode.db`, somente leitura)
- TUI: requer terminal POSIX (`stty`). Os modos texto/JSON funcionam em qualquer plataforma com `sqlite3`

## Instalação

```sh
git clone https://github.com/marcos-toliveira/opencode-quota-meter
cd opencode-quota-meter
cargo build --release   # binário zero-dependência (só a std)
./install.sh            # copia target/release/quota-meter para ~/.local/bin
```

`install.sh` compila e instala o artefato de `target/release/` (use
`PREFIX=/usr/local ./install.sh` para outro destino).

## Uso

| Comando | Saída |
|---|---|
| `quota-meter` | resumo compacto multi-linha — ideal p/ widget do tclock |
| `quota-meter --line` | resumo em uma linha |
| `quota-meter --tclock` | tabela numérica compacta para widget do tclock (%, por modelo) + linha oficial do ai-usagebar quando disponível |
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

## Contas múltiplas (oficiais isolados)

Para rodar mais de uma conta da mesma integração, aponte o medidor para um config alternativo do
ai-usagebar e isole o cache dele:

```sh
#!/usr/bin/env bash
# ex.: ~/.local/bin/quota-meter-2
export QUOTA_METER_DB="$HOME/.opencode-go2/data/opencode/opencode.db"
export QUOTA_METER_AUB_CONFIG="$HOME/.config/ai-usagebar/config2.toml"
export QUOTA_METER_AUB_CACHE_HOME="$HOME/.cache/ai-usagebar-2"
exec quota-meter "$@"
```

- `QUOTA_METER_AUB_CONFIG` → repassado como `--config` ao ai-usagebar.
- `QUOTA_METER_AUB_CACHE_HOME` → vira `XDG_CACHE_HOME` do processo (o `usage` não aceita `--cache-dir`).

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
- **Janelas:** 5h é rolante; **7d e 30d seguem os períodos oficiais** (a semana reinicia na data do
  reset; o mês no ciclo da assinatura), derivados dos resets do `ai-usagebar` quando disponível —
  sem isso, uma janela rolante **superestima** o consumo. Sem `ai-usagebar`, caem para rolante.
- Custo **ajustado 2×** para modelos DeepSeek em horário de pico (seg–sex, 01–04h e 06–10h UTC),
  porque o cliente grava tarifa off-peak fixa. A linha **"Oficial" (API)** é a referência; a tabela
  por modelo é uma estimativa local (costuma ficar ~20–30% abaixo do oficial).
- Schema interno do OpenCode v2: se uma atualização quebrar a leitura, abra uma issue.

## Testes

```bash
cargo test
# paridade byte a byte com o Python original (opcional):
QUOTA_METER_PARITY_PY="$PWD/quota-meter" cargo test --test cli
```

- **Unitários** (`src/main.rs`): janelas de pico, calendário civil, ISO-8601, `rel_reset`,
  formatação decimal exata do Python (HALF-EVEN), barra/percentuais e o parser JSON/SQLite.
- **Integração** (`tests/cli.rs`): constrói um banco SQLite temporário com `sqlite3` e um config
  isolado, com o relógio congelado por `QUOTA_METER_NOW_MS`; cobre `--line`, `--details`,
  `--tclock`, `--json`/`--flat`, `--version`, banco ausente ou sem schema e a recusa da TUI em
  `TERM=dumb`. Com `QUOTA_METER_PARITY_PY` definido, compara as saídas **byte a byte** com o
  script Python no mesmo banco, config e instante.

## Licença

MIT — veja [LICENSE](LICENSE).
