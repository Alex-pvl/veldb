//! veldbctl — интерактивный клиент.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Editor, Helper};
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex};
use veldb::cli::{complete, format_table, is_complete, Catalog};
use veldb::client;

#[derive(Parser, Debug)]
#[command(name = "veldbctl", version, about = "Клиент veldb")]
struct Args {
    /// Адрес HTTP-интерфейса сервера.
    #[arg(long, default_value = "127.0.0.1:8080")]
    url: String,

    /// Выполнить запрос и выйти.
    #[arg(short = 'e', long)]
    execute: Option<String>,

    /// Выполнить запросы из файла и выйти.
    #[arg(short = 'f', long)]
    file: Option<String>,
}

/// Каталог обновляется после каждого DDL, поэтому автодополнение знает про
/// таблицу сразу после её создания, а не после перезапуска клиента.
struct Ctl {
    catalog: Arc<Mutex<Catalog>>,
}

impl Completer for Ctl {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let cat = self.catalog.lock().unwrap();
        let (start, words) = complete(line, pos, &cat);
        Ok((
            start,
            words
                .into_iter()
                .map(|w| Pair {
                    display: w.clone(),
                    replacement: w,
                })
                .collect(),
        ))
    }
}

impl Hinter for Ctl {
    type Hint = String;
}
impl Highlighter for Ctl {}
impl Validator for Ctl {}
impl Helper for Ctl {}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let host = client::normalize(&args.url);

    if let Some(sql) = &args.execute {
        return run_and_print(&host, sql, false).await;
    }
    if let Some(path) = &args.file {
        let text = std::fs::read_to_string(path).with_context(|| format!("чтение {path}"))?;
        for stmt in split_statements(&text) {
            run_and_print(&host, &stmt, false).await?;
        }
        return Ok(());
    }
    repl(&host).await
}

async fn fetch_catalog(host: &str) -> Result<Catalog> {
    let body = client::get(host, "/schema").await?;
    Ok(Catalog::from_schema_json(&serde_json::from_slice::<Json>(
        &body,
    )?))
}

async fn repl(host: &str) -> Result<()> {
    let catalog = Arc::new(Mutex::new(match fetch_catalog(host).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("не удалось получить схему: {e:#}");
            eprintln!("сервер запущен? veldb --http {host}");
            return Err(e);
        }
    }));

    let mut rl: Editor<Ctl, rustyline::history::DefaultHistory> =
        Editor::new().map_err(|e| anyhow!("{e}"))?;
    rl.set_helper(Some(Ctl {
        catalog: catalog.clone(),
    }));
    let history = dirs_history();
    let _ = rl.load_history(&history);

    println!(
        "veldb {} — \\help для справки, \\q для выхода",
        env!("CARGO_PKG_VERSION")
    );
    let mut timing = false;
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() {
            "veldb> "
        } else {
            "    -> "
        };
        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if buffer.is_empty() && trimmed.is_empty() {
                    continue;
                }
                if buffer.is_empty() && trimmed.starts_with('\\') {
                    let _ = rl.add_history_entry(trimmed);
                    match meta(host, trimmed, &mut timing, &catalog).await {
                        Ok(true) => break,
                        Ok(false) => {}
                        Err(e) => eprintln!("ошибка: {e:#}"),
                    }
                    continue;
                }
                buffer.push_str(&line);
                buffer.push('\n');
                // Точка с запятой не обязательна: одиночная строка выполняется
                // по Enter, многострочный ввод — по `;`.
                if !is_complete(&buffer) && line.trim_end().ends_with(',') {
                    continue;
                }
                if !is_complete(&buffer) && !line.trim().is_empty() && needs_more(&buffer) {
                    continue;
                }
                let stmt = buffer.trim().trim_end_matches(';').to_string();
                buffer.clear();
                if stmt.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&stmt);
                if let Err(e) = run_and_print(host, &stmt, timing).await {
                    eprintln!("ошибка: {e:#}");
                }
                if is_ddl(&stmt) {
                    if let Ok(c) = fetch_catalog(host).await {
                        *catalog.lock().unwrap() = c;
                    }
                }
            }
            // Ctrl-C бросает недописанный запрос, но не выходит из клиента:
            // выйти по ошибке из-за длинного запроса обиднее, чем нажать \q.
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                println!("^C");
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(anyhow!("{e}")),
        }
    }
    let _ = rl.save_history(&history);
    Ok(())
}

/// Ввод продолжается, если открыта скобка или строковый литерал.
fn needs_more(buf: &str) -> bool {
    let mut depth = 0i32;
    let mut in_str = false;
    for c in buf.chars() {
        match c {
            '\'' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth -= 1,
            _ => {}
        }
    }
    in_str || depth > 0
}

fn is_ddl(sql: &str) -> bool {
    let s = sql.trim_start().to_lowercase();
    s.starts_with("create") || s.starts_with("drop")
}

fn dirs_history() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".veldb_history")
}

async fn meta(
    host: &str,
    cmd: &str,
    timing: &mut bool,
    catalog: &Arc<Mutex<Catalog>>,
) -> Result<bool> {
    let mut parts = cmd.split_whitespace();
    match parts.next().unwrap_or("") {
        "\\q" | "\\quit" => return Ok(true),
        "\\dt" => run_and_print(host, "SHOW TABLES", false).await?,
        "\\d" => match parts.next() {
            Some(t) => run_and_print(host, &format!("DESCRIBE {t}"), false).await?,
            None => run_and_print(host, "SHOW TABLES", false).await?,
        },
        "\\timing" => {
            *timing = !*timing;
            println!(
                "замер времени: {}",
                if *timing {
                    "включён"
                } else {
                    "выключен"
                }
            );
        }
        "\\refresh" => {
            *catalog.lock().unwrap() = fetch_catalog(host).await?;
            println!("схема обновлена");
        }
        "\\snapshot" => {
            let body = client::post_json(host, "/snapshot", &json!({})).await?;
            println!("{}", String::from_utf8_lossy(&body));
        }
        "\\help" => println!(
            "\\dt              список таблиц\n\
             \\d <таблица>     колонки таблицы\n\
             \\timing          замер времени запроса\n\
             \\snapshot        сбросить состояние на диск\n\
             \\refresh         перечитать схему для автодополнения\n\
             \\q               выход\n\
             Tab — автодополнение, Ctrl-C — сбросить ввод."
        ),
        other => println!("неизвестная команда '{other}', см. \\help"),
    }
    Ok(false)
}

async fn run_and_print(host: &str, sql: &str, timing: bool) -> Result<()> {
    let body = client::post_json(host, "/query", &json!({ "sql": sql })).await?;
    let v: Json = serde_json::from_slice(&body)?;
    let columns: Vec<String> = v["columns"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().unwrap_or("?").to_string())
                .collect()
        })
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = v["rows"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| {
                    r.as_array()
                        .map(|r| r.iter().map(render).collect())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();

    if columns.is_empty() {
        println!("{v}");
        return Ok(());
    }
    println!("{}", format_table(&columns, &rows));
    let n = rows.len();
    if timing {
        println!(
            "строк: {n} ({:.2} мс)",
            v["elapsed_ms"].as_f64().unwrap_or(0.0)
        );
    } else {
        println!("строк: {n}");
    }
    Ok(())
}

fn render(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => other.to_string(),
    }
}

/// Делит файл на запросы по `;` вне строковых литералов.
fn split_statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    for c in text.chars() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            ';' if !in_str => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}
