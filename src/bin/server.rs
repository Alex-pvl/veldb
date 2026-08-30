//! Сервер veldb: HTTP и gRPC поверх одной базы.

use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use veldb::storage::Durability;
use veldb::{grpc, http, Database};

#[derive(Parser, Debug)]
#[command(
    name = "veldb",
    version,
    about = "In-memory колоночная СУБД с векторным поиском"
)]
struct Args {
    /// Каталог данных. Без него база живёт только в памяти.
    #[arg(long)]
    data: Option<String>,

    #[arg(long, default_value = "127.0.0.1:8080")]
    http: SocketAddr,

    #[arg(long, default_value = "127.0.0.1:8081")]
    grpc: SocketAddr,

    /// `fsync` после каждой записи (надёжно) или только запись в файл (быстро).
    #[arg(long, default_value = "fsync")]
    durability: String,

    /// Период автоснапшота в секундах, 0 — выключить.
    #[arg(long, default_value_t = 300)]
    snapshot_interval: u64,

    /// Потолок памяти под данные, например `3GiB`. Без него на машине с малым
    /// объёмом ОЗУ процесс убьёт OOM-killer.
    #[arg(long)]
    max_memory: Option<String>,

    /// Адрес первичного узла: включает режим реплики (только чтение).
    #[arg(long)]
    replicate_from: Option<String>,

    /// Как часто реплика опрашивает первичный узел, миллисекунды.
    #[arg(long, default_value_t = 200)]
    replica_poll_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let durability = match args.durability.as_str() {
        "fsync" => Durability::Fsync,
        "buffered" => Durability::Buffered,
        other => anyhow::bail!("--durability: ожидалось fsync|buffered, получено '{other}'"),
    };

    let db = Arc::new(match &args.data {
        Some(dir) => Database::open(dir, durability).context("открытие каталога данных")?,
        None => Database::new(),
    });

    if let Some(limit) = &args.max_memory {
        let bytes = parse_size(limit)?;
        db.set_memory_limit(bytes);
        eprintln!("предел памяти: {bytes} байт");
    }

    let rows: usize = db
        .table_names()
        .iter()
        .filter_map(|n| db.table(n))
        .map(|t| t.read().unwrap().nrows())
        .sum();
    eprintln!(
        "veldb {}: таблиц {}, строк {rows}, хранилище {}",
        env!("CARGO_PKG_VERSION"),
        db.table_names().len(),
        args.data.as_deref().unwrap_or("только память")
    );

    let mut tasks = Vec::new();

    if let Some(primary) = args.replicate_from.clone() {
        eprintln!("режим реплики: первичный узел {primary}");
        let db = db.clone();
        let poll = Duration::from_millis(args.replica_poll_ms);
        tasks.push(tokio::spawn(async move {
            veldb::replication::follow(db, &primary, poll).await
        }));
    }

    if args.snapshot_interval > 0 && db.is_persistent() && args.replicate_from.is_none() {
        let db = db.clone();
        let period = Duration::from_secs(args.snapshot_interval);
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.tick().await; // первый тик срабатывает сразу — пропускаем
            loop {
                tick.tick().await;
                let db = db.clone();
                // Снапшот блокирующий; ошибка не должна ронять сервер,
                // потому что WAL по-прежнему цел.
                match tokio::task::spawn_blocking(move || db.snapshot()).await {
                    Ok(Ok(lsn)) => eprintln!("снапшот записан, lsn={lsn}"),
                    Ok(Err(e)) => eprintln!("снапшот не удался: {e:#}"),
                    Err(e) => eprintln!("снапшот прерван: {e}"),
                }
            }
        }));
    }

    let http_listener = tokio::net::TcpListener::bind(args.http)
        .await
        .with_context(|| format!("не удалось занять {}", args.http))?;
    eprintln!("HTTP  http://{}", args.http);
    {
        let app = http::router(db.clone());
        tasks.push(tokio::spawn(async move {
            axum::serve(http_listener, app).await.map_err(Into::into)
        }));
    }

    eprintln!("gRPC  http://{}", args.grpc);
    {
        let svc = grpc::Service::new(db.clone());
        let addr = args.grpc;
        tasks.push(tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(svc)
                .serve(addr)
                .await
                .map_err(Into::into)
        }));
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => eprintln!("\nостановка..."),
        r = futures_first(tasks) => {
            if let Err(e) = r {
                eprintln!("сервис упал: {e:#}");
            }
        }
    }

    // Финальный снапшот: без него после штатной остановки пришлось бы
    // проигрывать весь WAL заново.
    if db.is_persistent() && args.replicate_from.is_none() {
        match db.snapshot() {
            Ok(lsn) => eprintln!("финальный снапшот, lsn={lsn}"),
            Err(e) => eprintln!("финальный снапшот не удался: {e:#}"),
        }
    }
    Ok(())
}

/// Разбор размеров вида `512MiB`, `3GB`, `1073741824`.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.to_ascii_uppercase() {
        v if v.ends_with("GIB") || v.ends_with("G") => {
            (v.trim_end_matches(['G', 'I', 'B']).to_string(), 1u64 << 30)
        }
        v if v.ends_with("MIB") || v.ends_with("M") => {
            (v.trim_end_matches(['M', 'I', 'B']).to_string(), 1u64 << 20)
        }
        v if v.ends_with("KIB") || v.ends_with("K") => {
            (v.trim_end_matches(['K', 'I', 'B']).to_string(), 1u64 << 10)
        }
        v => (v, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("размер '{s}'"))?;
    if n < 0.0 {
        anyhow::bail!("размер '{s}' отрицательный");
    }
    Ok((n * mult as f64) as u64)
}

/// Ждёт первую завершившуюся задачу. `select!` по вектору задач стандартными
/// средствами не выражается, а тянуть `futures` ради одной комбинации — перебор.
async fn futures_first(tasks: Vec<tokio::task::JoinHandle<Result<()>>>) -> Result<()> {
    let mut tasks = tasks;
    loop {
        for t in tasks.iter_mut() {
            if t.is_finished() {
                return t.await?;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
