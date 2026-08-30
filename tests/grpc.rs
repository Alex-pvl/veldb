use std::sync::Arc;
use tokio_stream::wrappers::TcpListenerStream;
use veldb::grpc::pb::{self, veldb_client::VeldbClient};
use veldb::{grpc, Database};

/// Поднимает настоящий сервер на свободном порту и возвращает подключённого клиента.
/// In-process мок здесь не годится: половина смысла gRPC — в кодировании на проводе.
async fn serve(db: Database) -> VeldbClient<tonic::transport::Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let svc = grpc::Service::new(Arc::new(db));
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    // Сервер поднимается асинхронно; несколько попыток надёжнее фиксированной паузы.
    for _ in 0..50 {
        if let Ok(c) = VeldbClient::connect(format!("http://{addr}")).await {
            return c;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("сервер не поднялся на {addr}");
}

fn seeded() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INT, name TEXT, w DOUBLE, ok BOOL, e VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1,'один',1.5,true,'[1,2]'),(2,'два',2.5,false,'[3,4]')")
        .unwrap();
    db
}

fn i(v: i64) -> pb::Value {
    pb::Value {
        kind: Some(pb::value::Kind::I(v)),
    }
}
fn f(v: f64) -> pb::Value {
    pb::Value {
        kind: Some(pb::value::Kind::F(v)),
    }
}
fn s(v: &str) -> pb::Value {
    pb::Value {
        kind: Some(pb::value::Kind::S(v.into())),
    }
}
fn b(v: bool) -> pb::Value {
    pb::Value {
        kind: Some(pb::value::Kind::B(v)),
    }
}
fn vec(v: &[f32]) -> pb::Value {
    pb::Value {
        kind: Some(pb::value::Kind::V(pb::Vector { values: v.to_vec() })),
    }
}

#[tokio::test]
async fn query_round_trips_every_value_kind() {
    let mut c = serve(seeded()).await;
    let r = c
        .query(pb::QueryRequest {
            sql: "SELECT id, name, w, ok, e FROM t WHERE id = 2".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.columns, ["id", "name", "w", "ok", "e"]);
    assert_eq!(r.types, ["INT", "TEXT", "DOUBLE", "BOOL", "VECTOR(2)"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.rows[0].values,
        vec![i(2), s("два"), f(2.5), b(false), vec(&[3.0, 4.0])]
    );
}

#[tokio::test]
async fn insert_then_query_sees_the_rows() {
    let mut c = serve(seeded()).await;
    let n = c
        .insert(pb::InsertRequest {
            table: "t".into(),
            rows: vec![
                pb::Row {
                    values: vec![i(3), s("три"), f(3.5), b(true), vec(&[5.0, 6.0])],
                },
                pb::Row {
                    values: vec![i(4), s("четыре"), f(4.5), b(true), vec(&[7.0, 8.0])],
                },
            ],
        })
        .await
        .unwrap()
        .into_inner()
        .inserted;
    assert_eq!(n, 2);

    let r = c
        .query(pb::QueryRequest {
            sql: "SELECT count(*) FROM t".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.rows[0].values[0], i(4));
}

#[tokio::test]
async fn integer_literal_widens_into_double_column() {
    let mut c = serve(seeded()).await;
    // Клиенту не обязано быть известно, что 3 — это DOUBLE: тип берётся из схемы.
    c.insert(pb::InsertRequest {
        table: "t".into(),
        rows: vec![pb::Row {
            values: vec![i(9), s("девять"), i(3), b(true), vec(&[0.0, 0.0])],
        }],
    })
    .await
    .unwrap();
    let r = c
        .query(pb::QueryRequest {
            sql: "SELECT w FROM t WHERE id = 9".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.rows[0].values[0], f(3.0));
}

#[tokio::test]
async fn bad_query_is_invalid_argument_not_internal() {
    let mut c = serve(seeded()).await;
    let e = c
        .query(pb::QueryRequest {
            sql: "SELECT nope FROM t".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert!(e.message().contains("нет колонки 'nope'"));
}

#[tokio::test]
async fn insert_into_missing_table_is_not_found() {
    let mut c = serve(seeded()).await;
    let e = c
        .insert(pb::InsertRequest {
            table: "nope".into(),
            rows: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn wrong_vector_dimension_is_rejected_with_column_name() {
    let mut c = serve(seeded()).await;
    let e = c
        .insert(pb::InsertRequest {
            table: "t".into(),
            rows: vec![pb::Row {
                values: vec![i(5), s("x"), f(1.0), b(true), vec(&[1.0])],
            }],
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert!(e.message().contains("'e'"), "{}", e.message());
}

#[tokio::test]
async fn health_and_schema_match_the_database() {
    let mut c = serve(seeded()).await;
    let h = c.health(pb::HealthRequest {}).await.unwrap().into_inner();
    assert_eq!(h.tables, 1);
    assert_eq!(h.rows, 2);
    assert!(!h.persistent);

    let s = c.schema(pb::SchemaRequest {}).await.unwrap().into_inner();
    assert_eq!(s.tables.len(), 1);
    assert_eq!(s.tables[0].name, "t");
    assert_eq!(s.tables[0].columns.len(), 5);
    assert_eq!(s.tables[0].columns[4].r#type, "VECTOR(2)");
}

#[tokio::test]
async fn vector_search_over_grpc() {
    let mut c = serve(seeded()).await;
    let r = c
        .query(pb::QueryRequest {
            sql: "SELECT id FROM t ORDER BY l2_distance(e, '[3,4]') LIMIT 1".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.rows[0].values[0], i(2));
}
