//! Типизированные колонки. Всё плотные массивы — это и есть весь смысл движка.
//!
//! ponytail: NULL не поддерживаем. Ни ClickBench-датасет, ни векторный поиск их не требуют,
//! а валидити-битмап протекает в каждое ядро исполнителя. Добавим, когда появится
//! источник данных, где null — это не «нет строки», а значимое значение.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    I64,
    F64,
    Bool,
    Str,
    /// Плотный `f32`-вектор фиксированной размерности.
    Vector(usize),
}

impl DataType {
    pub fn name(&self) -> String {
        match self {
            DataType::I64 => "INT".into(),
            DataType::F64 => "DOUBLE".into(),
            DataType::Bool => "BOOL".into(),
            DataType::Str => "TEXT".into(),
            DataType::Vector(d) => format!("VECTOR({d})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(String),
    Vector(Vec<f32>),
}

/// В JSON значение уходит как обычный скаляр (или массив чисел для вектора),
/// а не как размеченное перечисление: с ответом базы должен работать любой клиент,
/// не знающий про внутренние имена вариантов.
impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::I64(v) => s.serialize_i64(*v),
            Value::F64(v) => {
                // JSON не умеет NaN/Infinity — отдаём их строкой, чтобы ответ
                // оставался валидным JSON, а не превращался в `null`.
                if v.is_finite() {
                    s.serialize_f64(*v)
                } else {
                    s.serialize_str(&v.to_string())
                }
            }
            Value::Bool(v) => s.serialize_bool(*v),
            Value::Str(v) => s.serialize_str(v),
            Value::Vector(v) => s.collect_seq(v.iter()),
        }
    }
}

impl Value {
    /// Разбор значения из JSON под известный тип колонки.
    pub fn from_json(j: &serde_json::Value, ty: DataType) -> Result<Value> {
        use serde_json::Value as J;
        Ok(match (ty, j) {
            (DataType::I64, J::Number(n)) => {
                Value::I64(n.as_i64().ok_or_else(|| anyhow::anyhow!("{n} не целое"))?)
            }
            (DataType::F64, J::Number(n)) => {
                Value::F64(n.as_f64().ok_or_else(|| anyhow::anyhow!("{n} не число"))?)
            }
            (DataType::Bool, J::Bool(b)) => Value::Bool(*b),
            (DataType::Str, J::String(s)) => Value::Str(s.clone()),
            (DataType::Vector(dim), J::Array(a)) => {
                if a.len() != dim {
                    bail!("вектор длины {} в колонку VECTOR({dim})", a.len());
                }
                Value::Vector(
                    a.iter()
                        .map(|x| {
                            x.as_f64()
                                .map(|f| f as f32)
                                .ok_or_else(|| anyhow::anyhow!("элемент вектора не число"))
                        })
                        .collect::<Result<_>>()?,
                )
            }
            (ty, other) => bail!("значение {other} не подходит колонке {}", ty.name()),
        })
    }

    pub fn type_of(&self) -> DataType {
        match self {
            Value::I64(_) => DataType::I64,
            Value::F64(_) => DataType::F64,
            Value::Bool(_) => DataType::Bool,
            Value::Str(_) => DataType::Str,
            Value::Vector(v) => DataType::Vector(v.len()),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::I64(v) => Some(*v as f64),
            Value::F64(v) => Some(*v),
            Value::Bool(v) => Some(*v as u8 as f64),
            _ => None,
        }
    }
}

/// Строковая колонка-арена: один непрерывный буфер + офсеты.
///
/// ponytail: без словаря. Словарь выигрывает только на низкой кардинальности, а на
/// ClickBench-овом `URL` (десятки миллионов уникальных) хеш-таблица словаря съедает
/// больше, чем экономит. Словарное кодирование — опция на фазе тюнинга, по замеру.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StrColumn {
    buf: Vec<u8>,
    /// `offsets[i]..offsets[i+1]` — байты i-й строки. Длина = nrows + 1.
    offsets: Vec<u32>,
}

impl StrColumn {
    pub fn new() -> Self {
        StrColumn {
            buf: Vec::new(),
            offsets: vec![0],
        }
    }

    pub fn push(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
        self.offsets.push(self.buf.len() as u32);
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn get_bytes(&self, i: usize) -> &[u8] {
        let s = self.offsets[i] as usize;
        let e = self.offsets[i + 1] as usize;
        &self.buf[s..e]
    }

    #[inline]
    pub fn get(&self, i: usize) -> &str {
        // Инвариант: в буфер попадают только байты из `&str`, границы — на границах строк.
        std::str::from_utf8(self.get_bytes(i)).expect("StrColumn хранит только валидный UTF-8")
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }

    pub fn bytes_used(&self) -> usize {
        self.buf.len() + self.offsets.len() * 4
    }

    pub(crate) fn parts(&self) -> (&[u8], &[u32]) {
        (&self.buf, &self.offsets)
    }

    pub(crate) fn from_parts(buf: Vec<u8>, offsets: Vec<u32>) -> Result<Self> {
        if offsets.first() != Some(&0) || offsets.last().copied() != Some(buf.len() as u32) {
            bail!("StrColumn: офсеты не согласованы с буфером");
        }
        if offsets.windows(2).any(|w| w[0] > w[1]) {
            bail!("StrColumn: офсеты не монотонны");
        }
        std::str::from_utf8(&buf)?;
        Ok(StrColumn { buf, offsets })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Column {
    I64(Vec<i64>),
    F64(Vec<f64>),
    Bool(Vec<u8>),
    Str(StrColumn),
    /// Row-major: строка `i` — это `data[i*dim..(i+1)*dim]`.
    Vector {
        dim: usize,
        data: Vec<f32>,
    },
}

impl Column {
    pub fn empty(ty: DataType) -> Column {
        match ty {
            DataType::I64 => Column::I64(Vec::new()),
            DataType::F64 => Column::F64(Vec::new()),
            DataType::Bool => Column::Bool(Vec::new()),
            DataType::Str => Column::Str(StrColumn::new()),
            DataType::Vector(dim) => Column::Vector {
                dim,
                data: Vec::new(),
            },
        }
    }

    pub fn data_type(&self) -> DataType {
        match self {
            Column::I64(_) => DataType::I64,
            Column::F64(_) => DataType::F64,
            Column::Bool(_) => DataType::Bool,
            Column::Str(_) => DataType::Str,
            Column::Vector { dim, .. } => DataType::Vector(*dim),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Column::I64(v) => v.len(),
            Column::F64(v) => v.len(),
            Column::Bool(v) => v.len(),
            Column::Str(v) => v.len(),
            Column::Vector { dim, data } => {
                if *dim == 0 {
                    0
                } else {
                    data.len() / dim
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn reserve(&mut self, extra: usize) {
        match self {
            Column::I64(v) => v.reserve(extra),
            Column::F64(v) => v.reserve(extra),
            Column::Bool(v) => v.reserve(extra),
            Column::Str(_) => {}
            Column::Vector { dim, data } => data.reserve(extra * *dim),
        }
    }

    pub fn push(&mut self, v: &Value) -> Result<()> {
        match (self, v) {
            (Column::I64(c), Value::I64(x)) => c.push(*x),
            (Column::F64(c), Value::F64(x)) => c.push(*x),
            // Целое в вещественную колонку принимаем: literal `1` в SQL это I64.
            (Column::F64(c), Value::I64(x)) => c.push(*x as f64),
            (Column::Bool(c), Value::Bool(x)) => c.push(*x as u8),
            (Column::Str(c), Value::Str(x)) => c.push(x),
            (Column::Vector { dim, data }, Value::Vector(x)) => {
                if x.len() != *dim {
                    bail!("размерность вектора {} != {} у колонки", x.len(), dim);
                }
                data.extend_from_slice(x);
            }
            (col, val) => bail!(
                "тип значения {} не подходит колонке {}",
                val.type_of().name(),
                col.data_type().name()
            ),
        }
        Ok(())
    }

    pub fn get(&self, i: usize) -> Value {
        match self {
            Column::I64(c) => Value::I64(c[i]),
            Column::F64(c) => Value::F64(c[i]),
            Column::Bool(c) => Value::Bool(c[i] != 0),
            Column::Str(c) => Value::Str(c.get(i).to_string()),
            Column::Vector { dim, data } => Value::Vector(data[i * dim..(i + 1) * dim].to_vec()),
        }
    }

    /// Срез i-го вектора без копирования. `None`, если колонка не векторная.
    #[inline]
    pub fn vector_at(&self, i: usize) -> Option<&[f32]> {
        match self {
            Column::Vector { dim, data } => Some(&data[i * dim..(i + 1) * dim]),
            _ => None,
        }
    }

    pub fn bytes_used(&self) -> usize {
        match self {
            Column::I64(v) => v.len() * 8,
            Column::F64(v) => v.len() * 8,
            Column::Bool(v) => v.len(),
            Column::Str(v) => v.bytes_used(),
            Column::Vector { data, .. } => data.len() * 4,
        }
    }
}
