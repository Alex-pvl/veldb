//! HTTP REST-интерфейс.

use crate::db::Database;
use crate::exec::QueryResult;
use crate::storage::WalRecord;
use anyhow::{anyhow, Result};
use axum::extract::{Query as UrlQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json2};
use std::sync::Arc;

pub type Shared = Arc<Database>;

/// Ошибка запроса — это ошибка клиента (400), а не сбой сервера: SQL приходит
/// снаружи, и падать пятисоткой на каждую опечатку неправильно.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn router(db: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/schema", get(schema))
        .route("/query", post(query))
        .route("/insert", post(insert))
        .route("/snapshot", post(snapshot))
        .route("/replication/wal", get(wal))
        .route("/replication/snapshot", get(replication_snapshot))
        .with_state(db)
}

async fn health(State(db): State<Shared>) -> Json<Json2> {
    let names = db.table_names();
    let rows: usize = names
        .iter()
        .filter_map(|n| db.table(n))
        .map(|t| t.read().unwrap().nrows())
        .sum();
    let bytes: usize = names
        .iter()
        .filter_map(|n| db.table(n))
        .map(|t| t.read().unwrap().bytes_used())
        .sum();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "tables": names.len(),
        "rows": rows,
        "bytes_used": bytes,
        "persistent": db.is_persistent(),
        "next_lsn": db.next_lsn(),
    }))
}

async fn schema(State(db): State<Shared>) -> Json<Json2> {
    let tables: Vec<Json2> = db
        .table_names()
        .into_iter()
        .filter_map(|n| {
            let t = db.table(&n)?;
            let t = t.read().unwrap();
            Some(json!({
                "name": t.name,
                "rows": t.nrows(),
                "bytes_used": t.bytes_used(),
                "columns": t.schema.fields.iter()
                    .map(|f| json!({ "name": f.name, "type": f.ty.name() }))
                    .collect::<Vec<_>>(),
            }))
        })
        .collect();
    Json(json!({ "tables": tables }))
}

#[derive(Deserialize)]
struct QueryBody {
    sql: String,
}

#[derive(Serialize)]
struct QueryReply {
    columns: Vec<String>,
    types: Vec<String>,
    rows: Vec<Vec<crate::column::Value>>,
    row_count: usize,
    elapsed_ms: f64,
}

async fn query(
    State(db): State<Shared>,
    Json(body): Json<QueryBody>,
) -> ApiResult<Json<QueryReply>> {
    let start = std::time::Instant::now();
    // Запрос может быть тяжёлым и целиком синхронный: держать его в reactor-потоке
    // нельзя, иначе он блокирует все остальные соединения.
    let r: QueryResult = tokio::task::spawn_blocking(move || db.execute(&body.sql))
        .await
        .map_err(|e| anyhow!(e))??;
    Ok(Json(QueryReply {
        columns: r.columns,
        types: r.types.iter().map(|t| t.name()).collect(),
        row_count: r.rows.len(),
        rows: r.rows,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
    }))
}

#[derive(Deserialize)]
struct InsertBody {
    table: String,
    /// Строки как массивы значений в порядке колонок таблицы.
    rows: Vec<Vec<Json2>>,
}

async fn insert(State(db): State<Shared>, Json(body): Json<InsertBody>) -> ApiResult<Json<Json2>> {
    let handle = db
        .table(&body.table)
        .ok_or_else(|| anyhow!("таблица '{}' не найдена", body.table))?;
    let schema = handle.read().unwrap().schema.clone();
    let rows = body
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            if r.len() != schema.len() {
                return Err(anyhow!(
                    "строка {i}: {} значений при {} колонках",
                    r.len(),
                    schema.len()
                ));
            }
            r.iter()
                .zip(&schema.fields)
                .map(|(v, f)| {
                    crate::column::Value::from_json(v, f.ty)
                        .map_err(|e| anyhow!("строка {i}, колонка '{}': {e:#}", f.name))
                })
                .collect()
        })
        .collect::<Result<Vec<Vec<_>>>>()?;

    let n = tokio::task::spawn_blocking(move || db.insert_rows(&body.table, &rows))
        .await
        .map_err(|e| anyhow!(e))??;
    Ok(Json(json!({ "inserted": n })))
}

async fn snapshot(State(db): State<Shared>) -> ApiResult<Json<Json2>> {
    let lsn = tokio::task::spawn_blocking(move || db.snapshot())
        .await
        .map_err(|e| anyhow!(e))??;
    Ok(Json(json!({ "lsn": lsn })))
}

#[derive(Deserialize)]
pub struct WalQuery {
    #[serde(default)]
    pub after: u64,
}

/// Поток записей WAL для реплики. Отдаём JSON, а не сырой кадр: реплике нужен
/// только LSN и полезная нагрузка, а base64 здесь дешевле собственного протокола.
async fn wal(State(db): State<Shared>, UrlQuery(q): UrlQuery<WalQuery>) -> ApiResult<Json<Json2>> {
    let records = db.wal_since(q.after)?;
    let items: Vec<Json2> = records
        .iter()
        .map(|(lsn, payload)| json!({ "lsn": lsn, "payload": hex(payload) }))
        .collect();
    Ok(Json(json!({ "next_lsn": db.next_lsn(), "records": items })))
}

async fn replication_snapshot(State(db): State<Shared>) -> ApiResult<Vec<u8>> {
    let bytes = tokio::task::spawn_blocking(move || db.snapshot_bytes())
        .await
        .map_err(|e| anyhow!(e))??;
    Ok(bytes)
}

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(anyhow!("нечётная длина hex-строки"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!("hex: {e}")))
        .collect()
}

/// Разбор записи WAL на стороне реплики.
pub fn decode_record(payload: &[u8], db: &Database) -> Result<WalRecord> {
    use crate::sql::Catalog;
    WalRecord::decode(payload, &|t| db.schema_of(t))
}
