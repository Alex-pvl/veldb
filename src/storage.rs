//! Персистентность: снапшот колонок + WAL для дельты.
//!
//! База живёт в оперативной памяти, поэтому «запись на диск» здесь нужна ровно для
//! одного — пережить рестарт. Отсюда порядок: изменение сначала применяется в памяти,
//! потом уходит в WAL, и только после этого подтверждается клиенту. Падение до записи
//! в WAL теряет неподтверждённое изменение, что для in-memory базы — корректное
//! поведение, а не потеря данных.

use crate::codec::{crc32, ensure_le, Reader, Writer};
use crate::column::Value;
use crate::table::{Schema, Table};
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SNAPSHOT_MAGIC: &[u8; 8] = b"VELDBSNP";
const WAL_MAGIC: &[u8; 8] = b"VELDBWAL";
const FORMAT_VERSION: u32 = 1;

/// Насколько сильно платим за долговечность.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// `fsync` после каждой записи в WAL. Переживает выключение питания.
    /// Стоит один системный вызов на *запрос*, а не на строку, поэтому пакетная
    /// вставка от этого почти не страдает.
    #[default]
    Fsync,
    /// Только запись в файл. Переживает падение процесса, но не машины.
    Buffered,
}

/// Прочитанная запись WAL: её LSN и полезная нагрузка.
pub type RawRecord = (u64, Vec<u8>);

#[derive(Debug, Clone, PartialEq)]
pub enum WalRecord {
    CreateTable {
        name: String,
        schema: Schema,
    },
    DropTable {
        name: String,
    },
    Insert {
        table: String,
        rows: Vec<Vec<Value>>,
    },
}

impl WalRecord {
    pub fn encode(&self, schema_of: &dyn Fn(&str) -> Option<Schema>) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        match self {
            WalRecord::CreateTable { name, schema } => {
                w.u8(1);
                w.str(name);
                w.schema(schema);
            }
            WalRecord::DropTable { name } => {
                w.u8(2);
                w.str(name);
            }
            WalRecord::Insert { table, rows } => {
                let schema =
                    schema_of(table).with_context(|| format!("схема таблицы '{table}' для WAL"))?;
                w.u8(3);
                w.str(table);
                w.u64(rows.len() as u64);
                for r in rows {
                    if r.len() != schema.len() {
                        bail!(
                            "строка из {} значений при {} колонках",
                            r.len(),
                            schema.len()
                        );
                    }
                    for v in r {
                        w.value(v);
                    }
                }
            }
        }
        Ok(w.buf)
    }

    pub fn decode(payload: &[u8], schema_of: &dyn Fn(&str) -> Option<Schema>) -> Result<WalRecord> {
        let mut r = Reader::new(payload);
        Ok(match r.u8()? {
            1 => WalRecord::CreateTable {
                name: r.str()?,
                schema: r.schema()?,
            },
            2 => WalRecord::DropTable { name: r.str()? },
            3 => {
                let table = r.str()?;
                let schema = schema_of(&table)
                    .with_context(|| format!("WAL ссылается на неизвестную таблицу '{table}'"))?;
                let n = r.u64()? as usize;
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    rows.push(
                        schema
                            .fields
                            .iter()
                            .map(|f| r.value(f.ty))
                            .collect::<Result<Vec<_>>>()?,
                    );
                }
                WalRecord::Insert { table, rows }
            }
            other => bail!("неизвестный код записи WAL: {other}"),
        })
    }
}

/// Кадр записи: `len | crc | lsn | payload`. CRC считается по `lsn + payload`,
/// поэтому перепутанный или обрезанный хвост определяется, а не читается как данные.
const FRAME_HEADER: usize = 4 + 4 + 8;

pub struct Wal {
    file: File,
    path: PathBuf,
    next_lsn: u64,
    pub durability: Durability,
}

impl Wal {
    /// Открывает WAL и возвращает уцелевшие записи. Повреждённый хвост
    /// (обрыв на середине кадра после `kill -9`) отбрасывается и файл обрезается:
    /// половина записи — это не данные, и притворяться, что это данные, опаснее.
    pub fn open(path: &Path, durability: Durability) -> Result<(Wal, Vec<RawRecord>)> {
        ensure_le()?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("открытие WAL {}", path.display()))?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)?;

        let mut good_len = WAL_MAGIC.len() + 4;
        let mut records = Vec::new();
        if raw.is_empty() {
            file.write_all(WAL_MAGIC)?;
            file.write_all(&FORMAT_VERSION.to_le_bytes())?;
            file.flush()?;
        } else {
            if raw.len() < good_len || &raw[..8] != WAL_MAGIC {
                bail!("{} не похож на WAL veldb", path.display());
            }
            let ver = u32::from_le_bytes(raw[8..12].try_into().unwrap());
            if ver != FORMAT_VERSION {
                bail!("версия WAL {ver}, поддерживается {FORMAT_VERSION}");
            }
            let mut pos = good_len;
            while pos + FRAME_HEADER <= raw.len() {
                let len = u32::from_le_bytes(raw[pos..pos + 4].try_into().unwrap()) as usize;
                let crc = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().unwrap());
                let end = pos + FRAME_HEADER + len;
                if end > raw.len() {
                    break; // кадр не дописан
                }
                let body = &raw[pos + 8..end];
                if crc32(body) != crc {
                    break; // кадр побит
                }
                let lsn = u64::from_le_bytes(body[..8].try_into().unwrap());
                records.push((lsn, body[8..].to_vec()));
                pos = end;
                good_len = pos;
            }
            if good_len != raw.len() {
                file.set_len(good_len as u64)?;
            }
        }
        file.seek(SeekFrom::End(0))?;
        let next_lsn = records.last().map(|(l, _)| l + 1).unwrap_or(1);
        Ok((
            Wal {
                file,
                path: path.to_path_buf(),
                next_lsn,
                durability,
            },
            records,
        ))
    }

    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    pub fn append(&mut self, payload: &[u8]) -> Result<u64> {
        let lsn = self.next_lsn;
        let mut body = Vec::with_capacity(8 + payload.len());
        body.extend_from_slice(&lsn.to_le_bytes());
        body.extend_from_slice(payload);
        let mut frame = Vec::with_capacity(FRAME_HEADER + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32(&body).to_le_bytes());
        frame.extend_from_slice(&body);
        // Один write_all на кадр: частичная запись возможна только при обрыве
        // питания, и её ловит CRC при следующем открытии.
        self.file.write_all(&frame)?;
        if self.durability == Durability::Fsync {
            self.file.sync_data()?;
        }
        self.next_lsn += 1;
        Ok(lsn)
    }

    /// Записи с LSN строго больше `after`. Используется репликацией.
    pub fn records_after(&self, after: u64) -> Result<Vec<RawRecord>> {
        let (_, all) = Wal::open(&self.path, self.durability)?;
        Ok(all.into_iter().filter(|(lsn, _)| *lsn > after).collect())
    }

    /// Сбрасывает WAL после успешного снапшота. Нумерация LSN продолжается —
    /// иначе реплика не отличила бы новые записи от уже применённых.
    pub fn reset(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(WAL_MAGIC)?;
        self.file.write_all(&FORMAT_VERSION.to_le_bytes())?;
        self.file.sync_data()?;
        Ok(())
    }
}

/// Снапшот: имя, схема и сырые колонки каждой таблицы.
/// Тот же массив байт уходит и в файл, и реплике при первичной синхронизации —
/// два формата для одного и того же неизбежно разъезжаются.
pub fn encode_snapshot(tables: &[&Table], lsn: u64) -> Result<Vec<u8>> {
    ensure_le()?;
    let mut w = Writer::new();
    w.buf.extend_from_slice(SNAPSHOT_MAGIC);
    w.u32(FORMAT_VERSION);
    w.u64(lsn);
    w.u64(tables.len() as u64);
    for t in tables {
        w.str(&t.name);
        w.schema(&t.schema);
        w.u64(t.nrows() as u64);
        for c in t.columns() {
            w.column(c);
        }
    }
    let crc = crc32(&w.buf);
    w.u32(crc);
    Ok(w.buf)
}

pub fn write_snapshot(path: &Path, tables: &[&Table], lsn: u64) -> Result<()> {
    let bytes = encode_snapshot(tables, lsn)?;

    // Пишем во временный файл и переименовываем: rename атомарен, поэтому
    // на диске никогда не бывает наполовину записанного снапшота.
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp).with_context(|| format!("создание {}", tmp.display()))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // Переименование тоже надо зафиксировать, иначе после сбоя питания
        // каталог может ещё указывать на старую версию.
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

pub fn read_snapshot(path: &Path) -> Result<(Vec<Table>, u64)> {
    let raw = std::fs::read(path).with_context(|| format!("чтение {}", path.display()))?;
    decode_snapshot(&raw).with_context(|| format!("снапшот {}", path.display()))
}

pub fn decode_snapshot(raw: &[u8]) -> Result<(Vec<Table>, u64)> {
    ensure_le()?;
    if raw.len() < 28 || &raw[..8] != SNAPSHOT_MAGIC {
        bail!("данные не похожи на снапшот veldb");
    }
    let body = &raw[..raw.len() - 4];
    let stored = u32::from_le_bytes(raw[raw.len() - 4..].try_into().unwrap());
    if crc32(body) != stored {
        bail!("снапшот повреждён (не сходится CRC)");
    }

    let mut r = Reader::new(body);
    let _ = r.take_magic()?;
    let ver = r.u32()?;
    if ver != FORMAT_VERSION {
        bail!("версия снапшота {ver}, поддерживается {FORMAT_VERSION}");
    }
    let lsn = r.u64()?;
    let ntables = r.u64()? as usize;
    let mut tables = Vec::with_capacity(ntables);
    for _ in 0..ntables {
        let name = r.str()?;
        let schema = r.schema()?;
        let nrows = r.u64()? as usize;
        let mut columns = Vec::with_capacity(schema.len());
        for f in &schema.fields {
            columns.push(r.column(f.ty)?);
        }
        let t = Table::from_columns(&name, schema, columns)
            .with_context(|| format!("таблица '{name}' из снапшота"))?;
        if t.nrows() != nrows {
            bail!(
                "таблица '{name}': в снапшоте {nrows} строк, прочитано {}",
                t.nrows()
            );
        }
        tables.push(t);
    }
    Ok((tables, lsn))
}
