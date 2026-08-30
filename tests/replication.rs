use std::sync::Arc;
use std::time::Duration;
use veldb::exec::render;
use veldb::replication::{bootstrap, follow, pull_once};
use veldb::storage::Durability;
use veldb::{http, Database};

fn q(db: &Database, sql: &str) -> Vec<String> {
    db.execute(sql)
        .unwrap_or_else(|e| panic!("{sql}\n  -> {e:#}"))
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect()
}

/// Первичный узел на свободном порту. Он обязан быть с каталогом данных:
/// без WAL реплицировать нечего.
async fn primary(dir: &std::path::Path) -> (Arc<Database>, String) {
    let db = Arc::new(Database::open(dir, Durability::Buffered).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = http::router(db.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (db, addr.to_string())
}

fn seed(db: &Database) {
    db.execute("CREATE TABLE t (id INT, name TEXT, e VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1,'один','[1,2]'),(2,'два','[3,4]')")
        .unwrap();
}

#[tokio::test]
async fn bootstrap_copies_full_state() {
    let dir = tempfile::tempdir().unwrap();
    let (p, host) = primary(dir.path()).await;
    seed(&p);

    let r = Database::new();
    let lsn = bootstrap(&r, &host).await.unwrap();
    assert_eq!(lsn, p.next_lsn() - 1);
    assert_eq!(q(&r, "SELECT * FROM t"), q(&p, "SELECT * FROM t"));
}

#[tokio::test]
async fn incremental_pull_applies_new_writes() {
    let dir = tempfile::tempdir().unwrap();
    let (p, host) = primary(dir.path()).await;
    seed(&p);

    let r = Database::new();
    let mut lsn = bootstrap(&r, &host).await.unwrap();

    p.execute("INSERT INTO t VALUES (3,'три','[5,6]')").unwrap();
    p.execute("CREATE TABLE more (a INT)").unwrap();
    p.execute("INSERT INTO more VALUES (42)").unwrap();

    let (new_lsn, applied) = pull_once(&r, &host, lsn).await.unwrap();
    assert_eq!(applied, 3, "три изменения — три записи WAL");
    lsn = new_lsn;
    assert_eq!(q(&r, "SELECT id FROM t"), ["1", "2", "3"]);
    assert_eq!(q(&r, "SELECT a FROM more"), ["42"]);

    // Повторный опрос без изменений ничего не делает и не двигает LSN.
    let (same, n) = pull_once(&r, &host, lsn).await.unwrap();
    assert_eq!((same, n), (lsn, 0));
}

#[tokio::test]
async fn ddl_including_drop_is_replicated() {
    let dir = tempfile::tempdir().unwrap();
    let (p, host) = primary(dir.path()).await;
    seed(&p);
    let r = Database::new();
    let lsn = bootstrap(&r, &host).await.unwrap();

    p.execute("CREATE TABLE gone (a INT)").unwrap();
    p.execute("DROP TABLE gone").unwrap();
    p.execute("DROP TABLE t").unwrap();

    let (_, n) = pull_once(&r, &host, lsn).await.unwrap();
    assert_eq!(n, 3);
    assert!(
        r.table_names().is_empty(),
        "осталось: {:?}",
        r.table_names()
    );
}

#[tokio::test]
async fn wal_gap_after_primary_snapshot_forces_resync() {
    let dir = tempfile::tempdir().unwrap();
    let (p, host) = primary(dir.path()).await;
    seed(&p);
    let r = Database::new();
    let lsn = bootstrap(&r, &host).await.unwrap();

    // Первичный узел ушёл вперёд и обрезал WAL — догонять по нему уже нельзя.
    p.execute("INSERT INTO t VALUES (3,'три','[5,6]')").unwrap();
    p.snapshot().unwrap();
    p.execute("INSERT INTO t VALUES (4,'четыре','[7,8]')")
        .unwrap();

    let e = pull_once(&r, &host, lsn).await.unwrap_err();
    assert!(format!("{e:#}").contains("разрыв в WAL"), "получили: {e:#}");

    // Лечится полной пересинхронизацией — и после неё состояния совпадают.
    let lsn = bootstrap(&r, &host).await.unwrap();
    pull_once(&r, &host, lsn).await.unwrap();
    assert_eq!(q(&r, "SELECT id FROM t"), q(&p, "SELECT id FROM t"));
}

#[tokio::test]
async fn follow_loop_converges_and_makes_replica_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let (p, host) = primary(dir.path()).await;
    seed(&p);

    let r = Arc::new(Database::new());
    let rc = r.clone();
    let h = host.clone();
    let task = tokio::spawn(async move { follow(rc, &h, Duration::from_millis(20)).await });

    p.execute("INSERT INTO t VALUES (3,'три','[5,6]')").unwrap();

    let mut converged = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if r.table("t").is_some() && q(&r, "SELECT count(*) FROM t") == ["3"] {
            converged = true;
            break;
        }
    }
    assert!(converged, "реплика не догнала первичный узел за 2 секунды");

    // Реплика не должна принимать записи от клиентов.
    let e = format!(
        "{:#}",
        r.execute("INSERT INTO t VALUES (9,'x','[0,0]')")
            .unwrap_err()
    );
    assert!(e.contains("реплика"), "получили: {e}");
    assert!(format!("{:#}", r.execute("CREATE TABLE z (a INT)").unwrap_err()).contains("реплика"));
    // Чтение при этом работает.
    assert_eq!(q(&r, "SELECT id FROM t"), ["1", "2", "3"]);

    task.abort();
}

#[tokio::test]
async fn replica_survives_primary_going_away() {
    let dir = tempfile::tempdir().unwrap();
    let (p, host) = primary(dir.path()).await;
    seed(&p);
    let r = Database::new();
    let lsn = bootstrap(&r, &host).await.unwrap();

    // Недоступный узел — это ошибка запроса, а не паника и не потеря состояния.
    let e = pull_once(&r, "127.0.0.1:1", lsn).await.unwrap_err();
    assert!(format!("{e:#}").contains("подключение"), "получили: {e:#}");
    assert_eq!(q(&r, "SELECT count(*) FROM t"), ["2"]);
}
