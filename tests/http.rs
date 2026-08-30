use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use veldb::http::router;
use veldb::Database;

struct Api(axum::Router);

impl Api {
    fn new(db: Database) -> Api {
        Api(router(Arc::new(db)))
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(uri);
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let resp = self.0.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 16 * 1024 * 1024).await.unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, v)
    }

    async fn sql(&self, sql: &str) -> Value {
        let (s, v) = self
            .call("POST", "/query", Some(json!({ "sql": sql })))
            .await;
        assert_eq!(s, StatusCode::OK, "{sql} -> {v}");
        v
    }
}

fn seeded() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INT, name TEXT, w DOUBLE, ok BOOL, e VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1,'один',1.5,true,'[1,2]'),(2,'два',2.5,false,'[3,4]')")
        .unwrap();
    db
}

#[tokio::test]
async fn health_reports_real_numbers() {
    let api = Api::new(seeded());
    let (s, v) = api.call("GET", "/health", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "ok");
    assert_eq!(v["tables"], 1);
    assert_eq!(v["rows"], 2);
    assert_eq!(v["persistent"], false);
    assert!(v["bytes_used"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn schema_lists_columns_with_readable_types() {
    let api = Api::new(seeded());
    let (_, v) = api.call("GET", "/schema", None).await;
    let cols = &v["tables"][0]["columns"];
    assert_eq!(cols[0]["name"], "id");
    assert_eq!(cols[0]["type"], "INT");
    assert_eq!(cols[4]["type"], "VECTOR(2)");
}

#[tokio::test]
async fn query_returns_plain_json_scalars_not_tagged_enums() {
    let api = Api::new(seeded());
    let v = api
        .sql("SELECT id, name, w, ok, e FROM t WHERE id = 1")
        .await;
    assert_eq!(v["row_count"], 1);
    assert_eq!(v["rows"][0], json!([1, "один", 1.5, true, [1.0, 2.0]]));
    assert_eq!(
        v["types"],
        json!(["INT", "TEXT", "DOUBLE", "BOOL", "VECTOR(2)"])
    );
    assert!(v["elapsed_ms"].as_f64().is_some());
}

#[tokio::test]
async fn non_finite_floats_stay_valid_json() {
    let api = Api::new(seeded());
    let v = api.sql("SELECT w / 0 FROM t LIMIT 1").await;
    assert_eq!(v["rows"][0][0], "inf");
}

#[tokio::test]
async fn bad_sql_is_client_error_with_message() {
    let api = Api::new(seeded());
    let (s, v) = api
        .call(
            "POST",
            "/query",
            Some(json!({ "sql": "SELECT nope FROM t" })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("нет колонки 'nope'"));
}

#[tokio::test]
async fn insert_endpoint_accepts_typed_json_rows() {
    let api = Api::new(seeded());
    let (s, v) = api
        .call(
            "POST",
            "/insert",
            Some(json!({ "table": "t", "rows": [[3, "три", 3.5, true, [5.0, 6.0]]] })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["inserted"], 1);
    assert_eq!(api.sql("SELECT count(*) FROM t").await["rows"][0][0], 3);
}

#[tokio::test]
async fn insert_rejects_wrong_types_and_names_the_column() {
    let api = Api::new(seeded());
    let (s, v) = api
        .call(
            "POST",
            "/insert",
            Some(json!({ "table": "t", "rows": [[1, "x", 1.0, true, [1.0]]] })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let msg = v["error"].as_str().unwrap();
    assert!(msg.contains("'e'") && msg.contains("VECTOR(2)"), "{msg}");
    // Плохая пачка не должна применяться частично.
    assert_eq!(api.sql("SELECT count(*) FROM t").await["rows"][0][0], 2);
}

#[tokio::test]
async fn insert_into_missing_table_is_client_error() {
    let api = Api::new(seeded());
    let (s, v) = api
        .call(
            "POST",
            "/insert",
            Some(json!({ "table": "nope", "rows": [] })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("не найдена"));
}

#[tokio::test]
async fn ddl_over_http_works_end_to_end() {
    let api = Api::new(Database::new());
    api.sql("CREATE TABLE docs (id INT, e VECTOR(3))").await;
    api.sql("INSERT INTO docs VALUES (1,'[1,0,0]'),(2,'[0,1,0]')")
        .await;
    let v = api
        .sql("SELECT id FROM docs ORDER BY l2_distance(e, '[0.9,0.1,0]') LIMIT 1")
        .await;
    assert_eq!(v["rows"][0][0], 1);
}

#[tokio::test]
async fn snapshot_endpoint_requires_a_data_dir() {
    let api = Api::new(Database::new());
    let (s, v) = api.call("POST", "/snapshot", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("каталога данных"));
}
