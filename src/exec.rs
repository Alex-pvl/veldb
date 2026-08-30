//! Исполнитель. Векторизованный: каждый узел выражения считается сразу по всей
//! выборке, а не построчно — так один проход по колонке остаётся линейным по памяти.
//!
//! ponytail: без батчинга. Промежуточный массив на 10M строк — это 80 МБ, на целевых
//! машинах терпимо. Потолок известен: как только рабочий набор перестанет влезать,
//! сюда добавляется разбиение на чанки по 64K строк, интерфейс `eval` от этого не меняется.

use crate::column::{Column, DataType, StrColumn, Value};
use crate::like::Pattern;
use crate::plan::{AggFunc, BinOp, Expr, Metric, ScalarFunc, Select, UnOp};
use crate::simd;
use crate::table::Table;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub types: Vec<DataType>,
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    pub fn message(text: &str) -> QueryResult {
        QueryResult {
            columns: vec!["result".into()],
            types: vec![DataType::Str],
            rows: vec![vec![Value::Str(text.into())]],
        }
    }
}

/// Набор колонок, по которому считаются выражения: либо базовая таблица,
/// либо промежуточный результат агрегации.
pub struct Frame<'a> {
    cols: Vec<&'a Column>,
}

impl<'a> Frame<'a> {
    pub fn of_table(t: &'a Table) -> Frame<'a> {
        Frame {
            cols: t.columns().iter().collect(),
        }
    }

    fn of_columns(cols: &'a [Column]) -> Frame<'a> {
        Frame {
            cols: cols.iter().collect(),
        }
    }
}

/// Какие строки кадра участвуют. `All` — важный частный случай: он позволяет
/// отдать колонку заимствованным срезом, без копирования.
#[derive(Debug, Clone)]
pub enum Sel {
    All(usize),
    Ids(Vec<u32>),
}

impl Sel {
    pub fn len(&self) -> usize {
        match self {
            Sel::All(n) => *n,
            Sel::Ids(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn row(&self, i: usize) -> usize {
        match self {
            Sel::All(_) => i,
            Sel::Ids(v) => v[i] as usize,
        }
    }

    fn permute(&self, order: &[u32]) -> Sel {
        Sel::Ids(order.iter().map(|&i| self.row(i as usize) as u32).collect())
    }
}

/// Результат вычисления выражения по выборке. Длина 1 означает константу
/// и разворачивается по месту (broadcast).
#[derive(Debug, Clone)]
pub enum Arr<'a> {
    I64(Cow<'a, [i64]>),
    F64(Cow<'a, [f64]>),
    Bool(Cow<'a, [u8]>),
    Str(Vec<Cow<'a, str>>),
    Vector { dim: usize, rows: Vec<&'a [f32]> },
}

impl Arr<'_> {
    pub fn len(&self) -> usize {
        match self {
            Arr::I64(v) => v.len(),
            Arr::F64(v) => v.len(),
            Arr::Bool(v) => v.len(),
            Arr::Str(v) => v.len(),
            Arr::Vector { rows, .. } => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn data_type(&self) -> DataType {
        match self {
            Arr::I64(_) => DataType::I64,
            Arr::F64(_) => DataType::F64,
            Arr::Bool(_) => DataType::Bool,
            Arr::Str(_) => DataType::Str,
            Arr::Vector { dim, .. } => DataType::Vector(*dim),
        }
    }

    fn value(&self, i: usize) -> Value {
        let i = if self.len() == 1 { 0 } else { i };
        match self {
            Arr::I64(v) => Value::I64(v[i]),
            Arr::F64(v) => Value::F64(v[i]),
            Arr::Bool(v) => Value::Bool(v[i] != 0),
            Arr::Str(v) => Value::Str(v[i].to_string()),
            Arr::Vector { rows, .. } => Value::Vector(rows[i].to_vec()),
        }
    }

    fn as_bool(&self) -> Result<&[u8]> {
        match self {
            Arr::Bool(v) => Ok(v),
            other => bail!(
                "ожидалось логическое выражение, получено {}",
                other.data_type().name()
            ),
        }
    }

    /// Числовое представление. Копирует только когда тип не совпал.
    fn to_f64(&self) -> Result<Cow<'_, [f64]>> {
        Ok(match self {
            Arr::F64(v) => Cow::Borrowed(v),
            Arr::I64(v) => Cow::Owned(v.iter().map(|&x| x as f64).collect()),
            Arr::Bool(v) => Cow::Owned(v.iter().map(|&x| x as f64).collect()),
            other => bail!("ожидалось число, получено {}", other.data_type().name()),
        })
    }

    fn into_column(self) -> Result<Column> {
        Ok(match self {
            Arr::I64(v) => Column::I64(v.into_owned()),
            Arr::F64(v) => Column::F64(v.into_owned()),
            Arr::Bool(v) => Column::Bool(v.into_owned()),
            Arr::Str(v) => {
                let mut c = StrColumn::new();
                for s in &v {
                    c.push(s);
                }
                Column::Str(c)
            }
            Arr::Vector { dim, rows } => {
                let mut data = Vec::with_capacity(rows.len() * dim);
                for r in rows {
                    data.extend_from_slice(r);
                }
                Column::Vector { dim, data }
            }
        })
    }
}

/// `l` и `r` вычислены по одной выборке, поэтому длины либо равны, либо одна из них 1.
fn pair_len(l: usize, r: usize) -> Result<usize> {
    match (l, r) {
        (a, b) if a == b => Ok(a),
        (1, b) => Ok(b),
        (a, 1) => Ok(a),
        (a, b) => bail!("несовместимые длины операндов: {a} и {b}"),
    }
}

macro_rules! zip_map {
    ($n:expr, $l:expr, $r:expr, $f:expr) => {{
        let (ls, rs) = ($l, $r);
        let (lb, rb) = (ls.len() == 1, rs.len() == 1);
        let f = $f;
        (0..$n)
            .map(|i| f(&ls[if lb { 0 } else { i }], &rs[if rb { 0 } else { i }]))
            .collect::<Vec<_>>()
    }};
}

fn eval_binary<'a>(op: BinOp, l: Arr<'a>, r: Arr<'a>) -> Result<Arr<'a>> {
    let n = pair_len(l.len(), r.len())?;

    if matches!(op, BinOp::And | BinOp::Or) {
        let (lb, rb) = (l.as_bool()?, r.as_bool()?);
        let and = op == BinOp::And;
        return Ok(Arr::Bool(Cow::Owned(zip_map!(
            n,
            lb,
            rb,
            |a: &u8, b: &u8| {
                u8::from(if and {
                    *a != 0 && *b != 0
                } else {
                    *a != 0 || *b != 0
                })
            }
        ))));
    }

    let is_cmp = matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    );

    // Строки сравниваются лексикографически; арифметика над ними бессмысленна.
    if let (Arr::Str(ls), Arr::Str(rs)) = (&l, &r) {
        if !is_cmp {
            bail!("арифметический оператор применён к строкам");
        }
        return Ok(Arr::Bool(Cow::Owned(zip_map!(n, ls, rs, |a: &Cow<
            str,
        >,
                                                            b: &Cow<
            str,
        >| {
            u8::from(cmp_ok(op, a.as_ref().cmp(b.as_ref())))
        }))));
    }
    if matches!(l, Arr::Str(_)) || matches!(r, Arr::Str(_)) {
        bail!("сравнение строки с числом: приведите типы явно через CAST");
    }

    // Целочисленный путь сохраняем: он точен там, где f64 уже теряет младшие биты.
    if let (Arr::I64(ls), Arr::I64(rs)) = (&l, &r) {
        if is_cmp {
            return Ok(Arr::Bool(Cow::Owned(zip_map!(
                n,
                ls,
                rs,
                |a: &i64, b: &i64| { u8::from(cmp_ok(op, a.cmp(b))) }
            ))));
        }
        if op != BinOp::Div {
            return Ok(Arr::I64(Cow::Owned(zip_map!(
                n,
                ls,
                rs,
                |a: &i64, b: &i64| match op {
                    BinOp::Add => a.saturating_add(*b),
                    BinOp::Sub => a.saturating_sub(*b),
                    BinOp::Mul => a.saturating_mul(*b),
                    // Деление на ноль не роняет запрос: остаток по нулю считаем нулём.
                    BinOp::Mod =>
                        if *b == 0 {
                            0
                        } else {
                            a.wrapping_rem(*b)
                        },
                    _ => unreachable!(),
                }
            ))));
        }
    }

    let (lf, rf) = (l.to_f64()?, r.to_f64()?);
    if is_cmp {
        return Ok(Arr::Bool(Cow::Owned(zip_map!(
            n,
            &lf,
            &rf,
            |a: &f64, b: &f64| {
                u8::from(match a.partial_cmp(b) {
                    Some(o) => cmp_ok(op, o),
                    // NaN не равен ничему, включая себя.
                    None => op == BinOp::NotEq,
                })
            }
        ))));
    }
    Ok(Arr::F64(Cow::Owned(zip_map!(
        n,
        &lf,
        &rf,
        |a: &f64, b: &f64| match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div => a / b,
            BinOp::Mod => a % b,
            _ => unreachable!(),
        }
    ))))
}

fn cmp_ok(op: BinOp, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        BinOp::Eq => o == Equal,
        BinOp::NotEq => o != Equal,
        BinOp::Lt => o == Less,
        BinOp::LtEq => o != Greater,
        BinOp::Gt => o == Greater,
        BinOp::GtEq => o != Less,
        _ => unreachable!(),
    }
}

pub fn eval<'a>(e: &Expr, f: &Frame<'a>, sel: &Sel) -> Result<Arr<'a>> {
    let n = sel.len();
    Ok(match e {
        Expr::Col(i) => {
            let col = *f.cols.get(*i).context("индекс колонки вне схемы кадра")?;
            gather(col, sel)
        }
        Expr::Lit(v) => match v {
            Value::I64(x) => Arr::I64(Cow::Owned(vec![*x])),
            Value::F64(x) => Arr::F64(Cow::Owned(vec![*x])),
            Value::Bool(x) => Arr::Bool(Cow::Owned(vec![u8::from(*x)])),
            Value::Str(x) => Arr::Str(vec![Cow::Owned(x.clone())]),
            Value::Vector(_) => bail!("вектор допустим только как аргумент метрики"),
        },
        Expr::Binary { op, l, r } => eval_binary(*op, eval(l, f, sel)?, eval(r, f, sel)?)?,
        Expr::Unary { op, e } => {
            let v = eval(e, f, sel)?;
            match op {
                UnOp::Not => Arr::Bool(Cow::Owned(
                    v.as_bool()?.iter().map(|&x| u8::from(x == 0)).collect(),
                )),
                UnOp::Neg => match v {
                    Arr::I64(x) => {
                        Arr::I64(Cow::Owned(x.iter().map(|v| v.saturating_neg()).collect()))
                    }
                    other => Arr::F64(Cow::Owned(other.to_f64()?.iter().map(|v| -v).collect())),
                },
            }
        }
        Expr::Like {
            e,
            pattern,
            negated,
            case_insensitive,
        } => {
            let v = eval(e, f, sel)?;
            let Arr::Str(items) = &v else {
                bail!("LIKE применим только к строкам");
            };
            let pat = Pattern::compile(&if *case_insensitive {
                pattern.to_lowercase()
            } else {
                pattern.clone()
            });
            Arr::Bool(Cow::Owned(
                items
                    .iter()
                    .map(|s| {
                        let hit = if *case_insensitive {
                            pat.matches(&s.to_lowercase())
                        } else {
                            pat.matches(s)
                        };
                        u8::from(hit != *negated)
                    })
                    .collect(),
            ))
        }
        Expr::InList { e, list, negated } => {
            let v = eval(e, f, sel)?;
            let mut mask = vec![0u8; v.len()];
            for lit in list {
                let cmp =
                    eval_binary(BinOp::Eq, v.clone(), eval(&Expr::Lit(lit.clone()), f, sel)?)?;
                for (m, &b) in mask.iter_mut().zip(cmp.as_bool()?) {
                    *m |= b;
                }
            }
            if *negated {
                mask.iter_mut().for_each(|m| *m = u8::from(*m == 0));
            }
            Arr::Bool(Cow::Owned(mask))
        }
        Expr::Between {
            e,
            low,
            high,
            negated,
        } => {
            let v = eval(e, f, sel)?;
            let lo = eval_binary(BinOp::GtEq, v.clone(), eval(low, f, sel)?)?;
            let hi = eval_binary(BinOp::LtEq, v, eval(high, f, sel)?)?;
            let mut m = eval_binary(BinOp::And, lo, hi)?;
            if *negated {
                if let Arr::Bool(b) = &mut m {
                    b.to_mut().iter_mut().for_each(|x| *x = u8::from(*x == 0));
                }
            }
            m
        }
        Expr::Cast { e, ty } => cast(eval(e, f, sel)?, *ty)?,
        Expr::Func { func, args } => {
            let vals = args
                .iter()
                .map(|a| eval(a, f, sel))
                .collect::<Result<Vec<_>>>()?;
            scalar_func(*func, vals)?
        }
        Expr::Distance { metric, col, query } => {
            let c = *f
                .cols
                .get(*col)
                .context("индекс векторной колонки вне схемы")?;
            Arr::F64(Cow::Owned(distances(c, sel, *metric, query)?))
        }
        Expr::Agg { .. } => bail!("агрегат вычисляется на этапе группировки, а не здесь"),
    })
    .and_then(|a: Arr<'a>| {
        // Инвариант, на который опирается broadcast: длина либо 1 (константа), либо n.
        if a.len() == n || a.len() == 1 {
            Ok(a)
        } else {
            bail!("выражение дало {} значений вместо {n}", a.len())
        }
    })
}

fn gather<'a>(col: &'a Column, sel: &Sel) -> Arr<'a> {
    match (col, sel) {
        (Column::I64(v), Sel::All(_)) => Arr::I64(Cow::Borrowed(v)),
        (Column::F64(v), Sel::All(_)) => Arr::F64(Cow::Borrowed(v)),
        (Column::Bool(v), Sel::All(_)) => Arr::Bool(Cow::Borrowed(v)),
        (Column::I64(v), Sel::Ids(ids)) => {
            Arr::I64(Cow::Owned(ids.iter().map(|&i| v[i as usize]).collect()))
        }
        (Column::F64(v), Sel::Ids(ids)) => {
            Arr::F64(Cow::Owned(ids.iter().map(|&i| v[i as usize]).collect()))
        }
        (Column::Bool(v), Sel::Ids(ids)) => {
            Arr::Bool(Cow::Owned(ids.iter().map(|&i| v[i as usize]).collect()))
        }
        (Column::Str(s), _) => Arr::Str(
            (0..sel.len())
                .map(|i| Cow::Borrowed(s.get(sel.row(i))))
                .collect(),
        ),
        (Column::Vector { dim, .. }, _) => Arr::Vector {
            dim: *dim,
            rows: (0..sel.len())
                .map(|i| col.vector_at(sel.row(i)).unwrap())
                .collect(),
        },
    }
}

fn distances(col: &Column, sel: &Sel, metric: Metric, q: &[f32]) -> Result<Vec<f64>> {
    let Column::Vector { dim, .. } = col else {
        bail!("метрика применима только к VECTOR-колонке");
    };
    if *dim != q.len() {
        bail!("вектор запроса длины {}, колонка VECTOR({dim})", q.len());
    }
    let f = |i: usize| -> f64 {
        let v = col.vector_at(sel.row(i)).unwrap();
        (match metric {
            Metric::L2 => simd::l2_sq(v, q),
            Metric::Cosine => simd::cosine_distance(v, q),
            Metric::NegInnerProduct => -simd::dot(v, q),
        }) as f64
    };
    // Порог подобран так, чтобы не платить за пул потоков на мелких выборках.
    const PAR_MIN_ROWS: usize = 4096;
    Ok(if sel.len() >= PAR_MIN_ROWS {
        (0..sel.len()).into_par_iter().map(f).collect()
    } else {
        (0..sel.len()).map(f).collect()
    })
}

fn cast(v: Arr<'_>, ty: DataType) -> Result<Arr<'static>> {
    Ok(match ty {
        DataType::I64 => match &v {
            Arr::Str(s) => Arr::I64(Cow::Owned(
                s.iter()
                    .map(|x| x.trim().parse::<i64>().unwrap_or(0))
                    .collect(),
            )),
            other => Arr::I64(Cow::Owned(
                other.to_f64()?.iter().map(|x| *x as i64).collect(),
            )),
        },
        DataType::F64 => match &v {
            Arr::Str(s) => Arr::F64(Cow::Owned(
                s.iter()
                    .map(|x| x.trim().parse::<f64>().unwrap_or(0.0))
                    .collect(),
            )),
            other => Arr::F64(Cow::Owned(other.to_f64()?.into_owned())),
        },
        DataType::Bool => Arr::Bool(Cow::Owned(
            v.to_f64()?.iter().map(|x| u8::from(*x != 0.0)).collect(),
        )),
        DataType::Str => Arr::Str(
            (0..v.len())
                .map(|i| Cow::Owned(render(&v.value(i))))
                .collect(),
        ),
        DataType::Vector(_) => bail!("CAST в VECTOR не поддерживается"),
    })
}

fn scalar_func<'a>(func: ScalarFunc, args: Vec<Arr<'a>>) -> Result<Arr<'a>> {
    use ScalarFunc::*;
    // Длину задают неконстантные аргументы: если выборка пуста, а рядом стоит
    // литерал, результат тоже пуст. Брать максимум было бы ошибкой — она стоила
    // паники на пустом результате в ClickBench Q40.
    let n = args.iter().map(|a| a.len()).find(|&l| l != 1).unwrap_or(1);
    let at = |a: &Arr<'a>, i: usize| -> usize {
        if a.len() == 1 {
            0
        } else {
            i
        }
    };
    Ok(match func {
        Length => match &args[0] {
            Arr::Str(s) => Arr::I64(Cow::Owned(
                s.iter().map(|x| x.chars().count() as i64).collect(),
            )),
            other => bail!(
                "length применим к строкам, получено {}",
                other.data_type().name()
            ),
        },
        Lower | Upper => match &args[0] {
            Arr::Str(s) => Arr::Str(
                s.iter()
                    .map(|x| {
                        Cow::Owned(if func == Lower {
                            x.to_lowercase()
                        } else {
                            x.to_uppercase()
                        })
                    })
                    .collect(),
            ),
            other => bail!(
                "lower/upper применимы к строкам, получено {}",
                other.data_type().name()
            ),
        },
        Abs => match &args[0] {
            Arr::I64(v) => Arr::I64(Cow::Owned(v.iter().map(|x| x.saturating_abs()).collect())),
            other => Arr::F64(Cow::Owned(
                other.to_f64()?.iter().map(|x| x.abs()).collect(),
            )),
        },
        Floor | Ceil | Sqrt => {
            let v = args[0].to_f64()?;
            Arr::F64(Cow::Owned(
                v.iter()
                    .map(|x| match func {
                        Floor => x.floor(),
                        Ceil => x.ceil(),
                        _ => x.sqrt(),
                    })
                    .collect(),
            ))
        }
        Round => {
            let v = args[0].to_f64()?;
            let digits = match args.get(1) {
                None => 0i32,
                Some(a) => a.to_f64()?.first().copied().unwrap_or(0.0) as i32,
            };
            let k = 10f64.powi(digits);
            Arr::F64(Cow::Owned(v.iter().map(|x| (x * k).round() / k).collect()))
        }
        Substring => {
            let Arr::Str(s) = &args[0] else {
                bail!("substring применим к строкам");
            };
            let start = args[1].to_f64()?.to_vec();
            let count = args
                .get(2)
                .map(|a| a.to_f64().map(|v| v.to_vec()))
                .transpose()?;
            Arr::Str(
                (0..n)
                    .map(|i| {
                        let src = &s[at(&args[0], i)];
                        // SQL нумерует символы с единицы.
                        let from = (start[at(&args[1], i)] as i64 - 1).max(0) as usize;
                        let take = match &count {
                            Some(c) => c[at(&args[2], i)].max(0.0) as usize,
                            None => usize::MAX,
                        };
                        Cow::Owned(src.chars().skip(from).take(take).collect::<String>())
                    })
                    .collect(),
            )
        }
        ExtractHour | ExtractMinute | ExtractDay | ExtractMonth | ExtractYear => {
            let v = args[0].to_f64()?;
            Arr::I64(Cow::Owned(
                v.iter().map(|x| extract_part(func, *x as i64)).collect(),
            ))
        }
        DateTrunc => {
            let Arr::Str(unit) = &args[0] else {
                bail!("date_trunc: первым аргументом единица времени, например 'minute'");
            };
            let secs = args[1].to_f64()?;
            let step = match unit[0].to_lowercase().as_str() {
                "second" => 1i64,
                "minute" => 60,
                "hour" => 3600,
                "day" => 86_400,
                other => bail!("date_trunc: единица '{other}' не поддерживается"),
            };
            Arr::I64(Cow::Owned(
                secs.iter()
                    .map(|x| (*x as i64).div_euclid(step) * step)
                    .collect(),
            ))
        }
        Coalesce => {
            // NULL нет, поэтому coalesce — это просто первый аргумент. Оставлен ради
            // совместимости с чужими запросами, которые его пишут «на всякий случай».
            args.into_iter().next().unwrap()
        }
        If => {
            let cond = args[0].as_bool()?.to_vec();
            let pick = |i: usize| if cond[at(&args[0], i)] != 0 { 1 } else { 2 };
            match (&args[1], &args[2]) {
                (Arr::Str(_), Arr::Str(_)) => Arr::Str(
                    (0..n)
                        .map(|i| {
                            let a = &args[pick(i)];
                            match a.value(at(a, i)) {
                                Value::Str(s) => Cow::Owned(s),
                                _ => unreachable!(),
                            }
                        })
                        .collect(),
                ),
                (a, b) => {
                    let (x, y) = (a.to_f64()?.to_vec(), b.to_f64()?.to_vec());
                    Arr::F64(Cow::Owned(
                        (0..n)
                            .map(|i| {
                                if cond[at(&args[0], i)] != 0 {
                                    x[at(&args[1], i)]
                                } else {
                                    y[at(&args[2], i)]
                                }
                            })
                            .collect(),
                    ))
                }
            }
        }
    })
}

/// Разбор unix-времени без внешнего календаря: нам нужны только эти пять полей,
/// а тянуть `chrono` ради них — лишняя зависимость в горячем пути.
fn extract_part(func: ScalarFunc, ts: i64) -> i64 {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    match func {
        ScalarFunc::ExtractHour => secs / 3600,
        ScalarFunc::ExtractMinute => (secs % 3600) / 60,
        _ => {
            let (y, m, d) = civil_from_days(days);
            match func {
                ScalarFunc::ExtractDay => d as i64,
                ScalarFunc::ExtractMonth => m as i64,
                _ => y,
            }
        }
    }
}

/// Алгоритм Говарда Хиннанта: дни от эпохи → (год, месяц, день) григорианского календаря.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn render(v: &Value) -> String {
    match v {
        Value::I64(x) => x.to_string(),
        Value::F64(x) => x.to_string(),
        Value::Bool(x) => x.to_string(),
        Value::Str(x) => x.clone(),
        Value::Vector(x) => {
            let body: Vec<String> = x.iter().map(|f| f.to_string()).collect();
            format!("[{}]", body.join(","))
        }
    }
}

// --- выполнение SELECT ------------------------------------------------------

pub fn run_select(plan: &Select, table: &Table) -> Result<QueryResult> {
    let base = Frame::of_table(table);
    let sel = match &plan.filter {
        None => Sel::All(table.nrows()),
        Some(f) => {
            let mask = eval(f, &base, &Sel::All(table.nrows()))?;
            let mask = mask
                .as_bool()
                .context("WHERE должен давать логическое значение")?;
            if mask.len() == 1 {
                // Константное условие: либо всё, либо ничего.
                if mask[0] != 0 {
                    Sel::All(table.nrows())
                } else {
                    Sel::Ids(Vec::new())
                }
            } else {
                Sel::Ids(
                    mask.iter()
                        .enumerate()
                        .filter(|(_, &m)| m != 0)
                        .map(|(i, _)| i as u32)
                        .collect(),
                )
            }
        }
    };

    // После агрегации выражения проекции считаются уже по промежуточному кадру,
    // поэтому владеть его колонками нужно до конца функции.
    let grouped;
    let (frame, sel, projection, order_by, having) = if plan.is_aggregate() {
        let g = aggregate(plan, &base, &sel)?;
        grouped = g;
        (
            Frame::of_columns(&grouped.columns),
            Sel::All(grouped.ngroups),
            grouped.projection.clone(),
            grouped.order_by.clone(),
            grouped.having.clone(),
        )
    } else {
        (
            base,
            sel,
            plan.projection.clone(),
            plan.order_by.clone(),
            None,
        )
    };

    let sel = match &having {
        None => sel,
        Some(h) => {
            let mask = eval(h, &frame, &sel)?;
            let mask = mask
                .as_bool()
                .context("HAVING должен давать логическое значение")?;
            if mask.len() == 1 {
                if mask[0] != 0 {
                    sel
                } else {
                    Sel::Ids(Vec::new())
                }
            } else {
                let ids = (0..sel.len())
                    .filter(|&i| mask[i] != 0)
                    .map(|i| sel.row(i) as u32)
                    .collect();
                Sel::Ids(ids)
            }
        }
    };

    let sel = order_and_limit(&frame, sel, &order_by, plan.limit, plan.offset)?;

    let mut columns = Vec::with_capacity(projection.len());
    let mut types = Vec::with_capacity(projection.len());
    let mut arrays = Vec::with_capacity(projection.len());
    for p in &projection {
        let a = eval(&p.expr, &frame, &sel)?;
        columns.push(p.alias.clone());
        types.push(a.data_type());
        arrays.push(a);
    }
    let rows = (0..sel.len())
        .map(|i| arrays.iter().map(|a| a.value(i)).collect())
        .collect();
    Ok(QueryResult {
        columns,
        types,
        rows,
    })
}

fn order_and_limit(
    frame: &Frame<'_>,
    sel: Sel,
    keys: &[crate::plan::OrderKey],
    limit: Option<usize>,
    offset: usize,
) -> Result<Sel> {
    if keys.is_empty() {
        // Без ORDER BY порядок — это порядок вставки; срезаем без сортировки.
        let n = sel.len();
        let start = offset.min(n);
        let end = limit.map_or(n, |l| (start + l).min(n));
        if start == 0 && end == n {
            return Ok(sel);
        }
        return Ok(sel.permute(&(start as u32..end as u32).collect::<Vec<_>>()));
    }

    let arrays = keys
        .iter()
        .map(|k| eval(&k.expr, frame, &sel))
        .collect::<Result<Vec<_>>>()?;
    let asc: Vec<bool> = keys.iter().map(|k| k.asc).collect();
    let cmp = |a: &u32, b: &u32| -> std::cmp::Ordering {
        for (arr, &up) in arrays.iter().zip(&asc) {
            let o = cmp_arr(arr, *a as usize, *b as usize);
            if o != std::cmp::Ordering::Equal {
                return if up { o } else { o.reverse() };
            }
        }
        // Стабилизируем результат: без этого равные ключи выдают разный порядок
        // от запуска к запуску, и бенчмарк перестаёт быть воспроизводимым.
        a.cmp(b)
    };

    let n = sel.len();
    let mut idx: Vec<u32> = (0..n as u32).collect();
    let need = limit.map(|l| (offset + l).min(n));
    match need {
        // Частичная сортировка окупается только когда хвост реально большой.
        Some(k) if k * 4 < n => {
            idx.select_nth_unstable_by(k, cmp);
            idx.truncate(k);
            idx.sort_unstable_by(cmp);
        }
        _ => idx.sort_unstable_by(cmp),
    }
    let start = offset.min(idx.len());
    let end = limit.map_or(idx.len(), |l| (start + l).min(idx.len()));
    Ok(sel.permute(&idx[start..end]))
}

fn cmp_arr(a: &Arr<'_>, i: usize, j: usize) -> std::cmp::Ordering {
    match a {
        Arr::I64(v) => v[i].cmp(&v[j]),
        Arr::F64(v) => v[i].total_cmp(&v[j]),
        Arr::Bool(v) => v[i].cmp(&v[j]),
        Arr::Str(v) => v[i].as_ref().cmp(v[j].as_ref()),
        Arr::Vector { .. } => std::cmp::Ordering::Equal,
    }
}

struct Grouped {
    columns: Vec<Column>,
    ngroups: usize,
    projection: Vec<crate::plan::Projection>,
    order_by: Vec<crate::plan::OrderKey>,
    having: Option<Expr>,
}

fn aggregate(plan: &Select, base: &Frame<'_>, sel: &Sel) -> Result<Grouped> {
    // 1. Ключи группировки.
    let key_arrays = plan
        .group_by
        .iter()
        .map(|e| eval(e, base, sel))
        .collect::<Result<Vec<_>>>()?;
    let (group_of, ngroups) = assign_groups(&key_arrays, sel.len())?;

    // 2. Все агрегаты из проекции и сортировки — по одному разу на уникальное выражение.
    let mut aggs: Vec<Expr> = Vec::new();
    let mut collect = |e: &Expr| {
        e.walk(&mut |node| {
            if matches!(node, Expr::Agg { .. }) {
                if !aggs.contains(node) {
                    aggs.push(node.clone());
                }
                return true;
            }
            false
        });
    };
    plan.projection.iter().for_each(|p| collect(&p.expr));
    plan.order_by.iter().for_each(|o| collect(&o.expr));
    if let Some(h) = &plan.having {
        collect(h);
    }

    // 3. Колонки промежуточного кадра: сначала ключи, потом агрегаты.
    let mut columns = Vec::with_capacity(key_arrays.len() + aggs.len());
    for a in key_arrays {
        columns.push(pick_first_per_group(&a, &group_of, ngroups).into_column()?);
    }
    // Агрегаты независимы друг от друга, поэтому считаются параллельно.
    // Порог по числу строк — чтобы на мелких выборках не платить за пул потоков.
    const PAR_MIN_ROWS: usize = 65_536;
    if aggs.len() > 1 && sel.len() >= PAR_MIN_ROWS {
        let computed: Vec<Column> = aggs
            .par_iter()
            .map(|agg| run_agg(agg, base, sel, &group_of, ngroups))
            .collect::<Result<Vec<_>>>()?;
        columns.extend(computed);
    } else {
        for agg in &aggs {
            columns.push(run_agg(agg, base, sel, &group_of, ngroups)?);
        }
    }

    // 4. Переписываем проекцию и сортировку на индексы промежуточного кадра.
    // Агрегаты идут первыми: `sum(x)` при `GROUP BY x` должен совпасть целиком,
    // а не превратиться в `sum(ключ)`.
    let pats: Vec<(Expr, Expr)> = aggs
        .iter()
        .enumerate()
        .map(|(i, a)| (a.clone(), Expr::Col(plan.group_by.len() + i)))
        .chain(
            plan.group_by
                .iter()
                .enumerate()
                .map(|(i, g)| (g.clone(), Expr::Col(i))),
        )
        .collect();
    let rewrite = |e: &Expr| -> Expr { e.substitute(&pats) };
    let projection: Vec<_> = plan
        .projection
        .iter()
        .map(|p| crate::plan::Projection {
            expr: rewrite(&p.expr),
            alias: p.alias.clone(),
        })
        .collect();
    let order_by: Vec<_> = plan
        .order_by
        .iter()
        .map(|o| crate::plan::OrderKey {
            expr: rewrite(&o.expr),
            asc: o.asc,
        })
        .collect();

    // Всё, что осталось ссылаться на базовые колонки, — это `SELECT x` без `GROUP BY x`.
    for p in &projection {
        let mut bad = false;
        p.expr.walk(&mut |node| {
            if let Expr::Col(i) = node {
                if *i >= columns.len() {
                    bad = true;
                }
            }
            bad
        });
        if bad {
            bail!("'{}' не агрегат и не входит в GROUP BY", p.alias);
        }
    }

    let having = plan.having.as_ref().map(&rewrite);
    Ok(Grouped {
        columns,
        ngroups,
        projection,
        order_by,
        having,
    })
}

/// Ключи склеиваются в байтовую строку. ponytail: аллокация на строку выборки.
/// Потолок известен — на 10M строк это заметная доля времени GROUP BY; замена
/// (составной хеш + сверка) имеет смысл только после профиля на реальных данных.
fn assign_groups(keys: &[Arr<'_>], n: usize) -> Result<(Vec<u32>, usize)> {
    if keys.is_empty() {
        // Агрегат без GROUP BY — одна группа на всю выборку.
        return Ok((vec![0; n], 1));
    }
    let mut map: FxHashMap<Box<[u8]>, u32> = FxHashMap::default();
    let mut group_of = Vec::with_capacity(n);
    let mut buf = Vec::with_capacity(64);
    for i in 0..n {
        buf.clear();
        for k in keys {
            let j = if k.len() == 1 { 0 } else { i };
            match k {
                Arr::I64(v) => buf.extend_from_slice(&v[j].to_le_bytes()),
                Arr::F64(v) => buf.extend_from_slice(&v[j].to_bits().to_le_bytes()),
                Arr::Bool(v) => buf.push(v[j]),
                Arr::Str(v) => {
                    // Длина обязательна: без неё ключи ("a","bc") и ("ab","c") совпадут.
                    buf.extend_from_slice(&(v[j].len() as u32).to_le_bytes());
                    buf.extend_from_slice(v[j].as_bytes());
                }
                Arr::Vector { .. } => bail!("группировка по векторной колонке не поддерживается"),
            }
        }
        // `entry` потребовал бы владеющий ключ на каждой строке, а не только на
        // новой группе: на 3M строк это 3M лишних аллокаций. Сначала ищем по срезу.
        let id = match map.get(buf.as_slice()) {
            Some(id) => *id,
            None => {
                let id = map.len() as u32;
                map.insert(buf.as_slice().into(), id);
                id
            }
        };
        group_of.push(id);
    }
    let ngroups = map.len();
    Ok((group_of, ngroups))
}

/// Значение ключа для каждой группы — берём из первой попавшейся строки группы:
/// внутри группы ключ по определению одинаков.
fn pick_first_per_group<'a>(a: &Arr<'a>, group_of: &[u32], ngroups: usize) -> Arr<'a> {
    let mut src = vec![usize::MAX; ngroups];
    for (row, &g) in group_of.iter().enumerate() {
        let s = &mut src[g as usize];
        if *s == usize::MAX {
            *s = if a.len() == 1 { 0 } else { row };
        }
    }
    match a {
        Arr::I64(v) => Arr::I64(Cow::Owned(src.iter().map(|&i| v[i]).collect())),
        Arr::F64(v) => Arr::F64(Cow::Owned(src.iter().map(|&i| v[i]).collect())),
        Arr::Bool(v) => Arr::Bool(Cow::Owned(src.iter().map(|&i| v[i]).collect())),
        Arr::Str(v) => Arr::Str(src.iter().map(|&i| v[i].clone()).collect()),
        Arr::Vector { dim, rows } => Arr::Vector {
            dim: *dim,
            rows: src.iter().map(|&i| rows[i]).collect(),
        },
    }
}

fn value_key(a: &Arr<'_>, i: usize) -> u64 {
    match a {
        Arr::I64(v) => v[i] as u64,
        Arr::F64(v) => v[i].to_bits(),
        Arr::Bool(v) => v[i] as u64,
        Arr::Str(v) => {
            use std::hash::{Hash, Hasher};
            let mut h = rustc_hash::FxHasher::default();
            v[i].as_ref().hash(&mut h);
            h.finish()
        }
        Arr::Vector { .. } => 0,
    }
}

fn run_agg(
    agg: &Expr,
    base: &Frame<'_>,
    sel: &Sel,
    group_of: &[u32],
    ngroups: usize,
) -> Result<Column> {
    let Expr::Agg {
        func,
        arg,
        distinct,
    } = agg
    else {
        bail!("run_agg вызван не на агрегате");
    };
    let n = sel.len();

    // COUNT(*) — единственный агрегат без аргумента.
    let Some(arg) = arg else {
        let mut counts = vec![0i64; ngroups];
        for &g in group_of.iter().take(n) {
            counts[g as usize] += 1;
        }
        return Ok(Column::I64(counts));
    };
    let v = eval(arg, base, sel)?;
    let idx = |i: usize| if v.len() == 1 { 0 } else { i };

    if *distinct {
        let mut sets = vec![FxHashSet::<u64>::default(); ngroups];
        for i in 0..n {
            sets[group_of[i] as usize].insert(value_key(&v, idx(i)));
        }
        // ponytail: различие считается по 64-битному ключу значения. Для чисел это
        // само значение (точно), для строк — хеш: на 10M уникальных строк шанс
        // коллизии ~3e-6. Если понадобится строгая точность — хранить сами строки.
        return Ok(Column::I64(sets.iter().map(|s| s.len() as i64).collect()));
    }

    match func {
        AggFunc::Count => {
            let mut counts = vec![0i64; ngroups];
            for &g in group_of.iter().take(n) {
                counts[g as usize] += 1;
            }
            Ok(Column::I64(counts))
        }
        AggFunc::Sum => match &v {
            Arr::I64(x) => {
                let mut acc = vec![0i64; ngroups];
                for i in 0..n {
                    let a = &mut acc[group_of[i] as usize];
                    *a = a.saturating_add(x[idx(i)]);
                }
                Ok(Column::I64(acc))
            }
            other => {
                let x = other.to_f64()?;
                let mut acc = vec![0f64; ngroups];
                for i in 0..n {
                    acc[group_of[i] as usize] += x[idx(i)];
                }
                Ok(Column::F64(acc))
            }
        },
        AggFunc::Avg => {
            let x = v.to_f64()?;
            let mut sum = vec![0f64; ngroups];
            let mut cnt = vec![0f64; ngroups];
            for i in 0..n {
                let g = group_of[i] as usize;
                sum[g] += x[idx(i)];
                cnt[g] += 1.0;
            }
            // Пустых групп не бывает, но при `WHERE`, отсеявшем всё, единственная
            // группа приходит с нулевым счётчиком — отдаём 0, а не NaN.
            Ok(Column::F64(
                sum.iter()
                    .zip(&cnt)
                    .map(|(s, c)| if *c == 0.0 { 0.0 } else { s / c })
                    .collect(),
            ))
        }
        AggFunc::Min | AggFunc::Max => {
            let is_min = *func == AggFunc::Min;
            match &v {
                Arr::Str(x) => {
                    let mut acc: Vec<Option<&str>> = vec![None; ngroups];
                    for i in 0..n {
                        let g = group_of[i] as usize;
                        let s = x[idx(i)].as_ref();
                        acc[g] = Some(match acc[g] {
                            None => s,
                            Some(cur) => {
                                if (s < cur) == is_min {
                                    s
                                } else {
                                    cur
                                }
                            }
                        });
                    }
                    let mut c = StrColumn::new();
                    for a in acc {
                        c.push(a.unwrap_or(""));
                    }
                    Ok(Column::Str(c))
                }
                Arr::I64(x) => {
                    let mut acc = vec![None::<i64>; ngroups];
                    for i in 0..n {
                        let g = group_of[i] as usize;
                        let val = x[idx(i)];
                        acc[g] = Some(match acc[g] {
                            None => val,
                            Some(cur) => {
                                if is_min {
                                    cur.min(val)
                                } else {
                                    cur.max(val)
                                }
                            }
                        });
                    }
                    Ok(Column::I64(
                        acc.into_iter().map(|a| a.unwrap_or(0)).collect(),
                    ))
                }
                other => {
                    let x = other.to_f64()?;
                    let mut acc = vec![None::<f64>; ngroups];
                    for i in 0..n {
                        let g = group_of[i] as usize;
                        let val = x[idx(i)];
                        acc[g] = Some(match acc[g] {
                            None => val,
                            Some(cur) => {
                                if is_min {
                                    cur.min(val)
                                } else {
                                    cur.max(val)
                                }
                            }
                        });
                    }
                    Ok(Column::F64(
                        acc.into_iter().map(|a| a.unwrap_or(0.0)).collect(),
                    ))
                }
            }
        }
    }
}
