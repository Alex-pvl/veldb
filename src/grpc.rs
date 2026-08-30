//! gRPC-интерфейс поверх той же `Database`, что и REST.

use crate::column::{DataType, Value};
use crate::db::Database;
use anyhow::anyhow;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub mod pb {
    tonic::include_proto!("veldb");
}

use pb::veldb_server::{Veldb, VeldbServer};

pub struct Service {
    db: Arc<Database>,
}

impl Service {
    pub fn new(db: Arc<Database>) -> VeldbServer<Service> {
        VeldbServer::new(Service { db })
    }
}

/// Ошибка запроса — `INVALID_ARGUMENT`, а не `INTERNAL`: клиент должен видеть,
/// что чинить надо запрос, а не звонить дежурному.
fn bad(e: anyhow::Error) -> Status {
    Status::invalid_argument(format!("{e:#}"))
}

fn to_pb(v: &Value) -> pb::Value {
    use pb::value::Kind;
    pb::Value {
        kind: Some(match v {
            Value::I64(x) => Kind::I(*x),
            Value::F64(x) => Kind::F(*x),
            Value::Bool(x) => Kind::B(*x),
            Value::Str(x) => Kind::S(x.clone()),
            Value::Vector(x) => Kind::V(pb::Vector { values: x.clone() }),
        }),
    }
}

/// Тип берётся из схемы, а не из того, что прислал клиент: иначе `1` вместо `1.0`
/// молча создаёт колонку не того типа.
fn from_pb(v: &pb::Value, ty: DataType) -> anyhow::Result<Value> {
    use pb::value::Kind;
    let kind = v.kind.as_ref().ok_or_else(|| anyhow!("пустое значение"))?;
    Ok(match (ty, kind) {
        (DataType::I64, Kind::I(x)) => Value::I64(*x),
        (DataType::F64, Kind::F(x)) => Value::F64(*x),
        (DataType::F64, Kind::I(x)) => Value::F64(*x as f64),
        (DataType::Bool, Kind::B(x)) => Value::Bool(*x),
        (DataType::Str, Kind::S(x)) => Value::Str(x.clone()),
        (DataType::Vector(dim), Kind::V(x)) => {
            if x.values.len() != dim {
                return Err(anyhow!(
                    "вектор длины {} в колонку VECTOR({dim})",
                    x.values.len()
                ));
            }
            Value::Vector(x.values.clone())
        }
        (ty, _) => return Err(anyhow!("значение не подходит колонке {}", ty.name())),
    })
}

#[tonic::async_trait]
impl Veldb for Service {
    async fn query(
        &self,
        req: Request<pb::QueryRequest>,
    ) -> Result<Response<pb::QueryReply>, Status> {
        let sql = req.into_inner().sql;
        let db = self.db.clone();
        let start = std::time::Instant::now();
        // Исполнение синхронное и может быть долгим — уводим с reactor-потока.
        let r = tokio::task::spawn_blocking(move || db.execute(&sql))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(bad)?;
        Ok(Response::new(pb::QueryReply {
            columns: r.columns,
            types: r.types.iter().map(|t| t.name()).collect(),
            rows: r
                .rows
                .iter()
                .map(|row| pb::Row {
                    values: row.iter().map(to_pb).collect(),
                })
                .collect(),
            elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        }))
    }

    async fn insert(
        &self,
        req: Request<pb::InsertRequest>,
    ) -> Result<Response<pb::InsertReply>, Status> {
        let req = req.into_inner();
        let handle = self
            .db
            .table(&req.table)
            .ok_or_else(|| Status::not_found(format!("таблица '{}' не найдена", req.table)))?;
        let schema = handle.read().unwrap().schema.clone();
        let rows = req
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                if row.values.len() != schema.len() {
                    return Err(anyhow!(
                        "строка {i}: {} значений при {} колонках",
                        row.values.len(),
                        schema.len()
                    ));
                }
                row.values
                    .iter()
                    .zip(&schema.fields)
                    .map(|(v, f)| {
                        from_pb(v, f.ty)
                            .map_err(|e| anyhow!("строка {i}, колонка '{}': {e:#}", f.name))
                    })
                    .collect()
            })
            .collect::<anyhow::Result<Vec<Vec<Value>>>>()
            .map_err(bad)?;

        let db = self.db.clone();
        let table = req.table.clone();
        let n = tokio::task::spawn_blocking(move || db.insert_rows(&table, &rows))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(bad)?;
        Ok(Response::new(pb::InsertReply { inserted: n as u64 }))
    }

    async fn health(
        &self,
        _: Request<pb::HealthRequest>,
    ) -> Result<Response<pb::HealthReply>, Status> {
        let names = self.db.table_names();
        let stats: Vec<(usize, usize)> = names
            .iter()
            .filter_map(|n| self.db.table(n))
            .map(|t| {
                let t = t.read().unwrap();
                (t.nrows(), t.bytes_used())
            })
            .collect();
        Ok(Response::new(pb::HealthReply {
            version: env!("CARGO_PKG_VERSION").to_string(),
            tables: names.len() as u64,
            rows: stats.iter().map(|s| s.0 as u64).sum(),
            bytes_used: stats.iter().map(|s| s.1 as u64).sum(),
            next_lsn: self.db.next_lsn(),
            persistent: self.db.is_persistent(),
        }))
    }

    async fn schema(
        &self,
        _: Request<pb::SchemaRequest>,
    ) -> Result<Response<pb::SchemaReply>, Status> {
        let tables = self
            .db
            .table_names()
            .into_iter()
            .filter_map(|n| {
                let t = self.db.table(&n)?;
                let t = t.read().unwrap();
                Some(pb::TableInfo {
                    name: t.name.clone(),
                    rows: t.nrows() as u64,
                    columns: t
                        .schema
                        .fields
                        .iter()
                        .map(|f| pb::ColumnInfo {
                            name: f.name.clone(),
                            r#type: f.ty.name(),
                        })
                        .collect(),
                })
            })
            .collect();
        Ok(Response::new(pb::SchemaReply { tables }))
    }
}
