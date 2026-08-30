use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use veldb::exec::render;
use veldb::storage::Durability;
use veldb::Database;

fn q(db: &Database, sql: &str) -> Vec<String> {
    db.execute(sql)
        .unwrap_or_else(|e| panic!("{sql}\n  -> {e:#}"))
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect()
}

fn seed(db: &Database) {
    db.execute("CREATE TABLE t (id INT, name TEXT, w DOUBLE, ok BOOL, e VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1,'один',1.5,true,'[1,2]'),(2,'два',2.5,false,'[3,4]')")
        .unwrap();
}

#[test]
fn wal_replay_restores_state_without_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        seed(&db);
    } // «падение»: снапшот не делали
    let db = Database::open(dir.path(), Durability::Fsync).unwrap();
    assert_eq!(
        q(&db, "SELECT * FROM t"),
        ["1|один|1.5|true|[1,2]", "2|два|2.5|false|[3,4]"]
    );
}

#[test]
fn snapshot_then_more_writes_then_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        seed(&db);
        db.snapshot().unwrap();
        db.execute("INSERT INTO t VALUES (3,'три',3.5,true,'[5,6]')")
            .unwrap();
    }
    let db = Database::open(dir.path(), Durability::Fsync).unwrap();
    // Ни одной потерянной и ни одной задвоенной строки: снапшот покрыл первые две,
    // WAL — третью.
    assert_eq!(q(&db, "SELECT id FROM t"), ["1", "2", "3"]);
}

#[test]
fn repeated_restart_does_not_duplicate_rows() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        seed(&db);
    }
    for _ in 0..3 {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        assert_eq!(q(&db, "SELECT count(*) FROM t"), ["2"]);
        db.snapshot().unwrap();
    }
    let db = Database::open(dir.path(), Durability::Fsync).unwrap();
    assert_eq!(q(&db, "SELECT count(*) FROM t"), ["2"]);
}

#[test]
fn torn_wal_tail_is_dropped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        seed(&db);
        db.execute("INSERT INTO t VALUES (3,'три',3.5,true,'[5,6]')")
            .unwrap();
    }
    // Имитируем kill -9 в момент записи: обрываем последний кадр посередине.
    let wal = dir.path().join("wal.log");
    let len = std::fs::metadata(&wal).unwrap().len();
    let f = OpenOptions::new().write(true).open(&wal).unwrap();
    f.set_len(len - 12).unwrap();

    let db = Database::open(dir.path(), Durability::Fsync).unwrap();
    assert_eq!(
        q(&db, "SELECT id FROM t"),
        ["1", "2"],
        "обрезанная запись должна быть отброшена"
    );
    // База пригодна к работе дальше, а не только к чтению.
    db.execute("INSERT INTO t VALUES (9,'девять',9.0,true,'[7,8]')")
        .unwrap();
    assert_eq!(q(&db, "SELECT id FROM t"), ["1", "2", "9"]);
}

#[test]
fn bit_flip_in_wal_stops_replay_at_damage() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        seed(&db);
        db.execute("INSERT INTO t VALUES (3,'три',3.5,true,'[5,6]')")
            .unwrap();
    }
    let wal = dir.path().join("wal.log");
    let len = std::fs::metadata(&wal).unwrap().len();
    let mut f = OpenOptions::new().write(true).open(&wal).unwrap();
    f.seek(SeekFrom::Start(len - 4)).unwrap();
    f.write_all(&[0xAA]).unwrap();
    drop(f);

    let db = Database::open(dir.path(), Durability::Fsync).unwrap();
    assert_eq!(
        q(&db, "SELECT id FROM t"),
        ["1", "2"],
        "битую запись CRC обязан поймать"
    );
}

#[test]
fn corrupt_snapshot_refuses_to_open_rather_than_lie() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        seed(&db);
        db.snapshot().unwrap();
    }
    let snap = dir.path().join("snapshot.vdb");
    let mut bytes = std::fs::read(&snap).unwrap();
    let n = bytes.len();
    bytes[n / 2] ^= 0xFF;
    std::fs::write(&snap, &bytes).unwrap();

    let e = match Database::open(dir.path(), Durability::Fsync) {
        Ok(_) => panic!("битый снапшот открылся как исправный"),
        Err(e) => e,
    };
    assert!(format!("{e:#}").contains("CRC"), "получили: {e:#}");
}

#[test]
fn ddl_is_persisted_too() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path(), Durability::Fsync).unwrap();
        seed(&db);
        db.execute("CREATE TABLE gone (a INT)").unwrap();
        db.execute("DROP TABLE gone").unwrap();
        db.execute("CREATE TABLE kept (a INT)").unwrap();
    }
    let db = Database::open(dir.path(), Durability::Fsync).unwrap();
    assert_eq!(q(&db, "SHOW TABLES"), ["kept|0", "t|2"]);
}

#[test]
fn all_column_types_survive_snapshot_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let expected;
    {
        let db = Database::open(dir.path(), Durability::Buffered).unwrap();
        db.execute("CREATE TABLE t (i INT, f DOUBLE, b BOOL, s TEXT, v VECTOR(3))")
            .unwrap();
        db.execute(
            "INSERT INTO t VALUES
             (-9007199254740993, -0.5, true, '', '[0,0,0]'),
             (0, 1e10, false, 'строка с юникодом 🐢', '[1.5,-2.5,3.5]')",
        )
        .unwrap();
        expected = q(&db, "SELECT * FROM t");
        db.snapshot().unwrap();
    }
    let db = Database::open(dir.path(), Durability::Buffered).unwrap();
    assert_eq!(q(&db, "SELECT * FROM t"), expected);
}

#[test]
fn in_memory_database_refuses_snapshot_clearly() {
    let db = Database::new();
    assert!(format!("{:#}", db.snapshot().unwrap_err()).contains("каталога данных"));
}

#[test]
fn memory_limit_refuses_writes_instead_of_letting_the_process_die() {
    let db = Database::new();
    db.execute("CREATE TABLE t (x INT)").unwrap();
    let rows: Vec<String> = (0..1000).map(|i| format!("({i})")).collect();
    db.execute(&format!("INSERT INTO t VALUES {}", rows.join(",")))
        .unwrap();
    assert_eq!(db.bytes_used(), 8000);

    db.set_memory_limit(8000);
    let e = format!("{:#}", db.execute("INSERT INTO t VALUES (1)").unwrap_err());
    assert!(e.contains("предел памяти"), "получили: {e}");
    // Уже записанное осталось читаемым — это отказ в записи, а не потеря данных.
    assert_eq!(q(&db, "SELECT count(*) FROM t"), ["1000"]);

    db.set_memory_limit(0);
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(q(&db, "SELECT count(*) FROM t"), ["1001"]);
}
