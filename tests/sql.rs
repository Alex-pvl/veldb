use veldb::exec::render;
use veldb::Database;

/// Результат запроса в виде строк вида "a|b|c" — так эталоны читаются глазами.
fn q(db: &Database, sql: &str) -> Vec<String> {
    let r = db
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}\n  -> {e:#}"));
    r.rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect()
}

fn err(db: &Database, sql: &str) -> String {
    format!("{:#}", db.execute(sql).unwrap_err())
}

fn shop() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE sales (id INT, city TEXT, item TEXT, qty INT, price DOUBLE, ts INT)")
        .unwrap();
    db.execute(
        "INSERT INTO sales VALUES
         (1, 'Чита',   'чай',   3, 100.0, 1700000000),
         (2, 'Чита',   'кофе',  1, 250.5, 1700003600),
         (3, 'Москва', 'чай',   10, 90.0, 1700007200),
         (4, 'Москва', 'какао', 2, 300.0, 1700010800),
         (5, 'Питер',  'чай',   0, 95.0,  1700014400),
         (6, 'Чита',   'чай',   7, 110.0, 1700018000)",
    )
    .unwrap();
    db
}

#[test]
fn select_star_and_projection() {
    let db = shop();
    assert_eq!(q(&db, "SELECT * FROM sales").len(), 6);
    assert_eq!(
        q(&db, "SELECT id, item FROM sales LIMIT 2"),
        ["1|чай", "2|кофе"]
    );
    assert_eq!(
        q(&db, "SELECT qty * 2 AS d FROM sales LIMIT 3"),
        ["6", "2", "20"]
    );
}

#[test]
fn where_comparisons_and_boolean_logic() {
    let db = shop();
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE qty > 2"),
        ["1", "3", "6"]
    );
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE city = 'Чита' AND qty >= 3"),
        ["1", "6"]
    );
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE qty = 0 OR price > 290"),
        ["4", "5"]
    );
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE NOT (city = 'Чита')"),
        ["3", "4", "5"]
    );
    assert_eq!(
        q(
            &db,
            "SELECT id FROM sales WHERE city <> 'Чита' AND item = 'чай'"
        ),
        ["3", "5"]
    );
    assert!(q(&db, "SELECT id FROM sales WHERE 1 = 0").is_empty());
    assert_eq!(q(&db, "SELECT id FROM sales WHERE 1 = 1").len(), 6);
}

#[test]
fn in_between_like() {
    let db = shop();
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE id IN (2, 4, 99)"),
        ["2", "4"]
    );
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE id NOT IN (1,2,3,4)"),
        ["5", "6"]
    );
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE qty BETWEEN 1 AND 3"),
        ["1", "2", "4"]
    );
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE item LIKE 'к%'"),
        ["2", "4"]
    );
    assert_eq!(
        q(&db, "SELECT id FROM sales WHERE item NOT LIKE '%а%'"),
        ["2"]
    );
}

#[test]
fn aggregates_without_group_by() {
    let db = shop();
    assert_eq!(q(&db, "SELECT count(*) FROM sales"), ["6"]);
    assert_eq!(q(&db, "SELECT sum(qty) FROM sales"), ["23"]);
    assert_eq!(q(&db, "SELECT min(qty), max(qty) FROM sales"), ["0|10"]);
    assert_eq!(q(&db, "SELECT count(DISTINCT city) FROM sales"), ["3"]);
    assert_eq!(q(&db, "SELECT uniq(item) FROM sales"), ["3"]);
    // avg = 945.5 / 6
    assert_eq!(q(&db, "SELECT round(avg(price), 2) FROM sales"), ["157.58"]);
    // Агрегат по пустой выборке: NULL нет, поэтому нейтральные значения.
    assert_eq!(
        q(
            &db,
            "SELECT count(*), sum(qty), avg(price) FROM sales WHERE id > 100"
        ),
        ["0|0|0"]
    );
}

#[test]
fn group_by_with_order_and_limit() {
    let db = shop();
    assert_eq!(
        q(
            &db,
            "SELECT city, count(*) c FROM sales GROUP BY city ORDER BY c DESC, city"
        ),
        ["Чита|3", "Москва|2", "Питер|1"]
    );
    assert_eq!(
        q(
            &db,
            "SELECT item, sum(qty) s FROM sales GROUP BY item ORDER BY s DESC LIMIT 2"
        ),
        ["чай|20", "какао|2"]
    );
    // Несколько ключей группировки.
    assert_eq!(
        q(
            &db,
            "SELECT city, item, count(*) FROM sales GROUP BY city, item ORDER BY 1, 2"
        ),
        [
            "Москва|какао|1",
            "Москва|чай|1",
            "Питер|чай|1",
            "Чита|кофе|1",
            "Чита|чай|2"
        ]
    );
    // Выражение как ключ группировки.
    assert_eq!(
        q(
            &db,
            "SELECT qty > 2 AS big, count(*) FROM sales GROUP BY qty > 2 ORDER BY 1"
        ),
        ["false|3", "true|3"]
    );
}

#[test]
fn order_by_resolves_ordinal_alias_and_expression_text() {
    let db = shop();
    // qty у чая: id1=3, id3=10, id5=0, id6=7.
    assert_eq!(
        q(
            &db,
            "SELECT id, price FROM sales WHERE item='чай' ORDER BY qty DESC"
        ),
        ["3|90", "6|110", "1|100", "5|95"]
    );
    assert_eq!(
        q(
            &db,
            "SELECT id, price p FROM sales WHERE item='чай' ORDER BY p"
        ),
        ["3|90", "5|95", "1|100", "6|110"]
    );
    assert_eq!(
        q(
            &db,
            "SELECT id, price FROM sales WHERE item='чай' ORDER BY 2"
        ),
        ["3|90", "5|95", "1|100", "6|110"]
    );
}

#[test]
fn order_by_is_stable_for_equal_keys() {
    let db = shop();
    // qty у 1 и 6 разные, а вот item='чай' одинаков — порядок должен быть по id.
    assert_eq!(
        q(&db, "SELECT id FROM sales ORDER BY item"),
        ["4", "2", "1", "3", "5", "6"]
    );
}

#[test]
fn limit_offset() {
    let db = shop();
    assert_eq!(q(&db, "SELECT id FROM sales LIMIT 2 OFFSET 3"), ["4", "5"]);
    assert_eq!(
        q(
            &db,
            "SELECT id FROM sales ORDER BY id DESC LIMIT 2 OFFSET 1"
        ),
        ["5", "4"]
    );
    assert!(q(&db, "SELECT id FROM sales LIMIT 0").is_empty());
    assert!(q(&db, "SELECT id FROM sales OFFSET 100").is_empty());
}

#[test]
fn scalar_functions() {
    let db = shop();
    assert_eq!(q(&db, "SELECT length(city) FROM sales LIMIT 1"), ["4"]);
    assert_eq!(q(&db, "SELECT upper(item) FROM sales LIMIT 1"), ["ЧАЙ"]);
    assert_eq!(
        q(&db, "SELECT substring(city, 1, 2) FROM sales LIMIT 1"),
        ["Чи"]
    );
    assert_eq!(q(&db, "SELECT abs(0 - qty) FROM sales LIMIT 1"), ["3"]);
    assert_eq!(
        q(
            &db,
            "SELECT if(qty > 2, 'много', 'мало') FROM sales LIMIT 2"
        ),
        ["много", "мало"]
    );
    // ts = 1700000000 → 2023-11-14 22:13:20 UTC
    assert_eq!(q(&db, "SELECT EXTRACT(YEAR FROM ts), EXTRACT(MONTH FROM ts), EXTRACT(DAY FROM ts), EXTRACT(HOUR FROM ts) FROM sales LIMIT 1"), ["2023|11|14|22"]);
    assert_eq!(
        q(
            &db,
            "SELECT to_hour(ts) h, count(*) FROM sales GROUP BY to_hour(ts) ORDER BY h"
        ),
        ["0|1", "1|1", "2|1", "3|1", "22|1", "23|1"]
    );
}

#[test]
fn integer_arithmetic_stays_exact() {
    let db = Database::new();
    db.execute("CREATE TABLE big (x INT)").unwrap();
    db.execute("INSERT INTO big VALUES (9007199254740993)")
        .unwrap();
    // Число не представимо в f64 — целочисленный путь обязан сохранить его точно.
    assert_eq!(q(&db, "SELECT x + 1 FROM big"), ["9007199254740994"]);
    assert_eq!(
        q(&db, "SELECT x FROM big WHERE x = 9007199254740993"),
        ["9007199254740993"]
    );
}

#[test]
fn division_and_modulo_do_not_panic() {
    let db = shop();
    assert_eq!(q(&db, "SELECT qty % 0 FROM sales LIMIT 1"), ["0"]);
    assert_eq!(q(&db, "SELECT qty / 2 FROM sales LIMIT 1"), ["1.5"]);
    assert_eq!(q(&db, "SELECT price / 0 FROM sales LIMIT 1"), ["inf"]);
}

#[test]
fn insert_with_explicit_column_order() {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();
    db.execute("INSERT INTO t (b, a) VALUES ('x', 1)").unwrap();
    assert_eq!(q(&db, "SELECT a, b FROM t"), ["1|x"]);
    assert!(err(&db, "INSERT INTO t (a) VALUES (1)").contains("NULL"));
}

#[test]
fn ddl_and_catalog() {
    let db = shop();
    assert_eq!(q(&db, "SHOW TABLES"), ["sales|6"]);
    db.execute("CREATE TABLE tmp (a INT)").unwrap();
    assert_eq!(q(&db, "SHOW TABLES"), ["sales|6", "tmp|0"]);
    db.execute("DROP TABLE tmp").unwrap();
    assert_eq!(q(&db, "SHOW TABLES"), ["sales|6"]);
    assert!(err(&db, "DROP TABLE tmp").contains("не найдена"));
    db.execute("DROP TABLE IF EXISTS tmp").unwrap();
}

#[test]
fn errors_are_specific_not_generic() {
    let db = shop();
    assert!(err(&db, "SELECT nope FROM sales").contains("нет колонки 'nope'"));
    assert!(err(&db, "SELECT * FROM nope").contains("'nope' не найдена"));
    assert!(err(&db, "SELECT city FROM sales GROUP BY item").contains("не агрегат"));
    assert!(err(&db, "SELECT * FROM sales WHERE city > 1").contains("сравнение строки"));
    assert!(err(&db, "SELECT frobnicate(id) FROM sales").contains("не поддерживается"));
    assert!(err(&db, "SELECT id FROM sales; SELECT id FROM sales").contains("один запрос"));
    assert!(err(&db, "SELECT * FROM sales WHERE id IS NULL").contains("NULL"));
}

#[test]
fn having_filters_groups_after_aggregation() {
    let db = shop();
    assert_eq!(
        q(
            &db,
            "SELECT city, count(*) c FROM sales GROUP BY city HAVING count(*) > 1 ORDER BY c DESC"
        ),
        ["Чита|3", "Москва|2"]
    );
    // HAVING по агрегату, которого нет в SELECT.
    assert_eq!(
        q(
            &db,
            "SELECT city FROM sales GROUP BY city HAVING sum(qty) >= 10 ORDER BY city"
        ),
        ["Москва", "Чита"]
    );
    assert!(q(
        &db,
        "SELECT city FROM sales GROUP BY city HAVING count(*) > 99"
    )
    .is_empty());
    assert!(err(&db, "SELECT id FROM sales HAVING id > 1").contains("HAVING без агрегатов"));
}

#[test]
fn case_when_becomes_nested_conditionals() {
    let db = shop();
    assert_eq!(
        q(&db, "SELECT CASE WHEN qty = 0 THEN 'нет' WHEN qty < 5 THEN 'мало' ELSE 'много' END FROM sales"),
        ["мало", "мало", "много", "мало", "нет", "много"]
    );
    // CASE с операндом.
    assert_eq!(
        q(
            &db,
            "SELECT CASE city WHEN 'Чита' THEN 1 ELSE 0 END FROM sales LIMIT 3"
        ),
        ["1", "1", "0"]
    );
    // Без ELSE ветка по умолчанию — пустая строка, NULL у нас нет.
    assert_eq!(
        q(
            &db,
            "SELECT CASE WHEN qty > 5 THEN 'да' END FROM sales LIMIT 2"
        ),
        ["", ""]
    );
}

#[test]
fn group_by_accepts_ordinals_like_clickbench_queries() {
    let db = shop();
    assert_eq!(
        q(
            &db,
            "SELECT 1, item, count(*) c FROM sales GROUP BY 1, item ORDER BY c DESC, item LIMIT 2"
        ),
        ["1|чай|4", "1|какао|1"]
    );
    assert_eq!(
        q(
            &db,
            "SELECT city, count(*) FROM sales GROUP BY 1 ORDER BY 1"
        ),
        ["Москва|2", "Питер|1", "Чита|3"]
    );
    assert!(err(&db, "SELECT city FROM sales GROUP BY 5").contains("в SELECT их 1"));
}

#[test]
fn date_trunc_buckets_unix_time() {
    let db = shop();
    // 1700000000 = 2023-11-14 22:13:20 UTC → минута обрезает до :13:00.
    assert_eq!(
        q(&db, "SELECT date_trunc('minute', ts) FROM sales LIMIT 1"),
        ["1699999980"]
    );
    assert_eq!(
        q(&db, "SELECT date_trunc('hour', ts) FROM sales LIMIT 1"),
        ["1699999200"]
    );
    assert_eq!(
        q(&db, "SELECT date_trunc('day', ts) FROM sales LIMIT 1"),
        ["1699920000"]
    );
    assert!(err(&db, "SELECT date_trunc('week', ts) FROM sales").contains("'week'"));
}

#[test]
fn scalar_functions_on_an_empty_selection_return_nothing() {
    let db = shop();
    // Литерал внутри функции не должен «оживлять» пустую выборку.
    assert!(q(
        &db,
        "SELECT if(qty > 0, 'да', 'нет') FROM sales WHERE id > 100"
    )
    .is_empty());
    assert!(q(
        &db,
        "SELECT substring(city, 1, 2) FROM sales WHERE id > 100"
    )
    .is_empty());
    assert!(q(&db, "SELECT round(price, 2) FROM sales WHERE id > 100").is_empty());
    assert!(q(
        &db,
        "SELECT date_trunc('hour', ts) FROM sales WHERE id > 100"
    )
    .is_empty());
    assert!(q(
        &db,
        "SELECT CASE WHEN qty > 0 THEN city ELSE '' END FROM sales WHERE id > 100"
    )
    .is_empty());
}

#[test]
fn parallel_aggregate_path_gives_the_same_answers_as_serial() {
    // Порог распараллеливания агрегатов — 65536 строк; берём заведомо больше.
    let db = Database::new();
    db.execute("CREATE TABLE big (g INT, x INT, y DOUBLE, s TEXT)")
        .unwrap();
    let n: i64 = 70_000;
    let rows: Vec<String> = (0..n)
        .map(|i| format!("({}, {i}, {}.5, 'k{}')", i % 7, i, i % 13))
        .collect();
    db.execute(&format!("INSERT INTO big VALUES {}", rows.join(",")))
        .unwrap();

    // Эталон считается здесь, а не другим запросом: сравнивать движок с самим
    // собой бессмысленно, если ошибка общая для обеих веток.
    let sum_x: i64 = (0..n).sum();
    let sum_y: f64 = (0..n).map(|i| i as f64 + 0.5).sum();
    assert_eq!(
        q(
            &db,
            "SELECT count(*), sum(x), min(x), max(x), sum(y), count(DISTINCT s) FROM big"
        ),
        [format!("{n}|{sum_x}|0|{}|{sum_y}|13", n - 1)]
    );

    // И то же самое с группировкой.
    let got = q(
        &db,
        "SELECT g, count(*), sum(x), max(x) FROM big GROUP BY g ORDER BY g",
    );
    assert_eq!(got.len(), 7);
    for (g, line) in got.iter().enumerate() {
        let g = g as i64;
        let members: Vec<i64> = (0..n).filter(|i| i % 7 == g).collect();
        let want = format!(
            "{g}|{}|{}|{}",
            members.len(),
            members.iter().sum::<i64>(),
            members.iter().max().unwrap()
        );
        assert_eq!(line, &want);
    }
}

#[test]
fn select_without_from_uses_a_single_synthetic_row() {
    let db = Database::new();
    assert_eq!(q(&db, "SELECT 1"), ["1"]);
    assert_eq!(q(&db, "SELECT 2 + 2 AS four"), ["4"]);
    assert_eq!(
        q(&db, "SELECT upper('привет'), length('привет')"),
        ["ПРИВЕТ|6"]
    );
    assert_eq!(q(&db, "SELECT count(*)"), ["1"]);
    assert_eq!(q(&db, "SELECT 1 WHERE 1 = 1"), ["1"]);
    assert!(q(&db, "SELECT 1 WHERE 1 = 0").is_empty());
    assert_eq!(q(&db, "SELECT if(1 > 0, 'да', 'нет')"), ["да"]);
    // Колонок нет — значит и ссылаться не на что; ошибка должна быть понятной.
    assert!(err(&db, "SELECT x").contains("нет колонки 'x'"));
}

#[test]
fn from_clause_still_rejects_joins_and_extra_tables() {
    let db = shop();
    assert!(err(&db, "SELECT * FROM sales, sales").contains("не больше одной таблицы"));
    assert!(err(&db, "SELECT * FROM sales JOIN sales ON 1 = 1").contains("не больше одной таблицы"));
}
