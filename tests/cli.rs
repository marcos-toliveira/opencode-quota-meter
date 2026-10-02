//! Testes de integração do binário `quota-meter`.
//!
//! Constroem um banco SQLite temporário com o schema mínimo do OpenCode v2 e um
//! config isolado, e exercitam a CLI ponta a ponta via `std::process::Command`.
//! O relógio é congelado por `QUOTA_METER_NOW_MS`, então as janelas rolantes de
//! 5h/7d/30d são determinísticas. Quando `QUOTA_METER_PARITY_PY` aponta para o
//! script Python original, um teste compara a saída **byte a byte** com ele —
//! com o mesmo banco, o mesmo config e o mesmo instante congelado.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

/// Instante congelado: 2026-10-01T19:57:42Z (quinta-feira).
const FROZEN_MS: i64 = 1_790_884_662_000;
/// 2026-10-01T18:57:42Z — dentro da janela de 5h, fora do horário de pico.
const TS_5H: i64 = 1_790_881_062_000;
/// 2026-10-01T02:00:00Z (quinta) — horário de pico do DeepSeek, fora das 5h.
const TS_PEAK: i64 = 1_790_820_000_000;
/// 2026-10-01T12:00:00Z — fora do pico, dentro de 7d, fora das 5h.
const TS_7D: i64 = 1_790_856_000_000;

const SCHEMA: &str = "create table session_message (\
    id text primary key, session_id text, time_created integer, type text, data text)";

/// Banco + config temporários no diretório do crate (removidos no `Drop`).
struct Fixture {
    db: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(statements: &[String]) -> Self {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let stem = format!(".qm-test-{}-{sequence}", std::process::id());
            let db = directory.join(format!("{stem}.db"));
            let config = directory.join(format!("{stem}.json"));
            if db.exists() || config.exists() {
                continue;
            }
            let script = format!("{}; {}", SCHEMA, statements.join("; "));
            let status = Command::new("sqlite3")
                .arg(&db)
                .arg(&script)
                .status()
                .expect("sqlite3 deve estar disponível para os testes");
            assert!(status.success(), "não foi possível criar o fixture");
            fs::write(&config, CONFIG).expect("não foi possível escrever o config de teste");
            return Self { db, config };
        }
    }

    fn db(&self) -> &str {
        self.db.to_str().unwrap()
    }

    fn config(&self) -> &str {
        self.config.to_str().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db);
        let _ = fs::remove_file(&self.config);
    }
}

/// Config isolado: tetos do plano Go, pico só no DeepSeek e nomes curtos.
const CONFIG: &str = r#"{
  "limits": { "deepseek-v4.1-flash": 60, "mimo-v2.6-flash": 60 },
  "peak_doubled": ["deepseek-v4.1-flash"],
  "short_names": { "deepseek-v4.1-flash": "DeepSeek 4.1F", "mimo-v2.6-flash": "MiMo 2.6F" }
}"#;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_quota-meter")
}

/// Roda o binário Rust com o fixture e o relógio congelados.
fn run(fixture: &Fixture, arguments: &[&str]) -> Output {
    let mut command = Command::new(binary());
    command
        .env("QUOTA_METER_DB", fixture.db())
        .env("QUOTA_METER_NOW_MS", FROZEN_MS.to_string())
        .env("TZ", "America/Sao_Paulo")
        .env("TERM", "dumb")
        .args(["--config", fixture.config()])
        .args(arguments);
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn assistant(id: &str, created: i64, model: &str, cost: f64) -> String {
    let data = format!(
        "{{\"model\": {{\"providerID\": \"opencode-go\", \"id\": \"{model}\"}}, \
         \"time\": {{\"created\": {created}}}, \"cost\": {cost}}}"
    );
    format!(
        "insert into session_message values ('{id}', 'ses_fixture01', {created}, 'assistant', '{data}')"
    )
}

/// Fixture padrão (relógio congelado em `FROZEN_MS`):
/// - DeepSeek: 2.4 dentro das 5h + 1.0 em pico (→ 2.0) dentro de 7d + 1.0 dentro de 7d.
/// - MiMo: 0.5 dentro das 5h.
/// - Uma linha de usuário e uma de JSON inválido que devem ser ignoradas.
fn fixture_padrao() -> Fixture {
    Fixture::new(&[
        assistant("msg_a1", TS_5H, "deepseek-v4.1-flash", 2.4),
        assistant("msg_a2", TS_PEAK, "deepseek-v4.1-flash", 1.0),
        assistant("msg_a3", TS_7D, "deepseek-v4.1-flash", 1.0),
        assistant("msg_a4", TS_5H, "mimo-v2.6-flash", 0.5),
        format!(
            "insert into session_message values ('msg_u1', 'ses_fixture01', {FROZEN_MS}, 'user', '{{\"text\": \"oi\"}}')"
        ),
        format!(
            "insert into session_message values ('msg_bad', 'ses_fixture01', {FROZEN_MS}, 'assistant', 'not-json')"
        ),
    ])
}

#[test]
fn version_imprime_string_canonica() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output).trim(),
        format!("quota-meter {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn line_resume_pct_por_janela() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--line", "--no-official"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    // 5h: 2.4/12 = 20%; 7d: (2.4 + 2.0 pico + 1.0)/30 = 18%; 30d: 5.4/60 = 9%.
    assert!(text.starts_with("OC Go ⟫ 5h máx 20%"), "{text}");
    assert!(text.contains("7d máx 18% (DeepSeek 4.1F)"), "{text}");
    assert!(text.contains("30d máx 9%"), "{text}");
    assert!(
        text.ends_with('\n'),
        "saída deve terminar em newline: {text:?}"
    );
}

#[test]
fn details_mostra_custo_teto_e_status() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--details", "--no-official"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.contains("OpenCode Go — quotas por modelo"), "{text}");
    assert!(text.contains("DeepSeek 4.1F"), "{text}");
    // Custo ajustado pelo pico (2×) e tetos do config.
    assert!(text.contains("$2.40/$12.00"), "{text}");
    assert!(text.contains("$5.40/$30.00"), "{text}");
    assert!(text.contains("$5.40/$60.00"), "{text}");
    assert!(text.contains("$0.500/$12.00"), "{text}");
    assert!(text.contains("ok"), "{text}");
}

#[test]
fn detalhes_reqs_conta_uma_linha_por_mensagem() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--details", "--no-official"]);
    let text = stdout(&output);
    let linhas: Vec<&str> = text.lines().collect();
    let deepseek = linhas
        .iter()
        .find(|linha| linha.starts_with("DeepSeek 4.1F"))
        .expect("linha do DeepSeek");
    assert!(
        deepseek.ends_with("3  ok") || deepseek.ends_with("3  estourado"),
        "{deepseek}"
    );
    let mimo = linhas
        .iter()
        .find(|linha| linha.starts_with("MiMo 2.6F"))
        .expect("linha do MiMo");
    assert!(mimo.ends_with("1  ok"), "{mimo}");
}

#[test]
fn tclock_imprime_tabela_numerica_por_modelo() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--tclock", "--no-official"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.starts_with("Modelo     5h     7d    30d"), "{text}");
    assert!(text.contains("DeepSeek 4.1F   20%    18%     9%"), "{text}");
    assert!(text.contains("MiMo 2.6F        4%     2%     1%"), "{text}");
    assert!(
        !text.contains("\u{1b}["),
        "tclock não deve emitir ANSI: {text:?}"
    );
}

#[test]
fn json_traz_janelas_ajuste_de_pico_e_floats_do_python() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--json", "--no-official"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        text.starts_with("{\"generated_at\": \"2026-10-01T16:57:42\""),
        "{text}"
    );
    assert!(text.contains("\"peak_adjusted\": true"), "{text}");
    assert!(text.contains("\"limit_usd\": 60"), "{text}");
    assert!(
        text.contains(
            "\"5h\": {\"used_usd\": 2.4, \"cap_usd\": 12.0, \"pct\": 20.0, \"requests\": 1}"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "\"7d\": {\"used_usd\": 5.4, \"cap_usd\": 30.0, \"pct\": 18.0, \"requests\": 3}"
        ),
        "{text}"
    );
    // Float integral sempre com `.0` (json.dumps), inteiro do config sem casa.
    assert!(!text.contains("\"cap_usd\": 12,"), "{text}");
}

#[test]
fn flat_desliga_o_ajuste_de_pico() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--json", "--flat", "--no-official"]);
    let text = stdout(&output);
    assert!(text.contains("\"peak_adjusted\": false"), "{text}");
    // Sem o 2×, o acumulado de 7d cai de 5.4 para 4.4.
    assert!(text.contains("\"used_usd\": 4.4"), "{text}");
}

#[test]
fn banco_ausente_retorna_1_com_orientacao() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--db", "/nao/existe/opencode.db", "--line"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("banco do OpenCode não encontrado"),
        "{:?}",
        stderr(&output)
    );
}

#[test]
fn banco_sem_schema_retorna_1() {
    let fixture = Fixture::new(&[]);
    let vazio = fixture.db().replace(".db", "-vazio.db");
    let status = Command::new("sqlite3")
        .args([&vazio, "create table outra (x text)"])
        .status()
        .unwrap();
    assert!(status.success());
    let output = Command::new(binary())
        .env("QUOTA_METER_NOW_MS", FROZEN_MS.to_string())
        .args(["--config", fixture.config(), "--db", &vazio, "--line"])
        .output()
        .unwrap();
    let _ = fs::remove_file(&vazio);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("schema do OpenCode v2 não encontrado"),
        "{:?}",
        stderr(&output)
    );
}

#[test]
fn tui_recusa_terminal_dumb_com_orientacao() {
    let fixture = fixture_padrao();
    let output = run(&fixture, &["--tui", "--no-official"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("TERM=dumb"),
        "{:?}",
        stderr(&output)
    );
}

/// Paridade byte a byte com o Python original, quando `QUOTA_METER_PARITY_PY`
/// aponta para o script `quota-meter`.
///
/// O relógio do Python não tem costura por variável de ambiente, então o teste
/// injeta o mesmo instante congelado antes de executá-lo (`time.time`) — assim
/// `generated_at`, as janelas rolantes e o ajuste de pico ficam idênticos.
#[test]
fn paridade_com_python_quando_disponivel() {
    let Ok(python_script) = std::env::var("QUOTA_METER_PARITY_PY") else {
        eprintln!("QUOTA_METER_PARITY_PY não definido — pulando teste de paridade");
        return;
    };
    let fixture = fixture_padrao();
    let wrapper = format!(
        "import runpy, sys, time\n\
         time.time = lambda: {seconds}\n\
         sys.argv = ['quota-meter'] + sys.argv[1:]\n\
         runpy.run_path({script:?}, run_name='__main__')\n",
        seconds = FROZEN_MS as f64 / 1000.0,
        script = python_script,
    );
    let casos: &[&[&str]] = &[
        &[],
        &["--line"],
        &["--details"],
        &["--tclock"],
        &["--json"],
        &["--json", "--flat"],
        &["--details", "--bar", "12"],
    ];
    for caso in casos {
        let mut flags = vec![
            "--no-official",
            "--db",
            fixture.db(),
            "--config",
            fixture.config(),
        ];
        flags.extend_from_slice(caso);
        let esperado = Command::new("python3")
            .env("TZ", "America/Sao_Paulo")
            .args(["-c", &wrapper])
            .args(&flags)
            .output()
            .expect("python3 de paridade deve rodar");
        let obtido = {
            let mut command = Command::new(binary());
            command
                .env("QUOTA_METER_NOW_MS", FROZEN_MS.to_string())
                .env("TZ", "America/Sao_Paulo")
                .env("TERM", "dumb")
                .args(&flags);
            command.output().unwrap()
        };
        assert_eq!(
            stdout(&obtido),
            stdout(&esperado),
            "stdout divergente para {caso:?}"
        );
        assert_eq!(
            obtido.status.code(),
            esperado.status.code(),
            "código de saída divergente para {caso:?}"
        );
    }
}
