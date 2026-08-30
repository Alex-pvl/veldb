use serde_json::json;
use std::sync::Arc;
use veldb::cli::{complete, format_table, is_complete, Catalog};
use veldb::{client, http, Database};

fn catalog() -> Catalog {
    Catalog::from_schema_json(&json!({
        "tables": [
            { "name": "sales", "columns": [{"name":"id"},{"name":"city"},{"name":"price"}] },
            { "name": "docs",  "columns": [{"name":"id"},{"name":"embedding"}] }
        ]
    }))
}

fn names(line: &str) -> Vec<String> {
    complete(line, line.len(), &catalog()).1
}

#[test]
fn after_from_only_tables_are_offered() {
    assert_eq!(names("SELECT * FROM "), ["docs", "sales"]);
    assert_eq!(names("SELECT * FROM s"), ["sales"]);
    assert_eq!(names("INSERT INTO d"), ["docs"]);
    // Позиция дополнения — начало слова, а не курсор.
    assert_eq!(complete("SELECT * FROM sa", 16, &catalog()).0, 14);
}

#[test]
fn elsewhere_columns_and_keywords_are_offered() {
    let c = names("SELECT ci");
    assert!(c.contains(&"city".to_string()));
    let k = names("SELECT * FROM sales WHE");
    assert!(k.contains(&"WHERE".to_string()));
    let f = names("SELECT l2_");
    assert_eq!(f, ["l2_distance("]);
    assert!(names("SELECT emb").contains(&"embedding".to_string()));
}

#[test]
fn completion_is_case_insensitive_and_deduplicated() {
    assert!(names("select CI").contains(&"city".to_string()));
    // `id` есть в обеих таблицах, но в списке должен быть один раз.
    let ids: Vec<_> = names("SELECT i")
        .into_iter()
        .filter(|s| s == "id")
        .collect();
    assert_eq!(ids.len(), 1);
}

#[test]
fn meta_commands_complete_too() {
    assert_eq!(names("\\d"), ["\\d", "\\dt"]);
    assert_eq!(names("\\t"), ["\\timing"]);
}

#[test]
fn empty_prefix_offers_everything_without_panicking() {
    assert!(!names("").is_empty());
    assert!(!names("SELECT ").is_empty());
    assert_eq!(complete("", 0, &Catalog::default()).0, 0);
}

#[test]
fn statement_is_complete_only_on_a_semicolon_outside_quotes() {
    assert!(is_complete("SELECT 1;"));
    assert!(is_complete("SELECT 1; \n"));
    assert!(!is_complete("SELECT 1"));
    assert!(!is_complete("SELECT 'a;b'"));
    assert!(is_complete("SELECT 'a;b';"));
    // Экранированная кавычка внутри литерала не закрывает строку.
    assert!(!is_complete("SELECT 'it''s; here'"));
    assert!(is_complete("SELECT 'it''s; here';"));
    assert!(!is_complete("SELECT 1; SELECT 2"));
}

#[test]
fn table_output_aligns_by_characters_not_bytes() {
    let out = format_table(
        &["город".into(), "n".into()],
        &[
            vec!["Чита".into(), "3".into()],
            vec!["Москва".into(), "12".into()],
        ],
    );
    let lines: Vec<&str> = out.lines().collect();
    // Все строки рамки одной ширины в символах — иначе кириллица её ломает.
    let width = lines[0].chars().count();
    assert!(lines.iter().all(|l| l.chars().count() == width), "{out}");
    assert!(out.contains("Москва"));
}

#[test]
fn table_output_handles_no_rows() {
    let out = format_table(&["a".into()], &[]);
    assert_eq!(out.lines().count(), 4);
}

// --- сквозная проверка клиентского транспорта -------------------------------

async fn serve(db: Database) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = http::router(Arc::new(db));
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr.to_string()
}

#[tokio::test]
async fn client_talks_to_the_real_server() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INT, city TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1,'Чита')").unwrap();
    let host = serve(db).await;

    let body = client::get(&host, "/schema").await.unwrap();
    let cat = Catalog::from_schema_json(&serde_json::from_slice(&body).unwrap());
    assert_eq!(cat.tables, ["t"]);
    assert_eq!(cat.columns["t"], ["id", "city"]);

    let body = client::post_json(&host, "/query", &json!({"sql": "SELECT city FROM t"}))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["rows"][0][0], "Чита");
}

#[tokio::test]
async fn client_surfaces_server_error_text_not_just_status() {
    let host = serve(Database::new()).await;
    let e = client::post_json(&host, "/query", &json!({"sql": "SELECT * FROM nope"}))
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("'nope' не найдена"),
        "получили: {e:#}"
    );
}

#[tokio::test]
async fn describe_used_by_backslash_d_is_supported() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INT, e VECTOR(4))").unwrap();
    let r = db.execute("DESCRIBE t").unwrap();
    assert_eq!(r.columns, ["column", "type"]);
    assert_eq!(r.rows.len(), 2);
}
