//! База: каталог таблиц + точка входа `execute`.

use crate::column::{DataType, Value};
use crate::exec::{run_select, QueryResult};
use crate::plan::Statement;
use crate::sql::{self, Catalog};
use crate::storage::{self, Durability, Wal, WalRecord};
use crate::table::{Schema, Table};
use anyhow::{anyhow, bail, Context, Result};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Блокировка на таблицу, а не на всю базу: читающие запросы к разным таблицам
/// не мешают друг другу, а внешний замок держится только на DDL.
pub struct Database {
    tables: RwLock<FxHashMap<String, Arc<RwLock<Table>>>>,
    store: Option<Store>,
    /// Реплика не должна принимать записи от клиентов: расхождение с первичным
    /// узлом молча и навсегда — худший из возможных исходов репликации.
    read_only: AtomicBool,
    /// Потолок памяти под данные, 0 — без ограничения.
    ///
    /// База целиком в памяти, а на Raspberry Pi её мало. Без потолка процесс
    /// убивает OOM-killer — вместе с ещё не сброшенным на диск WAL-буфером.
    /// Внятная ошибка на вставке лучше внезапной смерти процесса.
    memory_limit: AtomicU64,
}

struct Store {
    dir: PathBuf,
    wal: Mutex<Wal>,
}

impl Store {
    fn snapshot_path(&self) -> PathBuf {
        self.dir.join("snapshot.vdb")
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    /// База без диска. Для тестов и для режима «кеш, который не жалко».
    pub fn new() -> Database {
        Database {
            tables: RwLock::new(FxHashMap::default()),
            store: None,
            read_only: AtomicBool::new(false),
            memory_limit: AtomicU64::new(0),
        }
    }

    /// Открывает каталог данных: снапшот + догон по WAL.
    pub fn open(dir: impl AsRef<Path>, durability: Durability) -> Result<Database> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("создание каталога данных {}", dir.display()))?;

        let db = Database::new();
        let snapshot_path = dir.join("snapshot.vdb");
        let snapshot_lsn = if snapshot_path.exists() {
            let (tables, lsn) = storage::read_snapshot(&snapshot_path)?;
            for t in tables {
                db.put_table(t);
            }
            lsn
        } else {
            0
        };

        let (wal, records) = Wal::open(&dir.join("wal.log"), durability)?;
        let mut replayed = 0usize;
        for (lsn, payload) in records {
            // Записи, уже вошедшие в снапшот, пропускаем: повторная вставка
            // продублировала бы строки.
            if lsn <= snapshot_lsn {
                continue;
            }
            let rec = WalRecord::decode(&payload, &|t| db.schema_of(t))
                .with_context(|| format!("запись WAL lsn={lsn}"))?;
            db.apply(&rec)
                .with_context(|| format!("применение записи WAL lsn={lsn}"))?;
            replayed += 1;
        }
        if replayed > 0 {
            eprintln!("восстановлено записей из WAL: {replayed}");
        }

        Ok(Database {
            tables: db.tables,
            store: Some(Store {
                dir,
                wal: Mutex::new(wal),
            }),
            read_only: AtomicBool::new(false),
            memory_limit: AtomicU64::new(0),
        })
    }

    /// LSN, который получит следующая запись. Точка синхронизации для реплик.
    pub fn next_lsn(&self) -> u64 {
        match &self.store {
            Some(s) => s.wal.lock().unwrap().next_lsn(),
            None => 0,
        }
    }

    pub fn is_persistent(&self) -> bool {
        self.store.is_some()
    }

    /// Ограничение на суммарный размер данных в памяти. 0 — снять ограничение.
    pub fn set_memory_limit(&self, bytes: u64) {
        self.memory_limit.store(bytes, Ordering::Relaxed);
    }

    pub fn bytes_used(&self) -> usize {
        self.tables
            .read()
            .unwrap()
            .values()
            .map(|t| t.read().unwrap().bytes_used())
            .sum()
    }

    pub fn set_read_only(&self, v: bool) {
        self.read_only.store(v, Ordering::Relaxed);
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Relaxed)
    }

    fn deny_if_read_only(&self, what: &str) -> Result<()> {
        if self.is_read_only() {
            bail!("узел работает как реплика: {what} принимает только первичный узел");
        }
        Ok(())
    }

    /// Сбрасывает состояние на диск и очищает WAL.
    pub fn snapshot(&self) -> Result<u64> {
        let Some(store) = &self.store else {
            bail!("база открыта без каталога данных, снапшот некуда писать");
        };
        // WAL держим залоченным на всё время: иначе снапшот и новая запись
        // могли бы разъехаться по LSN, и после рестарта строка потерялась бы.
        let mut wal = store.wal.lock().unwrap();
        let lsn = wal.next_lsn() - 1;
        let handles: Vec<_> = self.tables.read().unwrap().values().cloned().collect();
        let guards: Vec<_> = handles.iter().map(|t| t.read().unwrap()).collect();
        let tables: Vec<&Table> = guards.iter().map(|g| &**g).collect();
        storage::write_snapshot(&store.snapshot_path(), &tables, lsn)?;
        wal.reset()?;
        Ok(lsn)
    }

    /// Полный снапшот в память — им реплика догоняет базу с нуля.
    /// LSN замораживается вместе с WAL, иначе между снимком и первым дозапросом
    /// записей образовалась бы дыра.
    pub fn snapshot_bytes(&self) -> Result<Vec<u8>> {
        let _wal_guard = self.store.as_ref().map(|s| s.wal.lock().unwrap());
        let lsn = _wal_guard.as_ref().map(|w| w.next_lsn() - 1).unwrap_or(0);
        let handles: Vec<_> = self.tables.read().unwrap().values().cloned().collect();
        let guards: Vec<_> = handles.iter().map(|t| t.read().unwrap()).collect();
        let tables: Vec<&Table> = guards.iter().map(|g| &**g).collect();
        storage::encode_snapshot(&tables, lsn)
    }

    /// Заменяет всё состояние снапшотом. Путь первичной синхронизации реплики.
    pub fn load_snapshot_bytes(&self, bytes: &[u8]) -> Result<u64> {
        let (tables, lsn) = storage::decode_snapshot(bytes)?;
        let mut map = self.tables.write().unwrap();
        map.clear();
        for t in tables {
            map.insert(Self::key(&t.name), Arc::new(RwLock::new(t)));
        }
        Ok(lsn)
    }

    /// Записи WAL после `after` — их запрашивает реплика.
    pub fn wal_since(&self, after: u64) -> Result<Vec<storage::RawRecord>> {
        match &self.store {
            Some(s) => s.wal.lock().unwrap().records_after(after),
            None => Ok(Vec::new()),
        }
    }

    /// Применяет запись, не записывая её в собственный WAL. Путь восстановления
    /// и путь реплики — один и тот же код, поэтому расхождение состояний
    /// невозможно по построению.
    pub fn apply(&self, rec: &WalRecord) -> Result<()> {
        match rec {
            WalRecord::CreateTable { name, schema } => {
                let table = Table::new(name, schema.clone())?;
                self.put_table(table);
                Ok(())
            }
            WalRecord::DropTable { name } => {
                self.drop_table_inner(name);
                Ok(())
            }
            WalRecord::Insert { table, rows } => {
                self.insert_inner(table, rows)?;
                Ok(())
            }
        }
    }

    fn log(&self, rec: &WalRecord) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let payload = rec.encode(&|t| self.schema_of(t))?;
        store.wal.lock().unwrap().append(&payload)?;
        Ok(())
    }

    fn key(name: &str) -> String {
        name.to_lowercase()
    }

    pub fn table(&self, name: &str) -> Option<Arc<RwLock<Table>>> {
        self.tables.read().unwrap().get(&Self::key(name)).cloned()
    }

    pub fn table_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .tables
            .read()
            .unwrap()
            .values()
            .map(|t| t.read().unwrap().name.clone())
            .collect();
        v.sort();
        v
    }

    pub fn create_table(&self, name: &str, schema: Schema) -> Result<()> {
        self.deny_if_read_only("CREATE TABLE")?;
        let table = Table::new(name, schema.clone())?;
        {
            let mut tables = self.tables.write().unwrap();
            if tables.contains_key(&Self::key(name)) {
                bail!("таблица '{name}' уже существует");
            }
            tables.insert(Self::key(name), Arc::new(RwLock::new(table)));
        }
        self.log(&WalRecord::CreateTable {
            name: name.to_string(),
            schema,
        })
    }

    pub fn drop_table(&self, name: &str) -> Result<bool> {
        self.deny_if_read_only("DROP TABLE")?;
        if !self.drop_table_inner(name) {
            return Ok(false);
        }
        self.log(&WalRecord::DropTable {
            name: name.to_string(),
        })?;
        Ok(true)
    }

    fn drop_table_inner(&self, name: &str) -> bool {
        self.tables
            .write()
            .unwrap()
            .remove(&Self::key(name))
            .is_some()
    }

    pub(crate) fn put_table(&self, table: Table) {
        self.tables
            .write()
            .unwrap()
            .insert(Self::key(&table.name), Arc::new(RwLock::new(table)));
    }

    /// Сначала в память, потом в WAL, и только потом ответ клиенту: подтверждаем
    /// лишь то, что переживёт рестарт.
    pub fn insert_rows(&self, name: &str, rows: &[Vec<Value>]) -> Result<usize> {
        self.deny_if_read_only("INSERT")?;
        // Потолок проверяется только здесь, на клиентском пути. Восстановление из
        // WAL и репликация идут мимо: там данные уже существуют выше по течению,
        // и отказ применить их означал бы расхождение состояний, а не экономию памяти.
        let limit = self.memory_limit.load(Ordering::Relaxed);
        if limit > 0 {
            // Проверяем по факту занятого до вставки: точный размер пачки заранее
            // неизвестен, поэтому это «стоп при подходе», а не гарантия до байта.
            let used = self.bytes_used() as u64;
            if used >= limit {
                bail!(
                    "достигнут предел памяти: занято {used} байт из {limit}. \
                     Сделайте снапшот и перезапустите с большим --max-memory либо удалите данные"
                );
            }
        }
        let n = self.insert_inner(name, rows)?;
        self.log(&WalRecord::Insert {
            table: name.to_string(),
            rows: rows.to_vec(),
        })?;
        Ok(n)
    }

    fn insert_inner(&self, name: &str, rows: &[Vec<Value>]) -> Result<usize> {
        let t = self
            .table(name)
            .ok_or_else(|| anyhow!("таблица '{name}' не найдена"))?;
        let mut t = t.write().unwrap();
        t.insert_many(rows)
    }

    pub fn execute(&self, query: &str) -> Result<QueryResult> {
        self.run(sql::plan(query, self)?)
    }

    pub fn run(&self, stmt: Statement) -> Result<QueryResult> {
        match stmt {
            Statement::CreateTable {
                name,
                schema,
                if_not_exists,
            } => {
                if if_not_exists && self.table(&name).is_some() {
                    return Ok(QueryResult::message(&format!("таблица '{name}' уже есть")));
                }
                self.create_table(&name, schema)?;
                Ok(QueryResult::message(&format!("таблица '{name}' создана")))
            }
            Statement::DropTable { name, if_exists } => {
                if !self.drop_table(&name)? && !if_exists {
                    bail!("таблица '{name}' не найдена");
                }
                Ok(QueryResult::message(&format!("таблица '{name}' удалена")))
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => {
                let rows = match &columns {
                    None => rows,
                    Some(names) => reorder(&table, names, rows, self)?,
                };
                let n = self.insert_rows(&table, &rows)?;
                Ok(QueryResult::message(&format!("вставлено строк: {n}")))
            }
            Statement::Select(plan) => {
                let t = self
                    .table(&plan.table)
                    .ok_or_else(|| anyhow!("таблица '{}' не найдена", plan.table))?;
                let t = t.read().unwrap();
                run_select(&plan, &t)
            }
            Statement::ShowTables => Ok(QueryResult {
                columns: vec!["name".into(), "rows".into()],
                types: vec![DataType::Str, DataType::I64],
                rows: self
                    .table_names()
                    .into_iter()
                    .map(|n| {
                        let rows = self
                            .table(&n)
                            .map(|t| t.read().unwrap().nrows())
                            .unwrap_or(0);
                        vec![Value::Str(n), Value::I64(rows as i64)]
                    })
                    .collect(),
            }),
            Statement::Describe(name) => {
                let t = self
                    .table(&name)
                    .ok_or_else(|| anyhow!("таблица '{name}' не найдена"))?;
                let t = t.read().unwrap();
                Ok(QueryResult {
                    columns: vec!["column".into(), "type".into()],
                    types: vec![DataType::Str, DataType::Str],
                    rows: t
                        .schema
                        .fields
                        .iter()
                        .map(|f| vec![Value::Str(f.name.clone()), Value::Str(f.ty.name())])
                        .collect(),
                })
            }
        }
    }
}

/// `INSERT INTO t (b, a) VALUES ...` — переставляем значения в порядок схемы.
/// Пропущенные колонки запрещены: NULL в veldb нет, а придумывать значение за
/// пользователя хуже, чем сказать об этом.
fn reorder(
    table: &str,
    names: &[String],
    rows: Vec<Vec<Value>>,
    db: &Database,
) -> Result<Vec<Vec<Value>>> {
    let schema = db
        .schema_of(table)
        .ok_or_else(|| anyhow!("таблица '{table}' не найдена"))?;
    if names.len() != schema.len() {
        bail!(
            "перечислено {} колонок из {}: INSERT без части колонок не поддерживается (NULL нет)",
            names.len(),
            schema.len()
        );
    }
    let mut pos = vec![usize::MAX; schema.len()];
    for (i, n) in names.iter().enumerate() {
        let target = schema
            .index_of(n)
            .ok_or_else(|| anyhow!("нет колонки '{n}'"))?;
        if pos[target] != usize::MAX {
            bail!("колонка '{n}' указана дважды");
        }
        pos[target] = i;
    }
    Ok(rows
        .into_iter()
        .map(|r| pos.iter().map(|&i| r[i].clone()).collect())
        .collect())
}

impl Catalog for Database {
    fn schema_of(&self, table: &str) -> Option<Schema> {
        self.table(table).map(|t| t.read().unwrap().schema.clone())
    }
}
