//! Функциональные тесты: поднимают настоящий процесс `veldb` и дёргают его
//! настоящим `veldbctl`, curl-подобным клиентом и gRPC-клиентом.
//!
//! Остальные файлы тестов работают с библиотекой напрямую — быстро, но мимо
//! бинарников. Ошибка вида «`SELECT 1` не работает» жила именно в этом зазоре:
//! библиотечные тесты всегда писали `FROM`, потому что писал их тот же человек,
//! что и планировщик. Здесь запросы идут ровно так, как их набирает пользователь.

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use veldb::client;

/// Свободный порт: занимаем, узнаём номер, отпускаем. Окно гонки есть, но оно
/// в микросекунды, а альтернатива — фиксированные порты, которые ломают
/// параллельный запуск тестов.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Server {
    child: Child,
    pub http: String,
    pub grpc: String,
    pub dir: PathBuf,
    log: PathBuf,
    _tmp: Option<tempfile::TempDir>,
}

impl Server {
    fn start(extra: &[&str]) -> Server {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        Server::start_in(dir, Some(tmp), extra)
    }

    /// Запуск в заранее известном каталоге — нужен, чтобы перезапустить сервер
    /// поверх тех же данных и проверить восстановление.
    ///
    /// С повтором попыток: между «узнали свободный порт» и «сервер его занял»
    /// есть окно, в которое на нагруженной машине успевает влезть кто-то ещё.
    /// На ноутбуке это микросекунды, на раннере CI — нет: релиз 1.0.0 упал
    /// ровно на `Address already in use`.
    fn start_in(dir: PathBuf, tmp: Option<tempfile::TempDir>, extra: &[&str]) -> Server {
        let mut tmp = tmp;
        let mut last = String::new();
        for attempt in 1..=5 {
            match Server::try_start(&dir, extra) {
                Ok((child, http, grpc, log)) => {
                    return Server {
                        child,
                        http,
                        grpc,
                        dir,
                        log,
                        _tmp: tmp.take(),
                    }
                }
                Err(e) => {
                    last = format!("попытка {attempt}: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        panic!("сервер не запустился за 5 попыток. Последняя — {last}");
    }

    #[allow(clippy::type_complexity)]
    fn try_start(
        dir: &std::path::Path,
        extra: &[&str],
    ) -> Result<(Child, String, String, PathBuf), String> {
        let (http_port, grpc_port) = (free_port(), free_port());
        let log = dir.join(format!("server-{http_port}.log"));
        let out = std::fs::File::create(&log).map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;

        let mut child = Command::new(env!("CARGO_BIN_EXE_veldb"))
            .args(["--data", dir.to_str().unwrap()])
            .args(["--http", &format!("127.0.0.1:{http_port}")])
            .args(["--grpc", &format!("127.0.0.1:{grpc_port}")])
            .args(extra)
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .map_err(|e| format!("не удалось запустить veldb: {e}"))?;

        let http = format!("127.0.0.1:{http_port}");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // Занятый порт роняет процесс: ловим это сразу, а не по таймауту.
            if let Ok(Some(status)) = child.try_wait() {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                return Err(format!("процесс вышел с {status}: {}", text.trim()));
            }
            if std::net::TcpStream::connect(&http).is_ok() {
                return Ok((child, http, format!("127.0.0.1:{grpc_port}"), log));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                return Err(format!("не поднялся за 30 с: {}", text.trim()));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Останавливает сервер так же, как это делает systemd, — по SIGINT,
    /// чтобы отработала запись финального снапшота.
    fn stop_gracefully(mut self) -> PathBuf {
        let dir = self.dir.clone();
        unsafe {
            libc_kill(self.child.id() as i32);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.child.try_wait().unwrap() {
                Some(_) => break,
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    panic!("сервер не завершился по SIGINT:\n{}", self.log_text());
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        }
        self.keep_dir();
        dir
    }

    /// Убивает процесс как `kill -9`: без финального снапшота, только WAL.
    fn kill_hard(mut self) -> PathBuf {
        let dir = self.dir.clone();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.keep_dir();
        dir
    }

    /// Отвязывает каталог от TempDir: иначе Drop удалит данные, которые
    /// следующий запуск как раз и должен восстановить.
    fn keep_dir(&mut self) {
        if let Some(t) = self._tmp.take() {
            let _ = t.keep();
        }
    }
}

/// `Command` не умеет посылать сигналы, а тащить крейт `nix` ради одного
/// `kill(2)` не стоит.
unsafe fn libc_kill(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, 2); // SIGINT
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Ctl {
    stdout: String,
    stderr: String,
    ok: bool,
}

impl Ctl {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn ctl(server: &Server, args: &[&str]) -> Ctl {
    let out = Command::new(env!("CARGO_BIN_EXE_veldbctl"))
        .args(["--url", &server.http])
        .args(args)
        .output()
        .expect("не удалось запустить veldbctl");
    Ctl {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        ok: out.status.success(),
    }
}

/// Прогон через REPL: команды подаются на stdin, как из пайпа.
fn repl(server: &Server, input: &str) -> Ctl {
    let mut child = Command::new(env!("CARGO_BIN_EXE_veldbctl"))
        .args(["--url", &server.http])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Ctl {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        ok: out.status.success(),
    }
}

fn seed(server: &Server) {
    let r = ctl(
        server,
        &[
            "-e",
            "CREATE TABLE docs (id INT, title TEXT, price DOUBLE, emb VECTOR(4))",
        ],
    );
    assert!(r.ok, "{}", r.all());
    let r = ctl(
        server,
        &[
            "-e",
            "INSERT INTO docs VALUES (1,'чай',99.5,'[1,0,0,0]'),\
             (2,'кофе',250.0,'[0,1,0,0]'),(3,'какао',180.0,'[0.9,0.1,0,0]')",
        ],
    );
    assert!(r.ok, "{}", r.all());
}

// --- запрос без FROM --------------------------------------------------------

#[test]
fn select_without_from_works_through_the_cli() {
    // Регрессия: планировщик требовал ровно одну таблицу в FROM, и `SELECT 1`
    // падал. На этом же запросе построен HEALTHCHECK в Dockerfile.
    let s = Server::start(&[]);

    let r = ctl(&s, &["-e", "SELECT 1"]);
    assert!(r.ok, "{}", r.all());
    assert!(r.stdout.contains("строк: 1"), "{}", r.stdout);

    let r = ctl(&s, &["-e", "SELECT 2 + 2 AS four"]);
    assert!(
        r.ok && r.stdout.contains("four") && r.stdout.contains('4'),
        "{}",
        r.all()
    );

    // Константное условие тоже должно работать, а не давать строку «на всякий случай».
    let r = ctl(&s, &["-e", "SELECT 1 WHERE 1 = 0"]);
    assert!(r.ok && r.stdout.contains("строк: 0"), "{}", r.all());

    let r = ctl(&s, &["-e", "SELECT upper('привет') AS u"]);
    assert!(r.stdout.contains("ПРИВЕТ"), "{}", r.all());

    // Ссылка на колонку без FROM — понятная ошибка, а не паника.
    let r = ctl(&s, &["-e", "SELECT nope"]);
    assert!(
        !r.ok && r.all().contains("нет колонки 'nope'"),
        "{}",
        r.all()
    );
}

#[tokio::test]
async fn select_without_from_works_over_http_and_grpc() {
    let s = Server::start(&[]);

    let body = client::post_json(&s.http, "/query", &json!({"sql": "SELECT 1 AS one"}))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["columns"], json!(["one"]));
    assert_eq!(v["rows"], json!([[1]]));

    use veldb::grpc::pb::{veldb_client::VeldbClient, QueryRequest};
    let mut g = VeldbClient::connect(format!("http://{}", s.grpc))
        .await
        .unwrap();
    let r = g
        .query(QueryRequest {
            sql: "SELECT 1 AS one".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn dockerfile_healthcheck_command_actually_passes() {
    // HEALTHCHECK в docker/Dockerfile.dist — это буквально `veldbctl -e "SELECT 1"`.
    // Тест держит их связанными: сломается запрос — упадёт тест, а не прод.
    let s = Server::start(&[]);
    let r = ctl(&s, &["-e", "SELECT 1"]);
    assert!(r.ok, "healthcheck из Dockerfile не проходит:\n{}", r.all());
}

// --- CLI --------------------------------------------------------------------

#[test]
fn cli_round_trip_prints_a_table() {
    let s = Server::start(&[]);
    seed(&s);

    let r = ctl(&s, &["-e", "SELECT id, title FROM docs ORDER BY id"]);
    assert!(r.ok, "{}", r.all());
    for expect in ["id", "title", "чай", "кофе", "какао", "строк: 3"] {
        assert!(r.stdout.contains(expect), "нет '{expect}' в:\n{}", r.stdout);
    }
    // Рамка должна быть ровной: кириллица не должна её ломать.
    let widths: Vec<usize> = r
        .stdout
        .lines()
        .filter(|l| l.starts_with('│') || l.starts_with('┌') || l.starts_with('└'))
        .map(|l| l.chars().count())
        .collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "рамка съехала:\n{}",
        r.stdout
    );
}

#[test]
fn cli_runs_a_script_file() {
    let s = Server::start(&[]);
    seed(&s);
    let script = s.dir.join("script.sql");
    std::fs::write(
        &script,
        "SELECT title FROM docs WHERE price > 150 ORDER BY price DESC;\nSHOW TABLES;\n",
    )
    .unwrap();

    let r = ctl(&s, &["-f", script.to_str().unwrap()]);
    assert!(r.ok, "{}", r.all());
    assert!(
        r.stdout.contains("кофе") && r.stdout.contains("какао"),
        "{}",
        r.stdout
    );
    assert!(
        !r.stdout.contains("чай"),
        "фильтр не отработал:\n{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("docs"),
        "второй запрос не выполнился:\n{}",
        r.stdout
    );
}

#[test]
fn cli_repl_handles_meta_commands_and_multiline_input() {
    let s = Server::start(&[]);
    seed(&s);

    let out = repl(
        &s,
        "\\dt\n\
         \\d docs\n\
         \\timing\n\
         SELECT id, title\n\
           FROM docs\n\
           WHERE title = 'чай';\n\
         \\q\n",
    );
    assert!(out.ok, "{}", out.all());
    let text = out.stdout;
    assert!(text.contains("docs"), "\\dt ничего не показал:\n{text}");
    assert!(text.contains("VECTOR(4)"), "\\d не показал типы:\n{text}");
    assert!(
        text.contains("замер времени: включён"),
        "\\timing не сработал:\n{text}"
    );
    assert!(
        text.contains("чай"),
        "многострочный запрос не выполнился:\n{text}"
    );
    assert!(text.contains(" мс)"), "\\timing не добавил время:\n{text}");
}

#[test]
fn cli_repl_keeps_semicolons_inside_string_literals() {
    let s = Server::start(&[]);
    let out = repl(&s, "SELECT 'a;b' AS s;\n\\q\n");
    assert!(out.ok, "{}", out.all());
    assert!(
        out.stdout.contains("a;b"),
        "литерал разрезало по ';':\n{}",
        out.stdout
    );
}

#[test]
fn cli_repl_survives_a_bad_query_and_keeps_going() {
    let s = Server::start(&[]);
    seed(&s);
    let out = repl(
        &s,
        "SELECT nope FROM docs;\nSELECT count(*) FROM docs;\n\\q\n",
    );
    assert!(out.all().contains("нет колонки 'nope'"), "{}", out.all());
    // После ошибки клиент обязан остаться живым и выполнить следующий запрос.
    assert!(
        out.stdout.contains("строк: 1"),
        "клиент умер после ошибки:\n{}",
        out.all()
    );
}

#[test]
fn cli_exits_nonzero_on_error() {
    let s = Server::start(&[]);
    let r = ctl(&s, &["-e", "SELECT * FROM nope"]);
    assert!(
        !r.ok,
        "код возврата 0 при ошибке — скрипты этого не заметят"
    );
    assert!(r.all().contains("'nope' не найдена"), "{}", r.all());
}

#[test]
fn cli_says_what_to_do_when_the_server_is_down() {
    let port = free_port(); // никто не слушает
    let out = Command::new(env!("CARGO_BIN_EXE_veldbctl"))
        .args(["--url", &format!("127.0.0.1:{port}"), "-e", "SELECT 1"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("подключение"), "непонятная ошибка: {text}");
}

// --- сервер -----------------------------------------------------------------

#[tokio::test]
async fn http_endpoints_answer_on_a_real_server() {
    let s = Server::start(&[]);
    seed(&s);

    let health: Value =
        serde_json::from_slice(&client::get(&s.http, "/health").await.unwrap()).unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["rows"], 3);
    assert_eq!(health["persistent"], true);

    let schema: Value =
        serde_json::from_slice(&client::get(&s.http, "/schema").await.unwrap()).unwrap();
    assert_eq!(schema["tables"][0]["name"], "docs");
    assert_eq!(schema["tables"][0]["columns"][3]["type"], "VECTOR(4)");

    let inserted: Value = serde_json::from_slice(
        &client::post_json(
            &s.http,
            "/insert",
            &json!({"table":"docs","rows":[[4,"мате",120.0,[0,0,1,0]]]}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(inserted["inserted"], 1);

    let knn: Value = serde_json::from_slice(
        &client::post_json(
            &s.http,
            "/query",
            &json!({"sql":"SELECT id FROM docs ORDER BY l2_distance(emb,'[0,1,0,0]') LIMIT 1"}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(knn["rows"], json!([[2]]));

    let snap: Value = serde_json::from_slice(
        &client::post_json(&s.http, "/snapshot", &json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(snap["lsn"].as_u64().is_some(), "{snap}");
    assert!(s.dir.join("snapshot.vdb").exists());
}

#[tokio::test]
async fn grpc_endpoints_answer_on_a_real_server() {
    use veldb::grpc::pb::{
        value::Kind, veldb_client::VeldbClient, HealthRequest, InsertRequest, QueryRequest, Row,
        SchemaRequest, Value as PbValue, Vector,
    };
    let s = Server::start(&[]);
    seed(&s);
    let mut g = VeldbClient::connect(format!("http://{}", s.grpc))
        .await
        .unwrap();

    let h = g.health(HealthRequest {}).await.unwrap().into_inner();
    assert_eq!((h.tables, h.rows), (1, 3));

    let sc = g.schema(SchemaRequest {}).await.unwrap().into_inner();
    assert_eq!(sc.tables[0].name, "docs");

    let pb = |k: Kind| PbValue { kind: Some(k) };
    let n = g
        .insert(InsertRequest {
            table: "docs".into(),
            rows: vec![Row {
                values: vec![
                    pb(Kind::I(4)),
                    pb(Kind::S("мате".into())),
                    pb(Kind::F(120.0)),
                    pb(Kind::V(Vector {
                        values: vec![0.0, 0.0, 1.0, 0.0],
                    })),
                ],
            }],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(n.inserted, 1);

    let r = g
        .query(QueryRequest {
            sql: "SELECT count(*) FROM docs".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.rows[0].values[0].kind, Some(Kind::I(4)));

    let e = g
        .query(QueryRequest {
            sql: "SELECT nope FROM docs".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
}

#[test]
fn server_restores_data_after_restart() {
    let s = Server::start(&[]);
    seed(&s);
    let dir = s.stop_gracefully();

    let s = Server::start_in(dir.clone(), None, &[]);
    let r = ctl(&s, &["-e", "SELECT count(*) FROM docs"]);
    assert!(
        r.ok && r.stdout.contains('3'),
        "данные не восстановились:\n{}",
        r.all()
    );
    // Дозапись после восстановления тоже должна работать.
    assert!(
        ctl(
            &s,
            &["-e", "INSERT INTO docs VALUES (9,'x',1.0,'[0,0,0,0]')"]
        )
        .ok
    );
    drop(s);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn server_survives_being_killed_without_a_snapshot() {
    let s = Server::start(&[]);
    seed(&s);
    let dir = s.kill_hard(); // снапшота не было, есть только WAL

    let s = Server::start_in(dir.clone(), None, &[]);
    let r = ctl(&s, &["-e", "SELECT count(*) FROM docs"]);
    assert!(
        r.ok && r.stdout.contains('3'),
        "WAL не проигрался:\n{}",
        r.all()
    );
    drop(s);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn max_memory_flag_is_enforced_by_the_server() {
    let s = Server::start(&["--max-memory", "4KiB"]);
    assert!(ctl(&s, &["-e", "CREATE TABLE t (x INT)"]).ok);
    // 4 КиБ — это 512 значений i64; заведомо переливаем через край.
    let rows: Vec<String> = (0..2000).map(|i| format!("({i})")).collect();
    let _ = ctl(
        &s,
        &["-e", &format!("INSERT INTO t VALUES {}", rows.join(","))],
    );

    let r = ctl(&s, &["-e", "INSERT INTO t VALUES (1)"]);
    assert!(!r.ok, "предел памяти не сработал");
    assert!(r.all().contains("предел памяти"), "{}", r.all());
    // Чтение при этом обязано работать: это отказ в записи, а не смерть базы.
    assert!(ctl(&s, &["-e", "SELECT count(*) FROM t"]).ok);
}

#[test]
fn server_rejects_a_bad_durability_value_instead_of_guessing() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_veldb"))
        .args(["--data", tmp.path().to_str().unwrap()])
        .args(["--http", &format!("127.0.0.1:{}", free_port())])
        .args(["--grpc", &format!("127.0.0.1:{}", free_port())])
        .args(["--durability", "иногда"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fsync|buffered"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn replica_process_follows_the_primary_process() {
    let primary = Server::start(&[]);
    seed(&primary);

    let replica = Server::start(&["--replicate-from", &primary.http, "--replica-poll-ms", "50"]);

    let converged = |n: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let r = ctl(&replica, &["-e", "SELECT count(*) FROM docs"]);
            if r.ok && r.stdout.contains(n) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    };
    assert!(
        converged("3"),
        "реплика не догнала:\n{}",
        replica.log_text()
    );

    assert!(
        ctl(
            &primary,
            &["-e", "INSERT INTO docs VALUES (4,'мате',120.0,'[0,0,1,0]')"]
        )
        .ok
    );
    assert!(
        converged("4"),
        "реплика не приняла новую запись:\n{}",
        replica.log_text()
    );

    // Реплика обязана отказывать в записи, иначе состояния разъедутся навсегда.
    let r = ctl(
        &replica,
        &["-e", "INSERT INTO docs VALUES (9,'x',1.0,'[0,0,0,0]')"],
    );
    assert!(!r.ok && r.all().contains("реплика"), "{}", r.all());
    assert!(
        ctl(&replica, &["-e", "SELECT id FROM docs"]).ok,
        "чтение с реплики сломалось"
    );
}
