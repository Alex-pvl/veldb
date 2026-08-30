# veldb

In-memory колоночная СУБД под ARM: векторный поиск, снапшоты на диск, репликация,
HTTP/gRPC/CLI. Целевые платформы — Apple silicon и Raspberry Pi 4/5 (64-битная ОС).

Все команды ниже выполнены и проверены, вывод в примерах настоящий.

---

## Содержание

- [Требования](#требования)
- [Сборка](#сборка)
- [Быстрый старт](#быстрый-старт)
- [Запуск сервера](#запуск-сервера)
- [CLI: veldbctl](#cli-veldbctl)
- [SQL](#sql)
- [Векторный поиск](#векторный-поиск)
- [HTTP REST](#http-rest)
- [gRPC](#grpc)
- [Репликация](#репликация)
- [Снапшоты и долговечность](#снапшоты-и-долговечность)
- [Бенчмарк ClickBench](#бенчмарк-clickbench)
- [Raspberry Pi](#raspberry-pi)
- [Разработка и тесты](#разработка-и-тесты)
- [Ограничения](#ограничения)

---

## Требования

| | |
|---|---|
| Rust | 1.85+ (`rustup update`) |
| `protoc` | для gRPC-кодогенерации: `brew install protobuf` / `apt install protobuf-compiler` |
| Архитектура | aarch64 (Apple silicon, Pi 4/5 на 64-битной ОС) или x86-64 — на не-ARM собирается, но без NEON, на скалярном пути |

Опционально, только для примеров: `jq` (форматирование JSON), `grpcurl` (gRPC из консоли).

## Сборка

```bash
git clone <репозиторий> && cd veldb
cargo build --release
```

Появятся три бинарника в `target/release/`:

| Бинарник | Зачем |
|---|---|
| `veldb` | сервер (HTTP + gRPC) |
| `veldbctl` | клиент с автодополнением |
| `veldb-bench` | раннер ClickBench |

Отладочная сборка (быстрее компилируется, медленнее работает — для бенчмарков не годится):

```bash
cargo build
```

## Быстрый старт

Три команды от нуля до векторного поиска:

```bash
# 1. сервер с каталогом данных
./target/release/veldb --data ./data &

# 2. таблица и данные
./target/release/veldbctl -e "CREATE TABLE docs (id INT, title TEXT, price DOUBLE, emb VECTOR(4))"
./target/release/veldbctl -e "INSERT INTO docs VALUES
    (1,'чай',   99.5,  '[1,0,0,0]'),
    (2,'кофе',  250.0, '[0,1,0,0]'),
    (3,'какао', 180.0, '[0.9,0.1,0,0]')"

# 3. ближайшие соседи
./target/release/veldbctl -e "SELECT id, title, l2_distance(emb,'[1,0,0,0]') AS d
                              FROM docs ORDER BY d LIMIT 2"
```

```
┌────┬───────┬──────────────────────┐
│ id │ title │ d                    │
├────┼───────┼──────────────────────┤
│ 1  │ чай   │ 0.0                  │
│ 3  │ какао │ 0.020000005140900612 │
└────┴───────┴──────────────────────┘
строк: 2
```

## Запуск сервера

```bash
./target/release/veldb --data ./data
```

```
veldb 0.1.0: таблиц 0, строк 0, хранилище ./data
HTTP  http://127.0.0.1:8080
gRPC  http://127.0.0.1:8081
```

Без `--data` база живёт только в памяти и исчезает вместе с процессом.

### Все флаги

| Флаг | По умолчанию | Что делает |
|---|---|---|
| `--data <каталог>` | нет | каталог данных: `snapshot.vdb` + `wal.log`. Без него — только память |
| `--http <адрес>` | `127.0.0.1:8080` | адрес REST-интерфейса |
| `--grpc <адрес>` | `127.0.0.1:8081` | адрес gRPC |
| `--durability fsync\|buffered` | `fsync` | `fsync` переживает потерю питания, `buffered` — только падение процесса |
| `--snapshot-interval <сек>` | `300` | период автоснапшота, `0` — выключить |
| `--max-memory <размер>` | без предела | потолок памяти под данные: `2GiB`, `512MiB`, `1073741824` |
| `--replicate-from <адрес>` | нет | включает режим реплики (только чтение) |
| `--replica-poll-ms <мс>` | `200` | как часто реплика опрашивает первичный узел |

Боевой запуск на сервере:

```bash
./target/release/veldb \
    --data /var/lib/veldb \
    --http 0.0.0.0:8080 \
    --grpc 0.0.0.0:8081 \
    --max-memory 8GiB \
    --snapshot-interval 300
```

Остановка — `Ctrl-C` (или `SIGINT`). По нему veldb пишет финальный снапшот и выходит;
`kill -9` тоже безопасен, но при следующем старте придётся проигрывать WAL.

Готовый systemd-юнит — `deploy/veldb.service`.

## CLI: veldbctl

```bash
./target/release/veldbctl                       # интерактивный режим
./target/release/veldbctl -e "SELECT 1"         # один запрос и выход
./target/release/veldbctl -f script.sql         # файл запросов
./target/release/veldbctl --url 10.0.0.5:8080   # другой сервер
```

В интерактивном режиме:

| | |
|---|---|
| `Tab` | автодополнение: ключевые слова, функции, имена таблиц и колонок. После `FROM`/`INTO`/`TABLE` — только таблицы |
| `Ctrl-C` | сбросить недописанный запрос (не выходит из клиента) |
| `Ctrl-D` | выход |
| `↑` / `↓` | история (хранится в `~/.veldb_history`) |
| `\dt` | список таблиц |
| `\d <таблица>` | колонки таблицы |
| `\timing` | показывать время выполнения |
| `\snapshot` | сбросить состояние на диск |
| `\refresh` | перечитать схему для автодополнения |
| `\q` | выход |

Многострочный ввод продолжается, пока открыта скобка или кавычка; завершается `;`.

Скрипт из файла:

```bash
cat > demo.sql <<'SQL'
SELECT title FROM docs WHERE price > 150 ORDER BY price DESC;
SHOW TABLES;
SQL
./target/release/veldbctl -f demo.sql
```

```
┌───────┐
│ title │
├───────┤
│ кофе  │
│ какао │
└───────┘
строк: 2
┌──────┬──────┐
│ name │ rows │
├──────┼──────┤
│ docs │ 4    │
└──────┴──────┘
строк: 1
```

## SQL

### Типы

| Тип | Хранение | Синонимы в `CREATE TABLE` |
|---|---|---|
| `INT` | `i64` | `INTEGER`, `BIGINT`, `SMALLINT`, `TINYINT` |
| `DOUBLE` | `f64` | `FLOAT`, `REAL`, `DECIMAL` |
| `BOOL` | байт | `BOOLEAN` |
| `TEXT` | арена + офсеты | `VARCHAR`, `CHAR`, `STRING` |
| `VECTOR(n)` | плотный `f32[n]` | — |

### DDL

```sql
CREATE TABLE sales (id INT, city TEXT, item TEXT, qty INT, price DOUBLE, ts INT);
CREATE TABLE IF NOT EXISTS docs (id INT, emb VECTOR(768));
DROP TABLE IF EXISTS old_table;
SHOW TABLES;
DESCRIBE sales;
```

### Вставка

```sql
INSERT INTO sales VALUES (1, 'Чита', 'чай', 3, 100.0, 1700000000);

-- несколько строк за раз (одна запись в WAL, один fsync — так и надо грузить пачки)
INSERT INTO sales VALUES
    (2, 'Чита',   'кофе',  1, 250.5, 1700003600),
    (3, 'Москва', 'чай',  10,  90.0, 1700007200);

-- свой порядок колонок
INSERT INTO sales (city, id, item, qty, price, ts) VALUES ('Питер', 4, 'чай', 0, 95.0, 1700014400);
```

Перечислять можно все колонки или ни одной: `NULL` в veldb нет, придумывать значение
за пользователя — хуже, чем сказать об этом.

### Выборка

```sql
SELECT * FROM sales WHERE qty > 2;
SELECT id, price * qty AS total FROM sales ORDER BY total DESC LIMIT 5;
SELECT id FROM sales WHERE city = 'Чита' AND qty BETWEEN 1 AND 5;
SELECT id FROM sales WHERE item IN ('чай','кофе') AND item NOT LIKE '%како%';
SELECT id FROM sales ORDER BY ts DESC LIMIT 10 OFFSET 20;
```

### Агрегаты и группировка

```sql
SELECT count(*), sum(qty), avg(price), min(ts), max(ts) FROM sales;
SELECT count(DISTINCT city) FROM sales;

SELECT city, count(*) AS c, round(avg(price), 2) AS avg_price
FROM sales
GROUP BY city
HAVING count(*) > 1
ORDER BY c DESC, city
LIMIT 10;

-- ORDER BY и GROUP BY понимают номер колонки, алиас и текст выражения
SELECT to_hour(ts) AS h, count(*) FROM sales GROUP BY 1 ORDER BY h;
```

Доступные агрегаты: `count`, `count(DISTINCT x)`, `uniq`, `sum`, `avg`, `min`, `max`.

### Функции

| Группа | Функции |
|---|---|
| Строки | `length`, `lower`, `upper`, `substring(s, from [, len])` |
| Числа | `abs`, `floor`, `ceil`, `round(x [, знаков])`, `sqrt` |
| Время | `EXTRACT(YEAR\|MONTH\|DAY\|HOUR\|MINUTE FROM ts)`, `to_year`, `to_month`, `to_day`, `to_hour`, `date_trunc('minute'\|'hour'\|'day', ts)` |
| Условия | `if(cond, a, b)`, `CASE WHEN … THEN … ELSE … END` |
| Векторы | `l2_distance`, `cosine_distance`, `inner_product` |

Время хранится как `INT` — unix-секунды (или дни от эпохи, если так удобнее).
Отдельного типа даты нет, календарные функции работают прямо по числу.

```sql
SELECT CASE WHEN qty = 0 THEN 'нет' WHEN qty < 5 THEN 'мало' ELSE 'много' END AS bucket,
       count(*)
FROM sales GROUP BY 1 ORDER BY 2 DESC;

SELECT date_trunc('hour', ts) AS h, count(*) FROM sales GROUP BY h ORDER BY h;
```

## Векторный поиск

Вектор пишется строкой `'[1,2,3]'` — так он проходит через любой SQL-диалект и любой
HTTP-клиент без экранирования. `ARRAY[1,2,3]` тоже принимается.

```sql
CREATE TABLE docs (id INT, title TEXT, emb VECTOR(768));
INSERT INTO docs VALUES (1, 'чай', '[0.013, -0.44, ...]');

-- k ближайших соседей
SELECT id, title, l2_distance(emb, '[0.013, -0.44, ...]') AS d
FROM docs
ORDER BY d
LIMIT 10;

-- с предфильтром: WHERE отрабатывает до расчёта расстояний
SELECT id FROM docs
WHERE title LIKE '%чай%'
ORDER BY cosine_distance(emb, '[...]')
LIMIT 5;
```

| Метрика | Смысл | Замечание |
|---|---|---|
| `l2_distance(col, q)` | квадрат евклидова расстояния | корень не берётся: для ранжирования не нужен, для отчёта — `sqrt(l2_distance(...))` |
| `cosine_distance(col, q)` | `1 - cos(a,b)`, диапазон `[0, 2]` | нулевой вектор считается максимально далёким, а не идеальным совпадением |
| `inner_product(col, q)` | `-(a·b)` | со знаком минус, чтобы «меньше = ближе», как у остальных |

Поиск точный (полный перебор), не приблизительный: результат совпадает с брутфорсом
до последней строки. Расстояния считаются NEON-интринсиками и распараллеливаются
по ядрам от 4096 строк.

## HTTP REST

По умолчанию `127.0.0.1:8080`.

| Метод | Путь | Тело | Ответ |
|---|---|---|---|
| `POST` | `/query` | `{"sql": "..."}` | `{columns, types, rows, row_count, elapsed_ms}` |
| `POST` | `/insert` | `{"table": "...", "rows": [[...]]}` | `{"inserted": n}` |
| `GET` | `/health` | — | статус, число таблиц/строк, занято байт, LSN |
| `GET` | `/schema` | — | таблицы с колонками и типами |
| `POST` | `/snapshot` | — | `{"lsn": n}` |
| `GET` | `/replication/wal?after=N` | — | записи WAL для реплики |
| `GET` | `/replication/snapshot` | — | полный снапшот, бинарно |

Ошибка запроса — `400` с полем `error`, а не `500`: SQL приходит снаружи, падать
пятисоткой на каждую опечатку неправильно.

### Примеры

```bash
# создать таблицу
curl -s -X POST 127.0.0.1:8080/query -H 'content-type: application/json' \
  -d '{"sql":"CREATE TABLE docs (id INT, title TEXT, price DOUBLE, emb VECTOR(4))"}' | jq -c
```
```json
{"columns":["result"],"types":["TEXT"],"rows":[["таблица 'docs' создана"]],"row_count":1,"elapsed_ms":3.34}
```

```bash
# вставка пачкой: строки — массивы значений в порядке колонок
curl -s -X POST 127.0.0.1:8080/insert -H 'content-type: application/json' \
  -d '{"table":"docs","rows":[
        [1,"чай",  99.5, [1,0,0,0]],
        [2,"кофе",250.0, [0,1,0,0]],
        [3,"какао",180.0,[0.9,0.1,0,0]]]}' | jq -c
```
```json
{"inserted":3}
```

```bash
# запрос
curl -s -X POST 127.0.0.1:8080/query -H 'content-type: application/json' \
  -d '{"sql":"SELECT id, title, l2_distance(emb, '"'"'[1,0,0,0]'"'"') AS d FROM docs ORDER BY d LIMIT 2"}' | jq -c
```
```json
{"columns":["id","title","d"],"types":["INT","TEXT","DOUBLE"],
 "rows":[[1,"чай",0.0],[3,"какао",0.020000005140900612]],"row_count":2,"elapsed_ms":0.43}
```

```bash
curl -s 127.0.0.1:8080/health | jq -c
```
```json
{"bytes_used":136,"next_lsn":3,"persistent":true,"rows":3,"status":"ok","tables":1,"version":"0.1.0"}
```

Значения возвращаются обычными скалярами JSON, а не размеченным перечислением:
с ответом работает любой клиент, не знающий внутренних имён вариантов.
`NaN`/`Infinity` уезжают строкой (`"inf"`) — иначе ответ перестал бы быть валидным JSON.

## gRPC

Описание — `proto/veldb.proto`, порт по умолчанию `127.0.0.1:8081`.
Методы: `Query`, `Insert`, `Health`, `Schema` — те же операции, что в REST.

Server reflection не включён, поэтому `grpcurl` запускается с `-proto`:

```bash
grpcurl -plaintext -proto proto/veldb.proto -import-path proto \
    127.0.0.1:8081 veldb.Veldb/Health
```
```json
{ "version": "0.1.0", "tables": "1", "rows": "3", "bytesUsed": "136",
  "nextLsn": "3", "persistent": true }
```

```bash
grpcurl -plaintext -proto proto/veldb.proto -import-path proto \
    -d '{"sql":"SELECT id, title FROM docs ORDER BY id LIMIT 2"}' \
    127.0.0.1:8081 veldb.Veldb/Query
```
```json
{ "columns": ["id", "title"], "types": ["INT", "TEXT"],
  "rows": [ {"values": [{"i": "1"}, {"s": "чай"}]},
            {"values": [{"i": "2"}, {"s": "кофе"}]} ] }
```

```bash
grpcurl -plaintext -proto proto/veldb.proto -import-path proto \
    -d '{"table":"docs","rows":[{"values":[
          {"i":4},{"s":"мате"},{"f":120.0},{"v":{"values":[0,0,1,0]}}]}]}' \
    127.0.0.1:8081 veldb.Veldb/Insert
```
```json
{ "inserted": "1" }
```

Тип значения берётся из схемы таблицы, а не из того, что прислал клиент: `{"i":3}`
в колонку `DOUBLE` корректно расширится до `3.0`, а вектор неверной длины будет
отвергнут с именем колонки в тексте ошибки.

Пример клиента на Rust — `tests/grpc.rs`.

### Кодогенерация для своего клиента

```bash
# Python
python -m grpc_tools.protoc -I proto --python_out=. --grpc_python_out=. proto/veldb.proto
# Go
protoc -I proto --go_out=. --go-grpc_out=. proto/veldb.proto
```

## Репликация

Первичный узел ничего не знает о репликах и не ждёт их: реплика сама тянет WAL.
Цена — отставание на один период опроса (`--replica-poll-ms`, по умолчанию 200 мс).

```bash
# первичный узел
./target/release/veldb --data /var/lib/veldb --http 0.0.0.0:8080 &

# реплика
./target/release/veldb --data /var/lib/veldb-replica \
    --replicate-from 127.0.0.1:8080 \
    --http 127.0.0.1:8090 --grpc 127.0.0.1:8091 &
```

```
veldb 0.1.0: таблиц 0, строк 0, хранилище /var/lib/veldb-replica
режим реплики: первичный узел 127.0.0.1:8080
HTTP  http://127.0.0.1:8090
gRPC  http://127.0.0.1:8091
реплика: применено записей 1, lsn=4
```

Проверка:

```bash
./target/release/veldbctl -e "INSERT INTO docs VALUES (5,'ройбуш',210.0,'[0,0,0,1]')"
sleep 1
./target/release/veldbctl --url 127.0.0.1:8090 -e "SELECT id, title FROM docs ORDER BY id DESC LIMIT 1"
```
```
┌────┬────────┐
│ id │ title  │
├────┼────────┤
│ 5  │ ройбуш │
└────┴────────┘
```

Запись в реплику отвергается — разъехаться с первичным узлом она не может:

```bash
./target/release/veldbctl --url 127.0.0.1:8090 -e "INSERT INTO docs VALUES (9,'x',1.0,'[0,0,0,0]')"
```
```
Error: 127.0.0.1:8090/query: узел работает как реплика: INSERT принимает только первичный узел
```

Как это устроено:

1. при старте реплика забирает полный снапшот (`GET /replication/snapshot`) и запоминает его LSN;
2. дальше опрашивает `GET /replication/wal?after=<LSN>` и применяет записи;
3. записи применяются тем же кодом, что и восстановление из WAL, поэтому «реплика
   применила не так, как первичный» невозможно по построению;
4. если первичный узел успел сделать снапшот и обрезать WAL, реплика видит разрыв
   в нумерации LSN и пересинхронизируется целиком, а не расходится тихо;
5. недоступность первичного узла — не фатальна: реплика продолжает отдавать чтение
   и повторяет попытку.

## Снапшоты и долговечность

Изменение сначала применяется в памяти, потом уходит в WAL, и только после этого
подтверждается клиенту. Падение до записи в WAL теряет неподтверждённое изменение —
для in-memory базы это корректное поведение, а не потеря данных.

```bash
curl -s -X POST 127.0.0.1:8080/snapshot | jq -c   # {"lsn":3}
ls -la ./data
# snapshot.vdb   полное состояние на момент LSN
# wal.log        всё, что произошло после
```

- Снапшот пишется во временный файл и переименовывается — наполовину записанного
  снапшота на диске не бывает.
- Оборванный (`kill -9`) или побитый хвост WAL отбрасывается по CRC, а не читается
  как данные; база после этого пригодна к работе, а не только к чтению.
- Битый снапшот — отказ открыться с явной ошибкой, а не молчаливая выдача мусора.
- `--durability fsync` (по умолчанию) переживает выключение питания; `fsync` делается
  один раз на *запрос*, а не на строку, поэтому пакетная вставка от него почти не страдает.
- `--durability buffered` убирает `fsync`: переживает падение процесса, но не машины.

## Бенчмарк ClickBench

Синтетические данные, без выкачивания датасета:

```bash
./target/release/veldb-bench --gen 3000000 --runs 3 --out bench/report-gen-3m.md
```

Настоящий ClickBench:

```bash
wget https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz && gzip -d hits.tsv.gz
./target/release/veldb-bench --tsv hits.tsv --rows 10000000 --runs 3 --out bench/report-hits-10m.md
```

Отладка одного запроса:

```bash
./target/release/veldb-bench --gen 1000000 --only 17,30 --runs 5
```

Последний прогон (3 млн строк, MacBook на M-серии, 12 ядер): **42 из 43** запросов,
суммарно ~5,1 с. Q29 не выполняется — ему нужен `REGEXP_REPLACE`.

Подробности, честная оговорка про размер датасета и список того, что оптимизировать
дальше — в [bench/README.md](bench/README.md).

## Raspberry Pi

```bash
# самый надёжный путь — собрать прямо на Pi (10-20 мин на Pi 4)
cargo build --release

# кросс-сборка с хоста
./scripts/build-pi.sh
```

Ориентир по памяти на схеме ClickBench — ~850 байт на строку, то есть на Pi 4 (4 ГБ)
это порядка 2,5 млн строк. На узких таблицах цифра совсем другая: строка из пяти `INT` —
40 байт. `--max-memory` на Pi обязателен: без него переполнение заканчивается
OOM-killer'ом, с ним вставка получает внятную ошибку, а записанное остаётся читаемым.

Подробности — [docs/raspberry-pi.md](docs/raspberry-pi.md), systemd-юнит —
`deploy/veldb.service`.

## Разработка и тесты

```bash
./check.sh                      # fmt + clippy -D warnings + все тесты
cargo test                      # 95 тестов
cargo test --test sql           # один файл
cargo test --test vector -- --nocapture
```

Структура:

```
src/
  column.rs table.rs      типы колонок и таблица
  simd.rs                 NEON-ядра расстояний + скалярный fallback
  like.rs                 матчер SQL LIKE
  plan.rs sql.rs          IR запроса и планировщик поверх sqlparser
  exec.rs                 векторизованный исполнитель
  codec.rs storage.rs     бинарный формат, снапшот, WAL
  db.rs                   каталог таблиц, точка входа execute
  http.rs grpc.rs         интерфейсы
  client.rs cli.rs        HTTP-клиент и логика CLI
  replication.rs          догон реплики
  bin/{server,cli,bench}.rs
tests/                    по файлу на модуль
bench/                    схема hits, 43 запроса, отчёты
```

Каждый модуль закрыт тестами в своём файле, включая неприятные случаи: обрыв WAL
на середине кадра, битый CRC, паритет NEON и скаляра, KNN против честного перебора,
сходимость реплики.

## Ограничения

Осознанные, а не «пока не дошли руки»:

- **`NULL` нет.** Агрегат по пустой выборке даёт нейтральное значение (0 / пустую строку).
- **Нет `UPDATE`/`DELETE`** — хранилище append-only.
- **Нет `JOIN`, подзапросов, CTE, `UNION`, `SELECT DISTINCT`** (вместо последнего — `GROUP BY`).
- **Нет транзакций.** Читающий запрос видит согласованный срез одной таблицы, но не нескольких.
- **Нет регулярных выражений.** Крейт `regex` добавим, когда они понадобятся не только бенчмарку.
- **Одна нода.** Шардинга нет: сначала одна должна быть быстрой.
- `uniq()` точный, а не приблизительный, как в ClickHouse: медленнее, зато воспроизводимо.
- Только little-endian платформы (проверяется при открытии файлов).

План работ и статус по фазам — [PLAN.md](PLAN.md).
