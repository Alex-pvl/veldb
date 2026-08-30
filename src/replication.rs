//! Асинхронная репликация: реплика тянет WAL с первичного узла.
//!
//! Pull, а не push: первичный узел не хранит список реплик, не ждёт их и не
//! замедляется от их числа. Реплика сама решает, как часто спрашивать, и сама
//! переживает свой рестарт. Цена — отставание на один период опроса.
//!
//! Применение записей идёт тем же `Database::apply`, что и восстановление из WAL,
//! поэтому «реплика применила не так, как первичный» невозможно по построению.

use crate::client::{get, normalize};
use crate::db::Database;
use anyhow::{anyhow, bail, Context, Result};
use std::sync::Arc;
use std::time::Duration;

/// Полная синхронизация: забирает снапшот первичного узла и заменяет им состояние.
/// Возвращает LSN, на котором снапшот сделан.
pub async fn bootstrap(db: &Database, host: &str) -> Result<u64> {
    let bytes = get(host, "/replication/snapshot")
        .await
        .context("снапшот с первичного узла")?;
    let lsn = db.load_snapshot_bytes(&bytes)?;
    Ok(lsn)
}

/// Один шаг догона. Возвращает новый LSN реплики и число применённых записей.
pub async fn pull_once(db: &Database, host: &str, applied: u64) -> Result<(u64, usize)> {
    let body = get(host, &format!("/replication/wal?after={applied}")).await?;
    let v: serde_json::Value = serde_json::from_slice(&body).context("ответ /replication/wal")?;
    let records = v["records"]
        .as_array()
        .ok_or_else(|| anyhow!("в ответе нет 'records'"))?;

    let mut lsn = applied;
    let mut n = 0usize;
    for rec in records {
        let rec_lsn = rec["lsn"]
            .as_u64()
            .ok_or_else(|| anyhow!("запись без lsn"))?;
        // LSN идут подряд. Разрыв означает, что первичный узел успел сделать
        // снапшот и обрезать WAL — догонять нечего, нужна полная синхронизация.
        if rec_lsn != lsn + 1 {
            bail!("разрыв в WAL: ожидался lsn={}, пришёл {rec_lsn}", lsn + 1);
        }
        let payload = crate::http::unhex(
            rec["payload"]
                .as_str()
                .ok_or_else(|| anyhow!("запись без payload"))?,
        )?;
        let record = crate::http::decode_record(&payload, db)
            .with_context(|| format!("разбор записи lsn={rec_lsn}"))?;
        db.apply(&record)
            .with_context(|| format!("применение записи lsn={rec_lsn}"))?;
        lsn = rec_lsn;
        n += 1;
    }
    Ok((lsn, n))
}

/// Бесконечный цикл реплики. Ошибки сети не считаются фатальными: первичный
/// узел может перезапускаться, реплика должна это пережить и продолжить.
pub async fn follow(db: Arc<Database>, primary: &str, poll: Duration) -> Result<()> {
    let host = normalize(primary);
    db.set_read_only(true);

    let mut applied: Option<u64> = None;
    loop {
        let result: Result<()> = async {
            let from = match applied {
                Some(l) => l,
                None => {
                    let l = bootstrap(&db, &host).await?;
                    applied = Some(l);
                    l
                }
            };
            let (lsn, n) = pull_once(&db, &host, from).await?;
            applied = Some(lsn);
            if n > 0 {
                eprintln!("реплика: применено записей {n}, lsn={lsn}");
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            eprintln!("реплика: {e:#}");
            // Разрыв в WAL лечится только полной пересинхронизацией.
            if format!("{e:#}").contains("разрыв в WAL") {
                applied = None;
            }
        }
        tokio::time::sleep(poll).await;
    }
}
