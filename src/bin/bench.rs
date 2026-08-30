//! Раннер ClickBench для veldb.
//!
//! Работает с базой напрямую, минуя HTTP и gRPC: измеряем движок, а не транспорт.
//! Датасет держится в памяти, WAL выключен — иначе бенчмарк мерил бы fsync.

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::io::{BufRead, BufReader, Write};
use std::time::Instant;
use veldb::column::{DataType, Value};
use veldb::Database;

#[derive(Parser, Debug)]
#[command(name = "veldb-bench", about = "Прогон ClickBench на veldb")]
struct Args {
    /// Путь к hits.tsv из ClickBench.
    #[arg(long)]
    tsv: Option<String>,

    /// Сгенерировать синтетический датасет из N строк вместо hits.tsv.
    /// Нужен, чтобы харнесс можно было проверить без выкачивания 15 ГБ.
    #[arg(long)]
    gen: Option<usize>,

    /// Ограничить число загружаемых строк.
    #[arg(long)]
    rows: Option<usize>,

    #[arg(long, default_value = "bench/hits.sql")]
    schema: String,

    #[arg(long, default_value = "bench/queries.sql")]
    queries: String,

    /// Сколько раз выполнять каждый запрос; в отчёт идёт медиана.
    #[arg(long, default_value_t = 3)]
    runs: usize,

    /// Куда положить отчёт в Markdown.
    #[arg(long)]
    out: Option<String>,

    /// Выполнить только запросы с этими номерами (через запятую), для отладки.
    #[arg(long)]
    only: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let db = Database::new();

    let schema_sql = std::fs::read_to_string(&args.schema)
        .with_context(|| format!("чтение схемы {}", args.schema))?;
    for stmt in split_sql(&schema_sql) {
        db.execute(&stmt)
            .with_context(|| format!("схема: {stmt}"))?;
    }

    let load_start = Instant::now();
    let nrows = match (&args.tsv, args.gen) {
        (Some(path), _) => load_tsv(&db, path, args.rows)?,
        (None, Some(n)) => generate(&db, args.rows.unwrap_or(n))?,
        (None, None) => bail!("нужен --tsv <hits.tsv> или --gen <N>"),
    };
    let load_secs = load_start.elapsed().as_secs_f64();
    let bytes = db
        .table("hits")
        .map(|t| t.read().unwrap().bytes_used())
        .unwrap_or(0);
    eprintln!(
        "загружено строк {nrows} за {load_secs:.1} с ({:.0} тыс. строк/с), в памяти {:.2} ГиБ",
        nrows as f64 / load_secs / 1000.0,
        bytes as f64 / (1 << 30) as f64
    );

    let queries_sql = std::fs::read_to_string(&args.queries)
        .with_context(|| format!("чтение запросов {}", args.queries))?;
    let queries = split_sql(&queries_sql);
    let only: Option<Vec<usize>> = args
        .only
        .as_ref()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect());

    let mut results = Vec::new();
    for (i, sql) in queries.iter().enumerate() {
        let n = i + 1;
        if let Some(only) = &only {
            if !only.contains(&n) {
                continue;
            }
        }
        let mut times = Vec::with_capacity(args.runs);
        let mut status = String::from("ok");
        let mut out_rows = 0usize;
        for _ in 0..args.runs {
            let t = Instant::now();
            match db.execute(sql) {
                Ok(r) => {
                    out_rows = r.rows.len();
                    times.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                Err(e) => {
                    status = format!("{:#}", e)
                        .lines()
                        .next()
                        .unwrap_or("ошибка")
                        .to_string();
                    break;
                }
            }
        }
        times.sort_by(f64::total_cmp);
        let median = times.get(times.len() / 2).copied();
        match median {
            Some(ms) => eprintln!("Q{n:<3} {ms:>9.1} мс  строк {out_rows}"),
            None => eprintln!("Q{n:<3} {:>9}  {status}", "—"),
        }
        results.push(Row {
            n,
            sql: sql.clone(),
            median,
            rows: out_rows,
            status,
        });
    }

    let report = render_report(&results, nrows, bytes, load_secs, args.runs);
    match &args.out {
        Some(p) => {
            std::fs::File::create(p)?.write_all(report.as_bytes())?;
            eprintln!("отчёт: {p}");
        }
        None => println!("{report}"),
    }
    Ok(())
}

struct Row {
    n: usize,
    sql: String,
    median: Option<f64>,
    rows: usize,
    status: String,
}

fn render_report(rows: &[Row], nrows: usize, bytes: usize, load_secs: f64, runs: usize) -> String {
    let ok: Vec<f64> = rows.iter().filter_map(|r| r.median).collect();
    let total: f64 = ok.iter().sum();
    let failed = rows.len() - ok.len();

    let mut s = String::new();
    s.push_str("# ClickBench на veldb\n\n");
    s.push_str(&format!(
        "- строк: **{nrows}**\n- в памяти: **{:.2} ГиБ** ({:.0} байт/строку)\n\
         - загрузка: **{load_secs:.1} с** ({:.0} тыс. строк/с)\n\
         - прогонов на запрос: {runs}, в таблице медиана\n\
         - выполнено: **{}/{}**, суммарно **{:.1} с**\n\n",
        bytes as f64 / (1u64 << 30) as f64,
        if nrows > 0 {
            bytes as f64 / nrows as f64
        } else {
            0.0
        },
        nrows as f64 / load_secs / 1000.0,
        ok.len(),
        rows.len(),
        total / 1000.0
    ));
    if failed > 0 {
        s.push_str(&format!(
            "{failed} запрос(ов) не выполнились — они перечислены ниже со статусом.\n\n"
        ));
    }
    s.push_str("| # | мс | строк | запрос |\n|---:|---:|---:|:---|\n");
    for r in rows {
        let ms = match r.median {
            Some(m) => format!("{m:.1}"),
            None => format!("— ({})", r.status),
        };
        s.push_str(&format!(
            "| {} | {} | {} | `{}` |\n",
            r.n,
            ms,
            r.rows,
            short(&r.sql)
        ));
    }
    s
}

fn short(sql: &str) -> String {
    let one: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 110 {
        format!("{}…", one.chars().take(110).collect::<String>())
    } else {
        one
    }
}

/// Делит файл на запросы. Строки-комментарии убираются до разбора: в них
/// встречаются и `;`, и апострофы, которые иначе съезжают с разбиением.
fn split_sql(text: &str) -> Vec<String> {
    let cleaned: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    for c in cleaned.chars() {
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

const BATCH: usize = 65_536;

fn load_tsv(db: &Database, path: &str, limit: Option<usize>) -> Result<usize> {
    let handle = db.table("hits").context("таблица hits не создана")?;
    let types: Vec<DataType> = handle
        .read()
        .unwrap()
        .schema
        .fields
        .iter()
        .map(|f| f.ty)
        .collect();
    drop(handle);

    let file = std::fs::File::open(path).with_context(|| format!("открытие {path}"))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = String::new();
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(BATCH);
    let mut total = 0usize;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let row = line.trim_end_matches('\n').trim_end_matches('\r');
        if row.is_empty() {
            continue;
        }
        let fields: Vec<&str> = row.split('\t').collect();
        if fields.len() != types.len() {
            bail!(
                "строка {}: полей {}, в схеме {}",
                total + 1,
                fields.len(),
                types.len()
            );
        }
        batch.push(
            fields
                .iter()
                .zip(&types)
                .map(|(f, ty)| parse_field(f, *ty))
                .collect::<Result<Vec<_>>>()?,
        );
        if batch.len() >= BATCH {
            total += db.insert_rows("hits", &batch)?;
            batch.clear();
            if total % (BATCH * 16) == 0 {
                eprint!("\rзагружено {total}");
            }
            if limit.is_some_and(|l| total >= l) {
                break;
            }
        }
    }
    if !batch.is_empty() {
        total += db.insert_rows("hits", &batch)?;
    }
    eprintln!("\rзагружено {total}   ");
    Ok(total)
}

/// TSV из ClickBench экранирует управляющие символы обратным слэшем.
fn parse_field(f: &str, ty: DataType) -> Result<Value> {
    Ok(match ty {
        // Пустое поле — это ноль, а не ошибка: в hits.tsv так закодировано «нет значения».
        DataType::I64 => Value::I64(if f.is_empty() {
            0
        } else {
            f.parse().unwrap_or(0)
        }),
        DataType::F64 => Value::F64(f.parse().unwrap_or(0.0)),
        DataType::Bool => Value::Bool(f == "1" || f.eq_ignore_ascii_case("true")),
        DataType::Str => Value::Str(unescape_tsv(f)),
        DataType::Vector(_) => bail!("векторных колонок в hits.tsv нет"),
    })
}

fn unescape_tsv(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Синтетический датасет с правдоподобными кардинальностями: без него харнесс
/// нельзя проверить, не выкачав 15 ГБ, а бенчмарк, который никто не прогонял,
/// ничего не доказывает.
fn generate(db: &Database, n: usize) -> Result<usize> {
    let handle = db.table("hits").context("таблица hits не создана")?;
    let fields = handle.read().unwrap().schema.fields.clone();
    drop(handle);

    let hosts = [
        "google.com",
        "yandex.ru",
        "vk.com",
        "example.org",
        "shop.test",
    ];
    let phrases = [
        "купить чай",
        "погода чита",
        "google translate",
        "рецепт какао",
        "",
    ];
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(BATCH);
    let mut total = 0usize;

    for i in 0..n {
        let user = rng.next_below(1_000_000) as i64;
        let host = hosts[rng.next_below(hosts.len() as u64) as usize];
        let path = rng.next_below(10_000);
        let url = format!("http://{host}/page/{path}");
        let phrase = phrases[rng.next_below(phrases.len() as u64) as usize];
        // Треть строк с CounterID = 62 — иначе запросы 37-43 возвращают пусто
        // и меряют только скорость отказа фильтра.
        let counter = if i % 3 == 0 {
            62
        } else {
            rng.next_below(1000) as i64
        };
        let event_time = 1_372_636_800 + rng.next_below(30 * 86_400) as i64; // июль 2013
        let row = fields
            .iter()
            .map(|f| match (f.name.as_str(), f.ty) {
                ("WatchID", _) => Value::I64(rng.next() as i64),
                ("UserID", _) => Value::I64(user),
                ("CounterID", _) => Value::I64(counter),
                ("EventTime", _) | ("ClientEventTime", _) | ("LocalEventTime", _) => {
                    Value::I64(event_time)
                }
                ("EventDate", _) => Value::I64(event_time / 86_400),
                ("URL", _) | ("OriginalURL", _) => Value::Str(url.clone()),
                ("Referer", _) => Value::Str(format!("http://{host}/")),
                ("Title", _) => Value::Str(format!("Страница {path} — {host}")),
                ("SearchPhrase", _) => Value::Str(phrase.to_string()),
                ("URLHash", _) => Value::I64(path as i64 * 1_000_003),
                ("RefererHash", _) => Value::I64(3_594_120_000_172_545_465),
                ("TraficSourceID", _) => Value::I64(rng.next_below(8) as i64 - 1),
                ("ResolutionWidth", _) => Value::I64(800 + rng.next_below(1200) as i64),
                // Флаги в реальном hits.tsv почти всегда нули; если сделать их
                // равномерными, запросы 37-43 отфильтруют всё и померят пустоту.
                ("IsRefresh", _)
                | ("DontCountHits", _)
                | ("IsDownload", _)
                | ("IsArtifical", _) => Value::I64(i64::from(rng.next_below(20) == 0)),
                ("IsLink", _) => Value::I64(i64::from(rng.next_below(3) == 0)),
                (_, DataType::I64) => Value::I64(rng.next_below(100) as i64),
                (_, DataType::F64) => Value::F64(rng.next_below(100) as f64),
                (_, DataType::Bool) => Value::Bool(rng.next_below(2) == 1),
                (_, DataType::Str) => Value::Str(String::new()),
                (_, DataType::Vector(d)) => Value::Vector(vec![0.0; d]),
            })
            .collect();
        batch.push(row);
        if batch.len() >= BATCH {
            total += db.insert_rows("hits", &batch)?;
            batch.clear();
            eprint!("\rсгенерировано {total}");
        }
    }
    if !batch.is_empty() {
        total += db.insert_rows("hits", &batch)?;
    }
    eprintln!("\rсгенерировано {total}   ");
    Ok(total)
}

/// Линейный конгруэнтный генератор: детерминированный и без зависимости.
/// Для наполнения таблицы качество распределения не критично.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 1
    }

    fn next_below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}
