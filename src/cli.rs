//! Логика CLI, отделённая от ввода-вывода, чтобы её можно было тестировать
//! без псевдотерминала.

use rustc_hash::FxHashMap;

/// Ключевые слова и функции для автодополнения. Список короткий намеренно:
/// дополнять тем, чего движок не умеет, — хуже, чем не дополнять вовсе.
pub const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "LIMIT",
    "OFFSET",
    "INSERT INTO",
    "VALUES",
    "CREATE TABLE",
    "DROP TABLE",
    "IF EXISTS",
    "IF NOT EXISTS",
    "SHOW TABLES",
    "AND",
    "OR",
    "NOT",
    "IN",
    "BETWEEN",
    "LIKE",
    "ILIKE",
    "AS",
    "ASC",
    "DESC",
    "DISTINCT",
    "CAST",
    "EXTRACT",
    "INT",
    "DOUBLE",
    "BOOL",
    "TEXT",
    "VECTOR",
    "count(",
    "sum(",
    "avg(",
    "min(",
    "max(",
    "uniq(",
    "length(",
    "lower(",
    "upper(",
    "abs(",
    "floor(",
    "ceil(",
    "round(",
    "sqrt(",
    "substring(",
    "to_hour(",
    "to_day(",
    "to_month(",
    "to_year(",
    "if(",
    "l2_distance(",
    "cosine_distance(",
    "inner_product(",
];

/// Что известно о базе на стороне клиента: имена таблиц и их колонок.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub tables: Vec<String>,
    pub columns: FxHashMap<String, Vec<String>>,
}

impl Catalog {
    pub fn from_schema_json(v: &serde_json::Value) -> Catalog {
        let mut c = Catalog::default();
        for t in v["tables"].as_array().unwrap_or(&Vec::new()) {
            let Some(name) = t["name"].as_str() else {
                continue;
            };
            let cols: Vec<String> = t["columns"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c["name"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            c.tables.push(name.to_string());
            c.columns.insert(name.to_lowercase(), cols);
        }
        c.tables.sort();
        c
    }

    fn all_columns(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .columns
            .values()
            .flatten()
            .map(|s| s.as_str())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// Начало дополняемого слова: индекс и сам префикс.
pub fn word_at(line: &str, pos: usize) -> (usize, &str) {
    let head = &line[..pos];
    let start = head
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\\'))
        .map(|i| i + 1)
        .unwrap_or(0);
    (start, &head[start..])
}

/// Подсказки для позиции `pos` в строке `line`.
///
/// После `FROM`/`INTO`/`TABLE` предлагаются только таблицы: в этой позиции
/// колонка или ключевое слово синтаксически невозможны, и мусор в списке
/// раздражает больше, чем отсутствие подсказки.
pub fn complete(line: &str, pos: usize, cat: &Catalog) -> (usize, Vec<String>) {
    let (start, prefix) = word_at(line, pos);
    let lower = prefix.to_lowercase();

    if prefix.starts_with('\\') {
        let cands = ["\\dt", "\\d", "\\timing", "\\q", "\\help"];
        return (start, pick(&cands, &lower));
    }

    let prev = line[..start]
        .split_whitespace()
        .next_back()
        .unwrap_or("")
        .to_lowercase();
    if matches!(prev.as_str(), "from" | "into" | "table" | "join" | "update") {
        let names: Vec<&str> = cat.tables.iter().map(|s| s.as_str()).collect();
        return (start, pick(&names, &lower));
    }

    let mut cands: Vec<&str> = cat.all_columns();
    cands.extend(cat.tables.iter().map(|s| s.as_str()));
    cands.extend(KEYWORDS.iter().copied());
    (start, pick(&cands, &lower))
}

fn pick(cands: &[&str], prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = cands
        .iter()
        .filter(|c| c.to_lowercase().starts_with(prefix))
        .map(|c| c.to_string())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Запрос считается законченным, когда точка с запятой стоит вне строкового
/// литерала. Иначе `'a;b'` обрывал бы ввод на середине.
pub fn is_complete(input: &str) -> bool {
    let mut in_str = false;
    let mut chars = input.chars().peekable();
    let mut last_semi = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' if in_str && chars.peek() == Some(&'\'') => {
                chars.next(); // экранированная кавычка ''
            }
            '\'' => in_str = !in_str,
            ';' if !in_str => last_semi = true,
            c if !c.is_whitespace() => last_semi = false,
            _ => {}
        }
    }
    last_semi
}

/// Табличный вывод. Ширина считается по символам: кириллица и латиница занимают
/// одну ячейку, а на эмодзи в именах колонок рамка поедет — цена приемлемая.
pub fn format_table(columns: &[String], rows: &[Vec<String>]) -> String {
    let mut w: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for r in rows {
        for (i, cell) in r.iter().enumerate().take(w.len()) {
            w[i] = w[i].max(cell.chars().count());
        }
    }
    let pad = |s: &str, n: usize| {
        let mut out = s.to_string();
        out.extend(std::iter::repeat_n(
            ' ',
            n.saturating_sub(s.chars().count()),
        ));
        out
    };
    let line = |sep: &str| {
        w.iter()
            .map(|n| "─".repeat(n + 2))
            .collect::<Vec<_>>()
            .join(sep)
    };

    let mut out = String::new();
    out.push_str(&format!("┌{}┐\n", line("┬")));
    out.push_str(&format!(
        "│ {} │\n",
        columns
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, w[i]))
            .collect::<Vec<_>>()
            .join(" │ ")
    ));
    out.push_str(&format!("├{}┤\n", line("┼")));
    for r in rows {
        out.push_str(&format!(
            "│ {} │\n",
            r.iter()
                .enumerate()
                .map(|(i, c)| pad(c, w[i]))
                .collect::<Vec<_>>()
                .join(" │ ")
        ));
    }
    out.push_str(&format!("└{}┘", line("┴")));
    out
}
