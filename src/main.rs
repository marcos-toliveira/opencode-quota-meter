// quota-meter — medidor de cotas do OpenCode Go por modelo (janelas rolantes 5h/7d/30d).
//
// Licença: MIT · https://github.com/marcos-toliveira/opencode-quota-meter
//
// Porte Rust ZERO-dependência (edition 2024) do utilitário original em Python,
// com paridade de CLI, JSON e TUI. O banco local do OpenCode é lido SOMENTE em
// modo leitura via subprocesso `sqlite3 -readonly -json`; as métricas oficiais
// vêm do `ai-usagebar usage --json`; a TUI usa `stty` + ANSI (sem crates).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = "1.2.1";

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31;1m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

/// Janelas rolantes: rótulo, duração em ms e fração do teto mensal.
const WINDOWS: [(&str, i64, f64); 3] = [
    ("5h", 5 * 3_600_000, 0.20),
    ("7d", 7 * 86_400_000, 0.50),
    ("30d", 30 * 86_400_000, 1.00),
];

/// Defaults do plano OpenCode Go (podem ser sobrescritos pelo config JSON).
const DEFAULT_LIMITS: [(&str, f64); 28] = [
    ("deepseek-v4.1-flash", 60.0),
    ("deepseek-v4-flash", 30.0),
    ("deepseek-v4-flash-vision-exp", 15.0),
    ("deepseek-v4-pro", 15.0),
    ("mimo-v2.6-flash", 60.0),
    ("mimo-v2.5", 60.0),
    ("mimo-v2.6-pro", 15.0),
    ("mimo-v2.5-pro", 15.0),
    ("glm-5.3-flash", 60.0),
    ("glm-5.3", 15.0),
    ("glm-5.2", 60.0),
    ("minimax-m3", 60.0),
    ("minimax-m2.7", 60.0),
    ("qwen3.7-plus", 60.0),
    ("qwen3.8-flash", 30.0),
    ("qwen3.8-max", 15.0),
    ("kimi-k2.7-code", 60.0),
    ("kimi-k2.6", 60.0),
    ("kimi-k3", 15.0),
    ("gpt-6-luna", 15.0),
    ("gpt-5.6-luna", 15.0),
    ("grok-4.7", 15.0),
    ("grok-4.6", 15.0),
    ("muse-spark-1.3-contributor", 60.0),
    ("muse-spark-1.2-contributor", 60.0),
    ("longcat-2.0", 60.0),
    ("hy3", 60.0),
    ("hy4-preview", 30.0),
];

/// Modelos sem teto (limite `null` no config original): valor sentinela `None`.
const UNLIMITED_MODELS: [&str; 2] = ["space-bunny-free", "longcat-2.5-preview-free"];

const DEFAULT_SHORT: [(&str, &str); 28] = [
    ("deepseek-v4.1-flash", "DeepSeek 4.1F"),
    ("deepseek-v4-flash", "DeepSeek 4F"),
    ("deepseek-v4-flash-vision-exp", "DS 4F Vision"),
    ("deepseek-v4-pro", "DeepSeek 4 Pro"),
    ("mimo-v2.6-flash", "MiMo 2.6F"),
    ("mimo-v2.5", "MiMo 2.5"),
    ("glm-5.3-flash", "GLM 5.3F"),
    ("glm-5.3", "GLM 5.3"),
    ("glm-5.2", "GLM 5.2"),
    ("minimax-m3", "MiniMax M3"),
    ("minimax-m2.7", "MiniMax 2.7"),
    ("qwen3.7-plus", "Qwen 3.7+"),
    ("qwen3.8-flash", "Qwen 3.8F"),
    ("qwen3.8-max", "Qwen 3.8 Max"),
    ("kimi-k2.7-code", "Kimi 2.7C"),
    ("kimi-k2.6", "Kimi 2.6"),
    ("kimi-k3", "Kimi K3"),
    ("gpt-6-luna", "GPT-6 Luna"),
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
    ("grok-4.7", "Grok 4.7"),
    ("grok-4.6", "Grok 4.6"),
    ("muse-spark-1.3-contributor", "Muse 1.3C"),
    ("muse-spark-1.2-contributor", "Muse 1.2C"),
    ("longcat-2.0", "LongCat 2.0"),
    ("hy3", "Hy3"),
    ("hy4-preview", "Hy4 prev"),
    ("space-bunny-free", "Space Bunny"),
    ("longcat-2.5-preview-free", "LongCat Free"),
];

const DEFAULT_PEAK_DOUBLED: [&str; 4] = [
    "deepseek-v4.1-flash",
    "deepseek-v4-flash",
    "deepseek-v4-flash-vision-exp",
    "deepseek-v4-pro",
];

/// Configuração efetiva após mesclar defaults + arquivo JSON.
#[derive(Clone)]
struct Config {
    limits: BTreeMap<String, Option<f64>>,
    short: BTreeMap<String, String>,
    peak_doubled: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut limits = BTreeMap::new();
        for (id, value) in DEFAULT_LIMITS {
            limits.insert(id.to_owned(), Some(value));
        }
        for id in UNLIMITED_MODELS {
            limits.insert(id.to_owned(), None);
        }
        let mut short = BTreeMap::new();
        for (id, name) in DEFAULT_SHORT {
            short.insert(id.to_owned(), name.to_owned());
        }
        Self {
            limits,
            short,
            peak_doubled: DEFAULT_PEAK_DOUBLED
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("quota-meter: {message}");
    std::process::exit(1);
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(relative);
    }
    PathBuf::from(path)
}

fn default_config_path() -> PathBuf {
    match env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/quota-meter.json"),
        None => PathBuf::from(".config/quota-meter.json"),
    }
}

/// `--db` > `QUOTA_METER_DB` > `opencode debug paths db` > fallback XDG/Windows.
fn resolve_db(explicit: Option<&str>) -> PathBuf {
    if let Some(path) = explicit
        && !path.is_empty()
    {
        return expand_home(path);
    }
    if let Some(path) = env::var_os("QUOTA_METER_DB")
        && !path.is_empty()
    {
        return expand_home(&path.to_string_lossy());
    }
    if let Some(exe) = which("opencode")
        && let Ok(output) = Command::new(exe)
            .args(["debug", "paths", "db"])
            .stdin(Stdio::null())
            .output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.trim().lines().rev() {
            let candidate = line.trim();
            if !candidate.is_empty()
                && Path::new(candidate).is_absolute()
                && Path::new(candidate).exists()
            {
                return PathBuf::from(candidate);
            }
        }
    }
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let xdg_data = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let candidates = [
        xdg_data.join("opencode/opencode.db"),
        env::var_os("LOCALAPPDATA")
            .map(|value| PathBuf::from(value).join("opencode/opencode.db"))
            .unwrap_or_default(),
        env::var_os("APPDATA")
            .map(|value| PathBuf::from(value).join("opencode/opencode.db"))
            .unwrap_or_default(),
    ];
    for candidate in &candidates {
        if !candidate.as_os_str().is_empty() && candidate.exists() {
            return candidate.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from("opencode.db"))
}

/// `shutil.which` simplificado: procura o executável no PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn now_ms() -> i64 {
    if let Some(value) = env::var_os("QUOTA_METER_NOW_MS")
        && let Ok(parsed) = value.to_string_lossy().parse::<i64>()
    {
        return parsed;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn now_secs() -> f64 {
    if let Some(value) = env::var_os("QUOTA_METER_NOW_MS")
        && let Ok(parsed) = value.to_string_lossy().parse::<i64>()
    {
        return parsed as f64 / 1000.0;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// Carrega e mescla o config JSON sobre os defaults. Aviso em stderr se inválido.
fn load_config(path: &Path) -> Config {
    let mut config = Config::default();
    let Ok(raw) = fs::read_to_string(path) else {
        return config;
    };
    let Ok(document) = parse_json(&raw) else {
        eprintln!(
            "quota-meter: aviso: config inválido em {}: JSON inválido",
            path.display()
        );
        return config;
    };

    if let Some(limits) = document.get("limits").and_then(JsonValue::as_object) {
        for (id, value) in limits {
            match value {
                JsonValue::Null => {
                    config.limits.insert(id.clone(), None);
                }
                JsonValue::Number(number) => {
                    config.limits.insert(id.clone(), Some(*number));
                }
                _ => {}
            }
        }
    }
    if let Some(short) = document.get("short_names").and_then(JsonValue::as_object) {
        for (id, value) in short {
            if let Some(text) = value.as_str() {
                config.short.insert(id.clone(), text.to_owned());
            }
        }
    }
    if let Some(peak) = document.get("peak_doubled").and_then(JsonValue::as_array) {
        config.peak_doubled = peak
            .iter()
            .filter_map(JsonValue::as_str)
            .map(str::to_owned)
            .collect();
    }
    config
}

// ─────────────────────────────────────────────────────────────────────────────
// Parser JSON mínimo (sem crates) — mesma técnica do painel da casa.
// ─────────────────────────────────────────────────────────────────────────────

const MAX_JSON_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            Self::Object(values) => values.get(key),
            _ => None,
        }
    }

    fn as_object(&self) -> Option<&BTreeMap<String, JsonValue>> {
        match self {
            Self::Object(values) => Some(values),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

struct JsonParser<'a> {
    input: &'a [u8],
    cursor: usize,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            cursor: 0,
            depth: 0,
        }
    }

    fn parse(mut self) -> Result<JsonValue, String> {
        self.skip_whitespace();
        let value = self.parse_value()?;
        self.skip_whitespace();
        if self.cursor != self.input.len() {
            return Err(self.error("conteúdo inesperado após o valor JSON"));
        }
        Ok(value)
    }

    fn parse_value(&mut self) -> Result<JsonValue, String> {
        self.skip_whitespace();
        if matches!(self.peek(), Some(b'{' | b'[')) {
            if self.depth >= MAX_JSON_DEPTH {
                return Err(self.error("JSON excedeu o limite de aninhamento"));
            }
            self.depth += 1;
            let value = if self.peek() == Some(b'{') {
                self.parse_object()
            } else {
                self.parse_array()
            };
            self.depth -= 1;
            return value;
        }
        match self.peek() {
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => {
                self.consume_literal(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.consume_literal(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'n') => {
                self.consume_literal(b"null")?;
                Ok(JsonValue::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("valor JSON inválido")),
            None => Err(self.error("fim inesperado do arquivo JSON")),
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, String> {
        self.expect(b'{')?;
        self.skip_whitespace();
        let mut values = BTreeMap::new();
        if self.consume_if(b'}') {
            return Ok(JsonValue::Object(values));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("a chave do objeto deve ser uma string"));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.parse_value()?;
            values.insert(key, value);
            self.skip_whitespace();
            if self.consume_if(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Object(values))
    }

    fn parse_array(&mut self) -> Result<JsonValue, String> {
        self.expect(b'[')?;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.consume_if(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value()?);
            self.skip_whitespace();
            if self.consume_if(b']') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Array(values))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut value = String::new();
        let mut segment_start = self.cursor;
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.push_utf8_segment(&mut value, segment_start, self.cursor)?;
                    self.cursor += 1;
                    return Ok(value);
                }
                b'\\' => {
                    self.push_utf8_segment(&mut value, segment_start, self.cursor)?;
                    self.cursor += 1;
                    let escaped = self
                        .peek()
                        .ok_or_else(|| self.error("sequência de escape incompleta"))?;
                    self.cursor += 1;
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{0008}'),
                        b'f' => value.push('\u{000c}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'u' => self.push_unicode_escape(&mut value)?,
                        _ => return Err(self.error("sequência de escape inválida")),
                    }
                    segment_start = self.cursor;
                }
                0..=0x1f => return Err(self.error("caractere de controle dentro de string")),
                _ => self.cursor += 1,
            }
        }
        Err(self.error("string JSON não foi fechada"))
    }

    fn push_utf8_segment(
        &self,
        target: &mut String,
        start: usize,
        end: usize,
    ) -> Result<(), String> {
        let segment = std::str::from_utf8(&self.input[start..end])
            .map_err(|_| self.error("string não contém UTF-8 válido"))?;
        target.push_str(segment);
        Ok(())
    }

    fn push_unicode_escape(&mut self, target: &mut String) -> Result<(), String> {
        let first = self.parse_hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            if self.peek() != Some(b'\\') || self.input.get(self.cursor + 1) != Some(&b'u') {
                return Err(self.error("par substituto Unicode incompleto"));
            }
            self.cursor += 2;
            let second = self.parse_hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(self.error("par substituto Unicode inválido"));
            }
            0x10000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00))
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(self.error("substituto Unicode isolado"));
        } else {
            first as u32
        };
        let character =
            char::from_u32(scalar).ok_or_else(|| self.error("código Unicode inválido"))?;
        target.push(character);
        Ok(())
    }

    fn parse_hex_quad(&mut self) -> Result<u16, String> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self
                .peek()
                .ok_or_else(|| self.error("escape Unicode incompleto"))?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("escape Unicode inválido")),
            };
            value = (value << 4) | u16::from(digit);
            self.cursor += 1;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let start = self.cursor;
        self.consume_if(b'-');
        match self.peek() {
            Some(b'0') => {
                self.cursor += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("número JSON não pode ter zero à esquerda"));
                }
            }
            Some(b'1'..=b'9') => {
                self.cursor += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.cursor += 1;
                }
            }
            _ => return Err(self.error("parte inteira do número inválida")),
        }
        if self.consume_if(b'.') {
            let fraction_start = self.cursor;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
            if self.cursor == fraction_start {
                return Err(self.error("parte decimal do número inválida"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.cursor += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.cursor += 1;
            }
            let exponent_start = self.cursor;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
            if self.cursor == exponent_start {
                return Err(self.error("expoente do número inválido"));
            }
        }
        let text = std::str::from_utf8(&self.input[start..self.cursor])
            .map_err(|_| self.error("número JSON inválido"))?;
        let number = text
            .parse::<f64>()
            .map_err(|_| self.error("número JSON inválido"))?;
        if !number.is_finite() {
            return Err(self.error("número fora do intervalo suportado"));
        }
        Ok(JsonValue::Number(number))
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), String> {
        let end = self.cursor + literal.len();
        if self.input.get(self.cursor..end) == Some(literal) {
            self.cursor = end;
            Ok(())
        } else {
            Err(self.error("literal JSON inválido"))
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.cursor += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.consume_if(expected) {
            Ok(())
        } else {
            Err(self.error(&format!("esperado '{}'", expected as char)))
        }
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.cursor).copied()
    }

    fn error(&self, message: &str) -> String {
        let consumed = String::from_utf8_lossy(&self.input[..self.cursor.min(self.input.len())]);
        let line = consumed.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = consumed
            .rsplit('\n')
            .next()
            .map_or(1, |last_line| last_line.chars().count() + 1);
        format!("{message} na linha {line}, coluna {column}")
    }
}

fn parse_json(input: &str) -> Result<JsonValue, String> {
    JsonParser::new(input).parse()
}

// ─────────────────────────────────────────────────────────────────────────────
// Datas/horas UTC sem crates — algoritmos de calendário civil (Hinnant).
// ─────────────────────────────────────────────────────────────────────────────

/// Converte dias desde 1970-01-01 para (ano, mês, dia) no calendário gregoriano.
/// Algoritmo `civil_from_days` de Howard Hinnant (domínio público).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Converte (ano, mês, dia) para dias desde 1970-01-01 (`days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let d = day as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Componentes de uma data/hora UTC com offset aplicado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DateTimeUtc {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

impl DateTimeUtc {
    /// Segundos desde a época Unix (UTC), ignorando frações de segundo.
    fn timestamp_secs(&self) -> f64 {
        let days = days_from_civil(self.year, self.month, self.day);
        (days * 86_400 + self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64)
            as f64
    }

    fn iso_string(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Converte segundos Unix (UTC) para componentes de data/hora UTC.
fn utc_from_timestamp(seconds: f64) -> DateTimeUtc {
    let mut secs = seconds.floor() as i64;
    let days = secs.div_euclid(86_400);
    secs = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    DateTimeUtc {
        year,
        month,
        day,
        hour: (secs / 3600) as u32,
        minute: ((secs % 3600) / 60) as u32,
        second: (secs % 60) as u32,
    }
}

/// Parser ISO-8601 compatível com `datetime.fromisoformat` (após trocar Z por +00:00).
/// Aceita `YYYY-MM-DD`, separador `T`/`t`/espaço, fração variável e offset `Z`/`±HH:MM`.
fn parse_iso8601(input: &str) -> Option<i64> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let year = parse_fixed(bytes.get(0..4)?)? as i64;
    if bytes.get(4) != Some(&b'-') {
        return None;
    }
    let month = parse_fixed(bytes.get(5..7)?)?;
    if bytes.get(7) != Some(&b'-') {
        return None;
    }
    let day = parse_fixed(bytes.get(8..10)?)?;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }

    let mut cursor = 10;
    let mut hour = 0u32;
    let mut minute = 0u32;
    let mut second = 0u32;
    if let Some(&separator) = bytes.get(cursor)
        && matches!(separator, b'T' | b't' | b' ')
    {
        cursor += 1;
        hour = parse_fixed(bytes.get(cursor..cursor + 2)?)?;
        if bytes.get(cursor + 2) != Some(&b':') {
            return None;
        }
        minute = parse_fixed(bytes.get(cursor + 3..cursor + 5)?)?;
        cursor += 5;
        if bytes.get(cursor) == Some(&b':') {
            second = parse_fixed(bytes.get(cursor + 1..cursor + 3)?)?;
            cursor += 3;
        }
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        if bytes.get(cursor) == Some(&b'.') {
            cursor += 1;
            let fraction_start = cursor;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
            if cursor == fraction_start {
                return None;
            }
        }
    }

    let offset_seconds = match bytes.get(cursor) {
        None => 0i64,
        Some(b'Z' | b'z') => {
            if cursor + 1 != bytes.len() {
                return None;
            }
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            let sign_value = if *sign == b'-' { -1 } else { 1 };
            let rest = bytes.get(cursor + 1..)?;
            let digits: Vec<u8> = rest.iter().copied().filter(|byte| *byte != b':').collect();
            let (offset_hour, offset_minute) = match digits.len() {
                2 => (parse_fixed(&digits)?, 0),
                4 => (parse_fixed(&digits[0..2])?, parse_fixed(&digits[2..4])?),
                _ => return None,
            };
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            sign_value * (offset_hour as i64 * 3600 + offset_minute as i64 * 60)
        }
        Some(_) => return None,
    };

    let local = DateTimeUtc {
        year,
        month,
        day,
        hour,
        minute,
        second,
    };
    Some(local.timestamp_secs() as i64 - offset_seconds)
}

fn parse_fixed(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(byte - b'0');
    }
    Some(value)
}

/// Verdadeiro em horário de pico DeepSeek: seg–sex (UTC), 01–04h ou 06–10h.
fn is_peak(ms: i64) -> bool {
    let seconds = ms as f64 / 1000.0;
    let hour = (seconds.rem_euclid(86_400.0) / 3600.0).floor() as i64;
    let days = (seconds.floor() as i64).div_euclid(86_400);
    // 1970-01-01 foi quinta-feira (weekday 3). Go: weekday = (days + 3) rem_euclid 7.
    let weekday = (days + 3).rem_euclid(7);
    weekday < 5 && ((1..4).contains(&hour) || (6..10).contains(&hour))
}

/// Tempo relativo até um reset ISO-8601 (ex.: '1h 03m'); vazio se inválido/passado.
fn rel_reset(iso: &str) -> String {
    if iso.is_empty() {
        return String::new();
    }
    let Some(target) = parse_iso8601(iso) else {
        return String::new();
    };
    let secs = target - now_secs().floor() as i64;
    if secs <= 0 {
        return "agora".to_owned();
    }
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hours = rem / 3600;
    let minutes = (rem % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

/// `datetime.fromtimestamp(ms/1000).strftime('%d/%m %H:%M')` no fuso local.
fn format_local_minute(ms: i64) -> String {
    let local = local_datetime(ms);
    format!(
        "{:02}/{:02} {:02}:{:02}",
        local.day, local.month, local.hour, local.minute
    )
}

/// Igual, no formato `%d/%m/%Y %H:%M`.
fn format_local_full(ms: i64) -> String {
    let local = local_datetime(ms);
    format!(
        "{:02}/{:02}/{:04} {:02}:{:02}",
        local.day, local.month, local.year, local.hour, local.minute
    )
}

/// Igual, no formato `%d/%m %H:%M:%S` (título da TUI).
fn format_local_seconds(ms: i64) -> String {
    let local = local_datetime(ms);
    format!(
        "{:02}/{:02} {:02}:{:02}:{:02}",
        local.day, local.month, local.hour, local.minute, local.second
    )
}

/// Deslocamento do fuso local em segundos na instante dado, detectado via `date`.
/// Resultado memorizado por processo (o offset não muda durante a execução).
fn local_offset_seconds() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| probe_local_offset().unwrap_or(0))
}

fn probe_local_offset() -> Option<i64> {
    if let Ok(offset) = env::var("QUOTA_METER_TZ_OFFSET")
        && let Ok(parsed) = offset.parse::<i64>()
    {
        return Some(parsed);
    }
    let output = Command::new("date")
        .arg("+%z")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_utc_offset(String::from_utf8_lossy(&output.stdout).trim())
}

fn parse_utc_offset(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 5 {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1i64,
        b'-' => -1i64,
        _ => return None,
    };
    let hours = parse_fixed(&bytes[1..3])? as i64;
    let minutes = parse_fixed(&bytes[3..5])? as i64;
    Some(sign * (hours * 3600 + minutes * 60))
}

fn local_datetime(ms: i64) -> DateTimeUtc {
    utc_from_timestamp(ms as f64 / 1000.0 + local_offset_seconds() as f64)
}

/// `dt.datetime.fromtimestamp(ms/1000).isoformat(timespec="seconds")` (local, sem offset).
fn iso_local_seconds(ms: i64) -> String {
    local_datetime(ms).iso_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Agregação: leitura do opencode.db via subprocesso `sqlite3 -readonly -json`.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ModelUsage {
    id: String,
    name: String,
    limit: Option<f64>,
    used: [f64; 3],
    caps: [Option<f64>; 3],
    pct: [f64; 3],
    reqs: [i64; 3],
    worst: f64,
    status: &'static str,
}

/// Garante que o binário e o schema existem e retorna o caminho validado.
fn open_db(db: &Path) -> PathBuf {
    if !db.exists() {
        fail(&format!(
            "banco do OpenCode não encontrado em {}\n  → instale/use o OpenCode v2, ou aponte com --db / QUOTA_METER_DB.",
            db.display()
        ));
    }
    let exe = which("sqlite3").unwrap_or_else(|| {
        fail("sqlite3 não encontrado no PATH (necessário para ler o banco do OpenCode).")
    });
    let query = "select name from sqlite_master where type='table'";
    let output = Command::new(&exe)
        .arg("-readonly")
        .arg("-json")
        .arg(db)
        .arg(query)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| fail(&format!("não consegui executar sqlite3: {error}")));
    if !output.status.success() {
        fail(&format!(
            "não consegui abrir o banco {}: {}",
            db.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let tables = parse_sqlite_rows(&text);
    let has_session_message = tables.iter().any(|row| {
        row.get("name")
            .and_then(JsonValue::as_str)
            .is_some_and(|name| name == "session_message")
    });
    if !has_session_message {
        fail(&format!(
            "schema do OpenCode v2 não encontrado em {} (tabela session_message ausente). Versão incompatível?",
            db.display()
        ));
    }
    db.to_path_buf()
}

/// As saídas `-json` do sqlite3 podem ser `[]` (vazio) ou um array de objetos.
fn parse_sqlite_rows(text: &str) -> Vec<BTreeMap<String, JsonValue>> {
    match parse_json(text.trim()) {
        Ok(JsonValue::Array(rows)) => rows
            .into_iter()
            .filter_map(|row| match row {
                JsonValue::Object(values) => Some(values),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn run_sqlite_json(db: &Path, query: &str) -> Option<Vec<BTreeMap<String, JsonValue>>> {
    let exe = which("sqlite3")?;
    let output = Command::new(exe)
        .arg("-readonly")
        .arg("-json")
        .arg(db)
        .arg(query)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_sqlite_rows(&String::from_utf8_lossy(&output.stdout)))
}

/// Limite de variáveis por consulta `IN (...)`: o SQLite clássico limita a 999.
const SQLITE_VAR_LIMIT: usize = 900;

/// Seleciona os objetos `data` paginando por chave primária `id`, evitando o
/// teto de variáveis do SQLite e sem trazer linhas desnecessárias.
fn fetch_assistant_data(db: &Path, lower_ms: i64) -> Vec<String> {
    let mut results = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let query = match &after {
            None => format!(
                "select id, data from session_message where type='assistant' and time_created >= {lower_ms} order by id limit {SQLITE_VAR_LIMIT}"
            ),
            Some(cursor) => format!(
                "select id, data from session_message where type='assistant' and time_created >= {lower_ms} and id > {} order by id limit {SQLITE_VAR_LIMIT}",
                sql_quote(cursor)
            ),
        };
        let Some(rows) = run_sqlite_json(db, &query) else {
            break;
        };
        if rows.is_empty() {
            break;
        }
        let mut last_id = None;
        for row in &rows {
            if let Some(data) = row.get("data").and_then(JsonValue::as_str) {
                results.push(data.to_owned());
            }
            last_id = row.get("id").and_then(JsonValue::as_str).map(str::to_owned);
        }
        match last_id {
            Some(cursor) if rows.len() == SQLITE_VAR_LIMIT => after = Some(cursor),
            _ => break,
        }
    }
    results
}

/// Escapa um literal de string SQL (aspas simples duplicadas).
fn sql_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn load(
    db: &Path,
    config: &Config,
    flat: bool,
    starts: Option<&BTreeMap<String, i64>>,
) -> (Vec<ModelUsage>, i64) {
    let validated = open_db(db);
    let now = now_ms();
    let mut bounds = [0i64; 3];
    for (index, (window, span, _)) in WINDOWS.iter().enumerate() {
        let start = starts
            .and_then(|map| map.get(*window))
            .copied()
            .unwrap_or(now - span);
        bounds[index] = start;
    }

    struct Aggregate {
        used: [f64; 3],
        count: [i64; 3],
    }
    let mut aggregates: BTreeMap<String, Aggregate> = BTreeMap::new();

    let lower = now - WINDOWS[WINDOWS.len() - 1].1;
    let _ = &validated;
    let peak_set: std::collections::BTreeSet<&str> =
        config.peak_doubled.iter().map(String::as_str).collect();

    for data in fetch_assistant_data(db, lower) {
        let Ok(document) = parse_json(&data) else {
            continue;
        };
        let Some(model_id) = document
            .get("model")
            .and_then(|model| model.get("id"))
            .and_then(JsonValue::as_str)
        else {
            continue;
        };
        let created = document
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(JsonValue::as_number)
            .map(|value| value as i64);
        let ts = created.unwrap_or(i64::MIN);
        let mut cost = document
            .get("cost")
            .and_then(JsonValue::as_number)
            .unwrap_or(0.0);
        if !flat && peak_set.contains(model_id) && is_peak(ts) {
            cost *= 2.0;
        }
        let aggregate = aggregates.entry(model_id.to_owned()).or_insert(Aggregate {
            used: [0.0; 3],
            count: [0; 3],
        });
        for (index, (_, _, _)) in WINDOWS.iter().enumerate() {
            if bounds[index] <= ts && ts <= now {
                aggregate.used[index] += cost;
                aggregate.count[index] += 1;
            }
        }
    }

    let mut models = Vec::new();
    for (id, aggregate) in aggregates {
        let limit = config.limits.get(&id).copied().flatten();
        let caps: [Option<f64>; 3] = match limit {
            Some(value) => [
                Some(value * WINDOWS[0].2),
                Some(value * WINDOWS[1].2),
                Some(value * WINDOWS[2].2),
            ],
            None => [None; 3],
        };
        let pct = [
            percentage(aggregate.used[0], caps[0]),
            percentage(aggregate.used[1], caps[1]),
            percentage(aggregate.used[2], caps[2]),
        ];
        let worst = if limit.is_some() {
            pct.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        } else {
            0.0
        };
        let status = status_for(limit, worst);
        let name = config
            .short
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id.chars().take(14).collect());
        models.push(ModelUsage {
            id,
            name,
            limit,
            used: aggregate.used,
            caps,
            pct,
            reqs: aggregate.count,
            worst,
            status,
        });
    }
    models.sort_by(|a, b| {
        b.worst
            .partial_cmp(&a.worst)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (models, now)
}

fn percentage(used: f64, cap: Option<f64>) -> f64 {
    match cap {
        Some(cap) if cap != 0.0 => used / cap * 100.0,
        _ => 0.0,
    }
}

fn status_for(limit: Option<f64>, worst: f64) -> &'static str {
    if limit.is_none() {
        "livre"
    } else if worst >= 100.0 {
        "estourado"
    } else if worst >= 90.0 {
        "critico"
    } else if worst >= 70.0 {
        "atencao"
    } else {
        "ok"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Formatação numérica compatível com os format-specs do Python.
// ─────────────────────────────────────────────────────────────────────────────

fn color_for(pct: f64) -> &'static str {
    if pct < 70.0 {
        GREEN
    } else if pct < 90.0 {
        YELLOW
    } else {
        RED
    }
}

/// `f"${x:,.2f}"` se |x| >= 1, senão `f"${x:.3f}"`.
fn usd(x: f64) -> String {
    if x.abs() >= 1.0 {
        format!("${}", group_thousands(x, 2))
    } else {
        format!("${:.*}", 3, x)
    }
}

/// Reproduz `format(value, ",.<decimals>f")` do Python (arredondamento bancário
/// via representação decimal exata, igual à estratégia `_Py_dg_dtoa`).
fn group_thousands(value: f64, decimals: usize) -> String {
    let negative = value.is_sign_negative() && value != 0.0;
    let magnitude = value.abs();
    let digits = py_format_decimal(magnitude, decimals);
    let (integer_part, fraction_part) = match digits.split_once('.') {
        Some((integer, fraction)) => (integer.to_owned(), fraction.to_owned()),
        None => (digits, String::new()),
    };
    let mut grouped = String::new();
    let chars: Vec<char> = integer_part.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if index > 0 && (chars.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*ch);
    }
    let mut result = String::new();
    if negative {
        result.push('-');
    }
    result.push_str(&grouped);
    if decimals > 0 {
        result.push('.');
        result.push_str(&fraction_part);
    }
    result
}

/// Arredonda `value` para `decimals` casas com HALF-EVEN, usando a representação
/// decimal exata do `f64` (a mesma decisão do Python para estes formatos).
fn py_format_decimal(value: f64, decimals: usize) -> String {
    if value == 0.0 {
        return if decimals > 0 {
            format!("0.{}", "0".repeat(decimals))
        } else {
            "0".to_owned()
        };
    }
    let (digits, exponent) = exact_decimal(value);
    // Valor = 0.digits × 10^exponent (digits sem zeros à esquerda), logo
    // N = floor(valor × 10^decimals) é formado pelos primeiros `keep` dígitos.
    let keep = exponent + decimals as i64;
    let mut scaled: Vec<u8> = digits;
    if keep < scaled.len() as i64 {
        let cut = keep.max(0) as usize;
        let kept_last = if cut > 0 {
            scaled.get(cut - 1).copied()
        } else {
            None
        };
        let round_up = decide_round_up(kept_last, &scaled[cut..]);
        scaled.truncate(cut);
        if round_up {
            propagate_carry(&mut scaled);
        }
    }
    if scaled.is_empty() {
        scaled.push(0);
    }

    // N em decimal (completa com zeros à direita quando `keep` é maior).
    let mut integer = String::with_capacity(scaled.len() + 1);
    for digit in &scaled {
        integer.push((b'0' + digit) as char);
    }
    if keep > 0 && (keep as usize) > scaled.len() {
        integer.push_str(&"0".repeat(keep as usize - scaled.len()));
    }
    // N / 10^decimals: o ponto decimal fica `decimals` dígitos à direita de N.
    let mut string = if decimals == 0 {
        integer
    } else if integer.len() > decimals {
        let split = integer.len() - decimals;
        format!("{}.{}", &integer[..split], &integer[split..])
    } else {
        format!("0.{}{}", "0".repeat(decimals - integer.len()), integer)
    };
    // Garante exatamente `decimals` casas decimais.
    pad_decimals(&mut string, decimals);
    string
}

/// Decompõe um `f64` positivo na sua representação decimal exata:
/// `0.digits × 10^exponent`, com `digits` sem zeros à esquerda.
/// Usa aritmética decimal arbitrária (vetor de dígitos) — sem overflow.
fn exact_decimal(value: f64) -> (Vec<u8>, i64) {
    let bits = value.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i64;
    let mantissa = bits & ((1u64 << 52) - 1);
    let (mantissa, exponent2) = if exponent_bits == 0 {
        (mantissa, -1074i64)
    } else {
        (mantissa | (1u64 << 52), exponent_bits - 1075)
    };

    // valor = mantissa × 2^exponent2 = mantissa × 5^p / 10^p, com p = max(0,-exponent2)
    // e, quando exponent2 > 0, valor = mantissa × 2^exponent2 (inteiro).
    if exponent2 >= 0 {
        // inteiro: mantissa << exponent2, em decimal.
        let mut digits = bigint_from_u64(mantissa);
        for _ in 0..exponent2 {
            bigint_mul_small(&mut digits, 2);
        }
        bigint_trim(&mut digits);
        let exponent = digits.len() as i64;
        return (if digits.is_empty() { vec![0] } else { digits }, exponent);
    }

    let p = (-exponent2) as u32;
    // valor = mantissa × 5^p / 10^p.
    let mut numerator = bigint_from_u64(mantissa);
    for _ in 0..p {
        bigint_mul_small(&mut numerator, 5);
    }
    bigint_trim(&mut numerator);
    // 0.digits × 10^exponent = numerator / 10^p  ⇒  digits = numerator,
    // exponent = (nº de dígitos de numerator) - p.
    let exponent = numerator.len() as i64 - p as i64;
    (numerator, exponent)
}

/// Converte um `u64` em dígitos decimais (mais significativo primeiro).
fn bigint_from_u64(mut value: u64) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    let mut reversed = Vec::new();
    while value > 0 {
        reversed.push((value % 10) as u8);
        value /= 10;
    }
    reversed.reverse();
    reversed
}

/// Multiplica em lugar um bigint decimal por um pequeno fator.
fn bigint_mul_small(digits: &mut Vec<u8>, factor: u32) {
    let mut carry = 0u32;
    for digit in digits.iter_mut().rev() {
        let product = u32::from(*digit) * factor + carry;
        *digit = (product % 10) as u8;
        carry = product / 10;
    }
    while carry > 0 {
        digits.insert(0, (carry % 10) as u8);
        carry /= 10;
    }
}

fn bigint_trim(digits: &mut Vec<u8>) {
    let leading = digits.iter().take_while(|digit| **digit == 0).count();
    if leading > 0 && leading < digits.len() {
        digits.drain(0..leading);
    }
}

fn decide_round_up(kept_last: Option<u8>, rest: &[u8]) -> bool {
    if rest.is_empty() {
        return false;
    }
    let first = rest[0];
    let tail_nonzero = rest[1..].iter().any(|digit| *digit != 0);
    if first > 5 {
        true
    } else if first < 5 {
        false
    } else if tail_nonzero {
        true
    } else {
        // Empate exato: HALF-EVEN depende da paridade do dígito mantido.
        kept_last.is_some_and(|digit| digit % 2 == 1)
    }
}

fn propagate_carry(digits: &mut Vec<u8>) {
    let mut index = digits.len();
    loop {
        if index == 0 {
            digits.insert(0, 1);
            return;
        }
        index -= 1;
        if digits[index] == 9 {
            digits[index] = 0;
        } else {
            digits[index] += 1;
            return;
        }
    }
}

fn pad_decimals(string: &mut String, decimals: usize) {
    let current = string.split_once('.').map_or(0, |(_, frac)| frac.len());
    if current < decimals {
        string.push_str(&"0".repeat(decimals - current));
    }
}

/// Reproduz `f"{value:>{width}.0f}"` (arredondamento HALF-EVEN, sem separador).
fn fixed0(value: f64, width: usize) -> String {
    let rounded = py_format_fixed0(value);
    let negative = rounded.starts_with('-');
    let body = if negative { &rounded[1..] } else { &rounded };
    if body.len() >= width {
        rounded
    } else {
        let padding = " ".repeat(width - body.len() - usize::from(negative));
        if negative {
            format!("-{padding}{body}")
        } else {
            format!("{padding}{rounded}")
        }
    }
}

fn py_format_fixed0(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let negative = value.is_sign_negative();
    let (digits, exponent) = exact_decimal(value.abs());
    let keep = exponent;
    let mut rounded = digits;
    if keep < 0 {
        rounded = vec![0];
    } else if (keep as usize) < rounded.len() {
        let kept_last = if keep > 0 {
            rounded.get(keep as usize - 1).copied()
        } else {
            None
        };
        let round_up = decide_round_up(kept_last, &rounded[keep as usize..]);
        rounded.truncate(keep as usize);
        if round_up {
            propagate_carry(&mut rounded);
        }
    }
    let mut integer = String::new();
    for digit in &rounded {
        integer.push((b'0' + digit) as char);
    }
    if (rounded.len() as i64) < exponent {
        integer.push_str(&"0".repeat(exponent as usize - rounded.len()));
    }
    if integer.is_empty() {
        integer.push('0');
    }
    if negative {
        format!("-{integer}")
    } else {
        integer
    }
}

/// `f"{value:>{width}}"` para floats já "limpos" (round 4 do Python).
fn round4(value: f64) -> f64 {
    py_round(value, 4)
}

fn round1(value: f64) -> f64 {
    py_round(value, 1)
}

/// Arredondamento HALF-EVEN do Python para `ndigits`.
fn py_round(value: f64, ndigits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    if ndigits >= 0 {
        let scale = 10f64.powi(ndigits);
        return round_half_even(value * scale) / scale;
    }
    round_half_even(value)
}

fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    let diff = value - floor;
    if diff > 0.5 {
        floor + 1.0
    } else if diff < 0.5 || (floor as i64) % 2 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

/// Serializa um float no estilo `json.dumps` do Python (repr mais curto).
fn py_float_json(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e16 {
        // `json.dumps` do Python sempre marca floats integrais com `.0`.
        return format!("{value:.1}");
    }
    let mut text = format!("{value}");
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    text
}

/// `json.dumps(..., ensure_ascii=False)` para uma string.
fn py_json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{0008}' => output.push_str("\\b"),
            '\u{000c}' => output.push_str("\\f"),
            c if (c as u32) < 0x20 => output.push_str(&format!("\\u{:04x}", c as u32)),
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

/// Barra com preenchimento `█` e vazio `░`; `int(round(pct/100*width))` HALF-EVEN.
fn bar(pct: f64, width: usize, use_color: bool) -> String {
    let clamped = pct.clamp(0.0, 100.0);
    let filled = round_half_even(clamped / 100.0 * width as f64) as usize;
    let filled = filled.min(width);
    let text = format!("{}{}", "█".repeat(filled), "░".repeat(width - filled));
    if use_color {
        format!("{}{}{}", color_for(pct), text, RESET)
    } else {
        text
    }
}

fn flag(model: &ModelUsage) -> &'static str {
    match model.status {
        "estourado" | "critico" => " ⚠",
        "atencao" => " ~",
        _ => "",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Métricas oficiais via `ai-usagebar usage --json`.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct OfficialMetric {
    pct: Option<f64>,
    reset_at: Option<String>,
    severity: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Official {
    windows: BTreeMap<String, OfficialMetric>,
    fetched_at: Option<String>,
    starts: BTreeMap<String, i64>,
}

/// Lê as métricas oficiais do OpenCode Go. `None` se indisponível.
fn official_usage(timeout_secs: u64) -> Option<Official> {
    let exe = which("ai-usagebar")?;
    let mut command = Command::new(exe);
    if let Some(config) = env::var_os("QUOTA_METER_AUB_CONFIG")
        && !config.is_empty()
    {
        command.arg("--config");
        command.arg(expand_home(&config.to_string_lossy()));
    }
    command.args(["usage", "--json"]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());
    if let Some(cache_home) = env::var_os("QUOTA_METER_AUB_CACHE_HOME")
        && !cache_home.is_empty()
    {
        command.env("XDG_CACHE_HOME", expand_home(&cache_home.to_string_lossy()));
    }
    let _ = timeout_secs;

    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let document = parse_json(String::from_utf8_lossy(&output.stdout).trim()).ok()?;
    let entries = document.get("entries").and_then(JsonValue::as_array)?;
    for entry in entries {
        let brand = entry.get("brand").and_then(JsonValue::as_str);
        let status = entry.get("status").and_then(JsonValue::as_str);
        let error = entry.get("error");
        if brand != Some("opencode-go")
            || status != Some("ready")
            || error.is_some_and(|value| !value.is_null())
        {
            continue;
        }
        let mut windows = BTreeMap::new();
        if let Some(metrics) = entry.get("metrics").and_then(JsonValue::as_array) {
            for metric in metrics {
                let label = metric
                    .get("label")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                let key = if label.contains("rolling") || label.contains("5h") {
                    Some("5h")
                } else if label.contains("weekly") || label.contains("7d") {
                    Some("7d")
                } else if label.contains("monthly") {
                    Some("30d")
                } else {
                    None
                };
                if let Some(key) = key {
                    windows.insert(
                        key.to_owned(),
                        OfficialMetric {
                            pct: metric.get("percent").and_then(JsonValue::as_number),
                            reset_at: metric
                                .get("reset_at")
                                .and_then(JsonValue::as_str)
                                .map(str::to_owned),
                            severity: metric
                                .get("severity")
                                .and_then(JsonValue::as_str)
                                .map(str::to_owned),
                        },
                    );
                }
            }
        }
        if windows.is_empty() {
            continue;
        }
        let starts = window_starts(&windows);
        return Some(Official {
            windows,
            fetched_at: entry
                .get("fetched_at")
                .and_then(JsonValue::as_str)
                .map(str::to_owned),
            starts,
        });
    }
    None
}

/// Deriva o início das janelas oficiais (7d = reset − 7d; 30d = mês anterior).
fn window_starts(windows: &BTreeMap<String, OfficialMetric>) -> BTreeMap<String, i64> {
    let mut starts = BTreeMap::new();
    if let Some(reset) = windows
        .get("7d")
        .and_then(|metric| metric.reset_at.as_deref())
        .and_then(parse_iso8601)
    {
        starts.insert("7d".to_owned(), reset - 7 * 86_400);
    }
    if let Some(reset) = windows
        .get("30d")
        .and_then(|metric| metric.reset_at.as_deref())
        .and_then(parse_iso8601)
    {
        let utc = utc_from_timestamp(reset as f64);
        let (year, month) = if utc.month == 1 {
            (utc.year - 1, 12)
        } else {
            (utc.year, utc.month - 1)
        };
        let day = utc.day.min(days_in_month(year, month));
        let previous = DateTimeUtc {
            year,
            month,
            day,
            hour: utc.hour,
            minute: utc.minute,
            second: utc.second,
        };
        starts.insert("30d".to_owned(), previous.timestamp_secs() as i64);
    }
    starts
}

// ─────────────────────────────────────────────────────────────────────────────
// Renderizadores de texto/JSON.
// ─────────────────────────────────────────────────────────────────────────────

fn fmt_pretty(models: &[ModelUsage], now: i64, size: usize, use_color: bool) -> String {
    if models.is_empty() {
        return "OC Go: sem uso nos últimos 30 dias".to_owned();
    }
    let head = format!("OC Go · {}", format_local_minute(now));
    let mut lines = vec![if use_color {
        format!("{CYAN}{head}{RESET}")
    } else {
        head
    }];
    for model in models.iter().take(6) {
        let cells = if model.limit.is_some() {
            let mut parts = Vec::new();
            for (index, (window, _, _)) in WINDOWS.iter().enumerate() {
                parts.push(format!(
                    "{window} {} {}%",
                    bar(model.pct[index], size, use_color),
                    fixed0(model.pct[index], 4)
                ));
            }
            parts.join("  ")
        } else {
            "ilimitado".to_owned()
        };
        lines.push(format!(
            "{} {}{}",
            pad_right(&model.name, 13),
            cells,
            flag(model)
        ));
    }
    lines.join("\n")
}

fn fmt_line(models: &[ModelUsage]) -> String {
    if models.is_empty() {
        return "OC Go: sem uso".to_owned();
    }
    let top = models
        .iter()
        .max_by(|a, b| {
            a.pct[1]
                .partial_cmp(&b.pct[1])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("lista não vazia");
    let max5 = models.iter().map(|m| m.pct[0]).fold(0.0f64, f64::max);
    let max30 = models.iter().map(|m| m.pct[2]).fold(0.0f64, f64::max);
    format!(
        "OC Go ⟫ 5h máx {}% · 7d máx {}% ({}){} · 30d máx {}%",
        fixed0(max5, 0),
        fixed0(top.pct[1], 0),
        top.name,
        flag(top),
        fixed0(max30, 0)
    )
}

fn fmt_tclock(models: &[ModelUsage], official: Option<&Official>, use_color: bool) -> String {
    let mut lines = Vec::new();
    if let Some(official) = official {
        let mut parts = Vec::new();
        for window in ["5h", "7d", "30d"] {
            if let Some(metric) = official.windows.get(window)
                && let Some(pct) = metric.pct
            {
                if use_color {
                    parts.push(format!(
                        "{window} {}{}{}",
                        color_for(pct),
                        fixed0(pct, 3),
                        RESET
                    ));
                } else {
                    parts.push(format!("{window} {}%", fixed0(pct, 3)));
                }
            }
        }
        if !parts.is_empty() {
            lines.push(format!("Oficial  {}", parts.join(" · ")));
        }
    }
    if !models.is_empty() {
        let mut header = "Modelo".to_owned();
        for window in ["5h", "7d", "30d"] {
            header.push_str(&fixed_width(window, 7, Alignment::Right));
        }
        lines.push(header);
        for model in models.iter().take(6) {
            if model.limit.is_none() {
                lines.push(format!("{} ilimitado", pad_right(&model.name, 14)));
                continue;
            }
            let mut row = pad_right(&model.name, 14);
            let mut cells = String::new();
            for index in 0..3 {
                let pct = model.pct[index];
                let mark = if pct >= 90.0 {
                    "⚠"
                } else if pct >= 70.0 {
                    "~"
                } else {
                    ""
                };
                let mut pad = format!("{}%{}", fixed0(pct, 4), mark);
                while pad.chars().count() < 7 {
                    pad.push(' ');
                }
                if use_color {
                    pad = format!("{}{}{}", color_for(pct), pad, RESET);
                }
                cells.push_str(&pad);
            }
            row.push_str(cells.trim_end());
            lines.push(row);
        }
    }
    if lines.is_empty() {
        lines.push("OC Go: sem uso nos últimos 30 dias".to_owned());
    }
    lines.join("\n")
}

fn fmt_details(models: &[ModelUsage], now: i64, official: Option<&Official>) -> String {
    let mut out = vec![format!(
        "OpenCode Go — quotas por modelo (5h/7d/30d) · {}",
        format_local_full(now)
    )];
    if let Some(official) = official {
        let mut parts = Vec::new();
        for window in ["5h", "7d", "30d"] {
            if let Some(metric) = official.windows.get(window)
                && let Some(pct) = metric.pct
            {
                let relative = metric
                    .reset_at
                    .as_deref()
                    .map(rel_reset)
                    .unwrap_or_default();
                let suffix = if relative.is_empty() {
                    String::new()
                } else {
                    format!(" (reseta {relative})")
                };
                parts.push(format!("{window} {}%{suffix}", fixed0(pct, 0)));
            }
        }
        if !parts.is_empty() {
            out.push(format!("Oficial (ai-usagebar): {}", parts.join(" · ")));
        }
    }
    out.push(format!(
        "{}{}{}{}{}",
        fixed_width("Modelo", 16, Alignment::Left),
        fixed_width("Lim", 6, Alignment::Right),
        "  ",
        fixed_width("5h", 19, Alignment::Right),
        format_args!(
            "  {}  {}  {}  Status",
            fixed_width("7d", 19, Alignment::Right),
            fixed_width("30d", 19, Alignment::Right),
            fixed_width("Reqs", 6, Alignment::Right)
        )
    ));
    for model in models {
        if model.limit.is_none() {
            out.push(format!(
                "{}{}  {}  {}  {}  {}  livre",
                fixed_width(&model.name, 16, Alignment::Left),
                fixed_width("∞", 6, Alignment::Right),
                fixed_width("—", 19, Alignment::Right),
                fixed_width("—", 19, Alignment::Right),
                fixed_width("—", 19, Alignment::Right),
                fixed_width(&model.reqs[2].to_string(), 6, Alignment::Right)
            ));
            continue;
        }
        let mut cells = Vec::new();
        for index in 0..3 {
            cells.push(format!(
                "{}/{} {}%",
                usd(model.used[index]),
                usd(model.caps[index].unwrap_or(0.0)),
                fixed0(model.pct[index], 3)
            ));
        }
        out.push(format!(
            "{}{}  {}  {}  {}  {}  {}",
            fixed_width(&model.name, 16, Alignment::Left),
            fixed_width(&usd(model.limit.unwrap_or(0.0)), 6, Alignment::Right),
            fixed_width(&cells[0], 19, Alignment::Right),
            fixed_width(&cells[1], 19, Alignment::Right),
            fixed_width(&cells[2], 19, Alignment::Right),
            fixed_width(&model.reqs[2].to_string(), 6, Alignment::Right),
            model.status
        ));
    }
    out.join("\n")
}

fn fmt_json(
    models: &[ModelUsage],
    now: i64,
    flat: bool,
    official: Option<&Official>,
    indent: Option<usize>,
) -> String {
    let mut root = String::new();
    root.push('{');
    root.push_str(&format!(
        "\"generated_at\": {}, \"peak_adjusted\": {}, \"official\": {}, \"models\": [",
        py_json_string(&iso_local_seconds(now)),
        if flat { "false" } else { "true" },
        official_json(official)
    ));
    for (index, model) in models.iter().enumerate() {
        if index > 0 {
            root.push_str(", ");
        }
        root.push('{');
        root.push_str(&format!(
            "\"id\": {}, \"name\": {}, \"limit_usd\": {}, \"windows\": {{",
            py_json_string(&model.id),
            py_json_string(&model.name),
            json_limit_option(model.limit)
        ));
        for (window_index, (window, _, _)) in WINDOWS.iter().enumerate() {
            if window_index > 0 {
                root.push_str(", ");
            }
            root.push_str(&format!(
                "{}: {{\"used_usd\": {}, \"cap_usd\": {}, \"pct\": {}, \"requests\": {}}}",
                py_json_string(window),
                py_float_json(round4(model.used[window_index])),
                json_number_option(model.caps[window_index]),
                py_float_json(round1(model.pct[window_index])),
                model.reqs[window_index]
            ));
        }
        root.push_str(&format!(
            "}}, \"status\": {}}}",
            py_json_string(model.status)
        ));
    }
    root.push_str("]}");
    match indent {
        Some(width) => py_indent_json(&root, width),
        None => root,
    }
}

fn json_number_option(value: Option<f64>) -> String {
    match value {
        Some(number) => py_float_json(number),
        None => "null".to_owned(),
    }
}

/// `limit_usd` é o valor bruto do config (int nas tabelas de Python, ex.: `60`);
/// `json.dumps` não acrescenta `.0` a ints, então floats integrais saem inteiros.
fn json_limit_option(value: Option<f64>) -> String {
    match value {
        Some(number) if number == number.trunc() && number.abs() < 1e16 => {
            format!("{}", number as i64)
        }
        Some(number) => py_float_json(number),
        None => "null".to_owned(),
    }
}

fn official_json(official: Option<&Official>) -> String {
    let Some(official) = official else {
        return "null".to_owned();
    };
    let mut parts = Vec::new();
    for window in ["5h", "7d", "30d"] {
        if let Some(metric) = official.windows.get(window) {
            let pct = metric
                .pct
                .and_then(py_float_json_num)
                .unwrap_or_else(|| "null".to_owned());
            let reset = metric
                .reset_at
                .as_deref()
                .map_or_else(|| "null".to_owned(), py_json_string);
            let severity = metric
                .severity
                .as_deref()
                .map_or_else(|| "null".to_owned(), py_json_string);
            parts.push(format!(
                "{}: {{\"pct\": {}, \"reset_at\": {}, \"severity\": {}}}",
                py_json_string(window),
                pct,
                reset,
                severity
            ));
        }
    }
    let starts: Vec<String> = official
        .starts
        .iter()
        .map(|(window, value)| {
            format!(
                "{}: {}",
                py_json_string(window),
                py_json_string(&utc_from_timestamp(*value as f64).iso_string())
            )
        })
        .collect();
    format!(
        "{{\"5h\": {}, \"7d\": {}, \"30d\": {}, \"fetched_at\": {}, \"starts\": {{{}}}}}",
        json_window_entry(official, "5h"),
        json_window_entry(official, "7d"),
        json_window_entry(official, "30d"),
        official
            .fetched_at
            .as_deref()
            .map_or_else(|| "null".to_owned(), py_json_string),
        starts.join(", ")
    )
}

fn py_float_json_num(value: f64) -> Option<String> {
    Some(py_float_json(value))
}

fn json_window_entry(official: &Official, window: &str) -> String {
    match official.windows.get(window) {
        Some(metric) => format!(
            "{{\"pct\": {}, \"reset_at\": {}, \"severity\": {}}}",
            metric
                .pct
                .map(py_float_json)
                .unwrap_or_else(|| "null".to_owned()),
            metric
                .reset_at
                .as_deref()
                .map_or_else(|| "null".to_owned(), py_json_string),
            metric
                .severity
                .as_deref()
                .map_or_else(|| "null".to_owned(), py_json_string)
        ),
        None => {
            let mut keys: Vec<&String> = official.windows.keys().collect();
            keys.sort();
            let _ = keys;
            "null".to_owned()
        }
    }
}

/// Reindenta um JSON compacto no estilo `json.dumps(indent=n)` (separadores `, `/`: `).
fn py_indent_json(text: &str, width: usize) -> String {
    let indent = " ".repeat(width);
    let mut output = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                output.push(ch);
            }
            '{' | '[' => {
                let closing = if ch == '{' { '}' } else { ']' };
                if matches!(chars.peek(), Some(c) if *c == closing) {
                    output.push(ch);
                    output.push(chars.next().unwrap());
                } else {
                    depth += 1;
                    output.push(ch);
                    output.push('\n');
                    output.push_str(&indent.repeat(depth));
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                output.push('\n');
                output.push_str(&indent.repeat(depth));
                output.push(ch);
            }
            ',' => {
                output.push(',');
                output.push('\n');
                output.push_str(&indent.repeat(depth));
            }
            ':' => output.push_str(": "),
            ' ' => {}
            _ => output.push(ch),
        }
    }
    output
}

#[derive(Clone, Copy)]
enum Alignment {
    Left,
    Right,
}

/// Emula `f"{text:<width}"` / `f"{text:>width}"` contando caracteres.
fn fixed_width(text: &str, width: usize, alignment: Alignment) -> String {
    let length = text.chars().count();
    if length >= width {
        return text.to_owned();
    }
    let padding = " ".repeat(width - length);
    match alignment {
        Alignment::Left => format!("{text}{padding}"),
        Alignment::Right => format!("{padding}{text}"),
    }
}

fn pad_right(value: &str, width: usize) -> String {
    fixed_width(value, width, Alignment::Left)
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI.
// ─────────────────────────────────────────────────────────────────────────────

struct Options {
    line: bool,
    details: bool,
    json: bool,
    tui: bool,
    watch: i64,
    flat: bool,
    bar: usize,
    no_color: bool,
    indent: Option<usize>,
    db: Option<String>,
    config: PathBuf,
    tclock: bool,
    official: bool,
    no_official: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            line: false,
            details: false,
            json: false,
            tui: false,
            watch: 0,
            flat: false,
            bar: 8,
            no_color: false,
            indent: None,
            db: None,
            config: default_config_path(),
            tclock: false,
            official: false,
            no_official: false,
        }
    }
}

const USAGE: &str = "\
Uso: quota-meter [opções]

  --line            resumo em uma linha
  --details         tabela completa
  --json            saída JSON (--indent N formata)
  --tui             interface interativa (stty + ANSI)
  --watch N         repete a cada N segundos
  --flat            não ajusta custo de pico
  --bar N           largura das barras (default 8)
  --no-color        sem cores ANSI
  --indent N        indentação do JSON
  --db PATH         caminho do opencode.db
  --config PATH     arquivo de configuração JSON (default ~/.config/quota-meter.json)
  --tclock          formato compacto p/ widget do tclock (inclui oficiais)
  --official        força leitura das métricas oficiais (ai-usagebar)
  --no-official     não consultar o ai-usagebar
  -V, --version     mostra a versão e sai
  -h, --help        mostra esta ajuda e sai";

fn parse_args() -> Options {
    let mut options = Options::default();
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--line" => options.line = true,
            "--details" => options.details = true,
            "--json" => options.json = true,
            "--tui" => options.tui = true,
            "--flat" => options.flat = true,
            "--no-color" => options.no_color = true,
            "--tclock" => options.tclock = true,
            "--official" => options.official = true,
            "--no-official" => options.no_official = true,
            "-V" | "--version" => {
                println!("quota-meter {VERSION}");
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--watch" => options.watch = parse_int_arg(&mut args, "--watch"),
            "--bar" => options.bar = parse_int_arg(&mut args, "--bar").max(0) as usize,
            "--indent" => {
                options.indent = Some(parse_int_arg(&mut args, "--indent").max(0) as usize)
            }
            "--db" => options.db = Some(next_value(&mut args, "--db")),
            "--config" => options.config = expand_home(&next_value(&mut args, "--config")),
            other => fail(&format!("argumento desconhecido: {other}\n{USAGE}")),
        }
    }
    options
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next()
        .unwrap_or_else(|| fail(&format!("quota-meter: faltou o valor de {flag}")))
}

fn parse_int_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> i64 {
    let raw = next_value(args, flag);
    raw.parse::<i64>()
        .unwrap_or_else(|_| fail(&format!("argumento {flag} precisa ser um inteiro")))
}

fn resolve_color(options: &Options) -> bool {
    !options.no_color
        && env::var_os("NO_COLOR").is_none()
        && (io::stdout().is_terminal() || env::var_os("TCLOCK_WIDGET_THEME").is_some())
}

fn use_official(options: &Options) -> bool {
    !options.no_official && (options.official || options.tclock || options.details || options.tui)
}

fn render(
    db: &Path,
    config: &Config,
    options: &Options,
    use_color: bool,
    official: Option<&Official>,
) -> String {
    let starts = official.map(|official| &official.starts);
    let (models, now) = load(db, config, options.flat, starts);
    if options.tclock {
        return fmt_tclock(&models, official, use_color);
    }
    if options.json {
        return fmt_json(&models, now, options.flat, official, options.indent);
    }
    if options.line {
        return fmt_line(&models);
    }
    if options.details {
        return fmt_details(&models, now, official);
    }
    fmt_pretty(&models, now, options.bar, use_color)
}

fn run() {
    let options = parse_args();
    let db = resolve_db(options.db.as_deref());
    let config = load_config(&options.config);

    // Correção do bug da origem: `--tui` funciona e consulta oficiais por padrão.
    if options.tui {
        run_tui(&db, &config, use_official(&options));
        return;
    }

    let color = resolve_color(&options);
    let wants_official = use_official(&options);

    if options.watch > 0 {
        loop {
            let official = if wants_official {
                official_usage(8)
            } else {
                None
            };
            println!(
                "{}",
                render(&db, &config, &options, color, official.as_ref())
            );
            io::stdout().flush().ok();
            std::thread::sleep(std::time::Duration::from_secs(options.watch.max(0) as u64));
        }
    } else {
        let official = if wants_official {
            official_usage(8)
        } else {
            None
        };
        println!(
            "{}",
            render(&db, &config, &options, color, official.as_ref())
        );
    }
}

fn main() {
    run();
}

// ─────────────────────────────────────────────────────────────────────────────
// TUI: `stty` + ANSI, espelhando o padrão `TerminalSession` do painel da casa.
// ─────────────────────────────────────────────────────────────────────────────

struct TerminalSession {
    original_mode: String,
}

impl TerminalSession {
    fn new() -> Result<Self, String> {
        let output = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .map_err(|error| format!("não foi possível consultar stty: {error}"))?;
        if !output.status.success() {
            return Err("stty -g falhou; verifique se stdin é um terminal".to_owned());
        }
        let original_mode = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let session = Self { original_mode };
        session
            .enable_raw_mode()
            .map_err(|error| format!("não foi possível ativar modo interativo: {error}"))?;
        print!("\x1b[?1049h\x1b[?25l");
        io::stdout()
            .flush()
            .map_err(|error| format!("não foi possível iniciar a tela TUI: {error}"))?;
        Ok(session)
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        let status = Command::new("stty")
            .args(["raw", "-echo", "min", "0", "time", "5"])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(
                "stty não conseguiu ativar modo interativo",
            ))
        }
    }

    fn restore_mode(&self) -> io::Result<()> {
        let status = Command::new("stty").arg(&self.original_mode).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("stty não conseguiu restaurar o terminal"))
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore_mode();
        print!("\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
}

fn terminal_size() -> (usize, usize) {
    if let Ok(output) = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        && output.status.success()
    {
        let values: Vec<usize> = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        if values.len() == 2 && values[0] > 0 && values[1] > 0 {
            return (values[0], values[1]);
        }
    }
    let rows = env::var("LINES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(24);
    let columns = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(80);
    (rows, columns)
}

fn read_key() -> io::Result<Option<u8>> {
    let mut input = io::stdin().lock();
    let mut byte = [0u8; 1];
    if input.read(&mut byte)? == 0 {
        return Ok(None);
    }
    Ok(Some(byte[0]))
}

fn run_tui(db: &Path, config: &Config, official_enabled: bool) {
    if env::var("TERM").is_ok_and(|term| term == "dumb") {
        fail("terminal TERM=dumb não oferece suporte à TUI; use --line/--details/--json.");
    }
    let session = match TerminalSession::new() {
        Ok(session) => session,
        Err(error) => fail(&error),
    };

    let mut flat = false;
    let mut official: Option<Official> = None;
    let mut official_last = 0.0f64;
    let mut last_draw = 0.0f64;

    loop {
        let now = now_secs();
        if official_enabled && now - official_last >= 300.0 {
            official = official_usage(8);
            official_last = now;
        }
        if now - last_draw >= 60.0 {
            if draw_screen(db, config, flat, official.as_ref()).is_err() {
                break;
            }
            last_draw = now_secs();
        }
        match read_key() {
            Ok(None) | Err(_) => {}
            Ok(Some(b'q')) => break,
            Ok(Some(b'r')) => {
                if draw_screen(db, config, flat, official.as_ref()).is_err() {
                    break;
                }
                last_draw = now_secs();
            }
            Ok(Some(b'a')) => {
                flat = !flat;
                if draw_screen(db, config, flat, official.as_ref()).is_err() {
                    break;
                }
                last_draw = now_secs();
            }
            Ok(Some(_)) => {}
        }
    }
    drop(session);
}

fn draw_screen(
    db: &Path,
    config: &Config,
    flat: bool,
    official: Option<&Official>,
) -> io::Result<()> {
    let (rows, columns) = terminal_size();
    let starts = official.map(|official| &official.starts);
    let (models, now) = load(db, config, flat, starts);

    let mut output = String::from("\x1b[2J\x1b[H");
    let title = format!(
        " quota-meter {VERSION} — OpenCode Go · {} · pico {} ",
        format_local_seconds(now),
        if flat { "OFF" } else { "ON" }
    );
    output.push_str("\x1b[7m");
    output.push_str(&take_chars(&title, columns.saturating_sub(1)));
    output.push_str(RESET);
    output.push_str("\r\n");

    if let Some(official) = official {
        let mut parts = Vec::new();
        for window in ["5h", "7d", "30d"] {
            if let Some(metric) = official.windows.get(window)
                && let Some(pct) = metric.pct
            {
                let relative = metric
                    .reset_at
                    .as_deref()
                    .map(rel_reset)
                    .unwrap_or_default();
                let suffix = if relative.is_empty() {
                    String::new()
                } else {
                    format!(" r:{relative}")
                };
                parts.push(format!("{window} {}%{suffix}", fixed0(pct, 0)));
            }
        }
        push_line(
            &mut output,
            &format!("Oficial (ai-usagebar): {}", parts.join(" · ")),
            columns,
            LineStyle::Normal,
        );
    }

    let mode = if columns >= 118 {
        "full"
    } else if columns >= 94 {
        "mid"
    } else {
        "tiny"
    };
    let cellw = match mode {
        "full" => 20,
        "mid" => 14,
        _ => 6,
    };
    let namew = if mode == "tiny" { 13 } else { 15 };

    let mut head = fixed_width("Modelo", namew, Alignment::Left);
    if mode == "full" {
        head.push_str(&fixed_width("Lim", 7, Alignment::Right));
    }
    head.push_str("  ");
    for window in ["5h", "7d", "30d"] {
        head.push_str(&fixed_width(window, cellw, Alignment::Left));
        head.push_str("  ");
    }
    head.push_str(&format!(
        "{}  Status",
        fixed_width("Reqs", 6, Alignment::Right)
    ));
    push_line(&mut output, &head, columns, LineStyle::Bold);

    let mut row = 3usize;
    for model in &models {
        if row >= rows.saturating_sub(3) {
            break;
        }
        let mut line = fixed_width(&model.name, namew, Alignment::Left);
        if mode == "full" {
            let limit = match model.limit {
                Some(value) => usd(value),
                None => "∞".to_owned(),
            };
            line.push_str(&fixed_width(&limit, 7, Alignment::Right));
        }
        line.push_str("  ");
        if model.limit.is_some() {
            for index in 0..3 {
                let cell = match mode {
                    "full" => format!(
                        "{}/{} {}%",
                        usd(model.used[index]),
                        usd(model.caps[index].unwrap_or(0.0)),
                        fixed0(model.pct[index], 3)
                    ),
                    "mid" => format!(
                        "{} {}%",
                        usd(model.used[index]),
                        fixed0(model.pct[index], 3)
                    ),
                    _ => format!("{}%", fixed0(model.pct[index], 3)),
                };
                line.push_str(&fixed_width(&cell, cellw, Alignment::Left));
                line.push_str("  ");
            }
        } else {
            line.push_str(&format!("ilimitado ({} reqs) ", model.reqs[2]));
        }
        line.push_str(&format!(
            "{}  {}{}",
            fixed_width(&model.reqs[2].to_string(), 6, Alignment::Right),
            model.status,
            flag(model)
        ));
        push_line(&mut output, &line, columns, LineStyle::Normal);
        row += 1;
    }
    if row == 3 {
        push_line(
            &mut output,
            "Sem uso registrado nos últimos 30 dias.",
            columns,
            LineStyle::Normal,
        );
    }
    push_line(
        &mut output,
        " q sair · r atualizar · a ajuste de pico · auto 60s ",
        columns,
        LineStyle::Dim,
    );

    io::stdout().write_all(output.as_bytes())?;
    io::stdout().flush()
}

#[derive(Clone, Copy)]
enum LineStyle {
    Normal,
    Bold,
    Dim,
}

fn push_line(output: &mut String, text: &str, columns: usize, style: LineStyle) {
    match style {
        LineStyle::Normal => {}
        LineStyle::Bold => output.push_str("\x1b[1m"),
        LineStyle::Dim => output.push_str("\x1b[2m"),
    }
    output.push_str(&take_chars(text, columns.saturating_sub(1)));
    if !matches!(style, LineStyle::Normal) {
        output.push_str(RESET);
    }
    output.push_str("\r\n");
}

fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_hours_follow_weekday_and_utc_windows() {
        // 2026-10-01 (quinta) 01:30 UTC → pico.
        assert!(is_peak(1790818200000));
        // 2026-10-01 (quinta) 05:00 UTC → fora do pico.
        assert!(!is_peak(1790830800000));
        // 2026-10-03 (sábado) 02:00 UTC → fim de semana, fora do pico.
        assert!(!is_peak(1790935200000));
        // 2026-10-01 (quinta) 09:59 UTC → pico.
        assert!(is_peak(1790848740000));
        // 2026-10-01 (quinta) 10:00 UTC → fora.
        assert!(!is_peak(1790848800000));
        // 2026-10-01 (quinta) 00:59 UTC → fora.
        assert!(!is_peak(1790816340000));
    }

    #[test]
    fn civil_calendar_round_trips() {
        for days in [-100_000i64, -1, 0, 1, 19_000, 20_000, 100_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "days={days}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn iso8601_parses_python_forms() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_iso8601("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            parse_iso8601("2026-10-02T03:58:46.037Z"),
            Some(1_790_913_526)
        );
        assert_eq!(parse_iso8601("2026-10-25T03:01:39Z"), Some(1_792_897_299));
        // Sem hora: interpretado como 00:00 UTC (a API sempre envia instante completo).
        assert_eq!(parse_iso8601("2026-10-01"), Some(1_790_812_800));
        assert_eq!(parse_iso8601(""), None);
        assert_eq!(parse_iso8601("nope"), None);
        assert_eq!(parse_iso8601("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601("2025-02-29T00:00:00Z"), None);
    }

    #[test]
    fn rel_reset_formats_like_python() {
        let now = now_secs().floor() as i64;
        assert_eq!(rel_reset(""), "");
        assert_eq!(rel_reset("not-a-date"), "");
        assert_eq!(rel_reset(&iso_for(now - 10)), "agora");
        assert_eq!(rel_reset(&iso_for(now + 90)), "1m");
        assert_eq!(rel_reset(&iso_for(now + 3660)), "1h 01m");
        assert_eq!(rel_reset(&iso_for(now + 3 * 86_400 + 5 * 3600)), "3d 05h");
    }

    fn iso_for(seconds: i64) -> String {
        utc_from_timestamp(seconds as f64).iso_string() + "Z"
    }

    #[test]
    fn usd_matches_python_currency_rules() {
        assert_eq!(usd(60.0), "$60.00");
        assert_eq!(usd(12.0), "$12.00");
        assert_eq!(usd(39.781), "$39.78");
        assert_eq!(usd(51.7059), "$51.71");
        assert_eq!(usd(0.955), "$0.955");
        assert_eq!(usd(0.0044), "$0.004");
        assert_eq!(usd(0.0038), "$0.004");
        assert_eq!(usd(1234.5), "$1,234.50");
    }

    #[test]
    fn fixed0_uses_half_even_rounding() {
        assert_eq!(fixed0(8.0, 4), "   8");
        assert_eq!(fixed0(132.6, 4), " 133");
        assert_eq!(fixed0(17.5, 4), "  18");
        assert_eq!(fixed0(0.0, 4), "   0");
        assert_eq!(fixed0(99.5, 3), "100");
        // HALF-EVEN: 0.5 → 0; 1.5 → 2; 2.5 → 2.
        assert_eq!(fixed0(0.5, 0), "0");
        assert_eq!(fixed0(1.5, 0), "2");
        assert_eq!(fixed0(2.5, 0), "2");
    }

    #[test]
    fn py_round_matches_python_bankers_rounding() {
        assert_eq!(round4(0.955), 0.955);
        assert_eq!(round4(51.7059), 51.7059);
        assert_eq!(round1(86.2), 86.2);
        assert_eq!(round1(0.0), 0.0);
    }

    #[test]
    fn bar_fills_with_half_even_rounding() {
        assert_eq!(bar(8.0, 8, false), "█░░░░░░░");
        assert_eq!(bar(133.0, 8, false), "████████");
        assert_eq!(bar(86.2, 8, false), "███████░");
        assert_eq!(bar(17.5, 8, false), "█░░░░░░░");
        assert_eq!(bar(0.0, 8, false), "░░░░░░░░");
        assert_eq!(bar(50.0, 8, false), "████░░░░");
    }

    #[test]
    fn bar_with_color_wraps_ansi() {
        assert_eq!(bar(100.0, 4, true), format!("{RED}████{RESET}"));
        assert_eq!(bar(0.0, 4, true), format!("{GREEN}░░░░{RESET}"));
        assert_eq!(bar(75.0, 4, true), format!("{YELLOW}███░{RESET}"));
    }

    #[test]
    fn json_payload_is_semantically_equivalent_to_python() {
        let models = vec![ModelUsage {
            id: "deepseek-v4.1-flash".to_owned(),
            name: "DeepSeek 4.1F".to_owned(),
            limit: Some(60.0),
            used: [0.955, 39.781, 51.7059],
            caps: [Some(12.0), Some(30.0), Some(60.0)],
            pct: [7.958333333333333, 132.60333333333332, 86.1765],
            reqs: [808, 14544, 19168],
            worst: 132.60333333333332,
            status: "estourado",
        }];
        let output = fmt_json(&models, 1790895462000, false, None, None);
        let parsed = parse_json(&output).unwrap();
        assert_eq!(parsed.get("peak_adjusted"), Some(&JsonValue::Bool(true)));
        let model = &parsed.get("models").unwrap().as_array().unwrap()[0];
        assert_eq!(
            model.get("limit_usd").and_then(JsonValue::as_number),
            Some(60.0)
        );
        assert_eq!(
            model.get("status").and_then(JsonValue::as_str),
            Some("estourado")
        );
    }

    #[test]
    fn config_merges_over_defaults() {
        let config = Config::default();
        assert_eq!(config.limits.get("deepseek-v4.1-flash"), Some(&Some(60.0)));
        assert_eq!(config.limits.get("space-bunny-free"), Some(&None));
        assert!(!config.limits.contains_key("unknown-model"));
    }

    #[test]
    fn sql_quote_escapes_single_quotes() {
        assert_eq!(sql_quote("a'b"), "'a''b'");
        assert_eq!(sql_quote("plain"), "'plain'");
    }

    #[test]
    fn parse_sqlite_rows_accepts_empty_output() {
        assert!(parse_sqlite_rows("").is_empty());
        assert!(parse_sqlite_rows("[]").is_empty());
        let rows = parse_sqlite_rows(r#"[{"x": 1}]"#);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("x").and_then(JsonValue::as_number), Some(1.0));
    }
}
