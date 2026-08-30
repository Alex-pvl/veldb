//! Бинарный формат для снапшотов, WAL и репликации.
//!
//! Своё, а не serde: колонка — уже плотный массив нужного вида, и весь «формат»
//! сводится к длине и байтам. Типы значений берутся из схемы, поэтому тег типа
//! на каждое значение не пишется.
//!
//! Порядок байтов — little-endian; при открытии файла это проверяется (`ensure_le`).

use crate::column::{Column, DataType, StrColumn, Value};
use crate::table::{Field, Schema};
use anyhow::{bail, Context, Result};

pub fn ensure_le() -> Result<()> {
    if cfg!(target_endian = "big") {
        bail!("формат хранения little-endian; big-endian платформа не поддерживается");
    }
    Ok(())
}

// --- запись -----------------------------------------------------------------

pub struct Writer {
    pub buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.buf.extend_from_slice(v);
    }

    pub fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    /// Сырой дамп среза чисел. Это и есть смысл колоночного снапшота:
    /// на диск уходит ровно та память, что лежит в колонке.
    fn raw<T: Copy>(&mut self, v: &[T]) {
        let n = std::mem::size_of_val(v);
        // SAFETY: читаем `v` как байты той же длины; выравнивание при чтении байтов не требуется.
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, n) };
        self.u64(v.len() as u64);
        self.buf.extend_from_slice(bytes);
    }

    pub fn data_type(&mut self, ty: DataType) {
        match ty {
            DataType::I64 => self.u8(1),
            DataType::F64 => self.u8(2),
            DataType::Bool => self.u8(3),
            DataType::Str => self.u8(4),
            DataType::Vector(d) => {
                self.u8(5);
                self.u64(d as u64);
            }
        }
    }

    pub fn schema(&mut self, s: &Schema) {
        self.u64(s.fields.len() as u64);
        for f in &s.fields {
            self.str(&f.name);
            self.data_type(f.ty);
        }
    }

    /// Значение без тега типа: тип известен из схемы.
    pub fn value(&mut self, v: &Value) {
        match v {
            Value::I64(x) => self.i64(*x),
            Value::F64(x) => self.f64(*x),
            Value::Bool(x) => self.u8(u8::from(*x)),
            Value::Str(x) => self.str(x),
            Value::Vector(x) => self.raw(x),
        }
    }

    pub fn column(&mut self, c: &Column) {
        match c {
            Column::I64(v) => self.raw(v),
            Column::F64(v) => self.raw(v),
            Column::Bool(v) => self.raw(v),
            Column::Vector { dim, data } => {
                self.u64(*dim as u64);
                self.raw(data);
            }
            Column::Str(s) => {
                let (buf, offsets) = s.parts();
                self.raw(offsets);
                self.bytes(buf);
            }
        }
    }
}

// --- чтение -----------------------------------------------------------------

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_done(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            bail!(
                "данные обрываются: нужно {n} байт, осталось {}",
                self.remaining()
            );
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Пропускает 8-байтовую сигнатуру в начале файла.
    pub fn take_magic(&mut self) -> Result<&'a [u8]> {
        self.take(8)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u64()? as usize;
        self.take(n)
    }

    pub fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).context("строка не в UTF-8")
    }

    /// Обратная сторона `Writer::raw`. Через `chunks_exact` — потому что байты
    /// из файла не выровнены под `i64`, и приводить указатель было бы UB.
    fn raw_i64(&mut self) -> Result<Vec<i64>> {
        let n = self.u64()? as usize;
        let b = self.take(n * 8)?;
        Ok(b.chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }

    fn raw_f64(&mut self) -> Result<Vec<f64>> {
        let n = self.u64()? as usize;
        let b = self.take(n * 8)?;
        Ok(b.chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }

    fn raw_f32(&mut self) -> Result<Vec<f32>> {
        let n = self.u64()? as usize;
        let b = self.take(n * 4)?;
        Ok(b.chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }

    fn raw_u32(&mut self) -> Result<Vec<u32>> {
        let n = self.u64()? as usize;
        let b = self.take(n * 4)?;
        Ok(b.chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }

    fn raw_u8(&mut self) -> Result<Vec<u8>> {
        let n = self.u64()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    pub fn data_type(&mut self) -> Result<DataType> {
        Ok(match self.u8()? {
            1 => DataType::I64,
            2 => DataType::F64,
            3 => DataType::Bool,
            4 => DataType::Str,
            5 => DataType::Vector(self.u64()? as usize),
            other => bail!("неизвестный код типа {other}"),
        })
    }

    pub fn schema(&mut self) -> Result<Schema> {
        let n = self.u64()? as usize;
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            fields.push(Field {
                name: self.str()?,
                ty: self.data_type()?,
            });
        }
        Ok(Schema { fields })
    }

    pub fn value(&mut self, ty: DataType) -> Result<Value> {
        Ok(match ty {
            DataType::I64 => Value::I64(self.i64()?),
            DataType::F64 => Value::F64(self.f64()?),
            DataType::Bool => Value::Bool(self.u8()? != 0),
            DataType::Str => Value::Str(self.str()?),
            DataType::Vector(dim) => {
                let v = self.raw_f32()?;
                if v.len() != dim {
                    bail!("вектор длины {} вместо {dim}", v.len());
                }
                Value::Vector(v)
            }
        })
    }

    pub fn column(&mut self, ty: DataType) -> Result<Column> {
        Ok(match ty {
            DataType::I64 => Column::I64(self.raw_i64()?),
            DataType::F64 => Column::F64(self.raw_f64()?),
            DataType::Bool => Column::Bool(self.raw_u8()?),
            DataType::Str => {
                let offsets = self.raw_u32()?;
                let buf = self.bytes()?.to_vec();
                Column::Str(StrColumn::from_parts(buf, offsets)?)
            }
            DataType::Vector(dim) => {
                let stored = self.u64()? as usize;
                if stored != dim {
                    bail!("в снапшоте VECTOR({stored}), в схеме VECTOR({dim})");
                }
                let data = self.raw_f32()?;
                if dim == 0 || data.len() % dim != 0 {
                    bail!("длина векторных данных {} не кратна {dim}", data.len());
                }
                Column::Vector { dim, data }
            }
        })
    }
}

/// CRC-32 (полином IEEE). Свой, потому что ради 12 строк не стоит тянуть крейт.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
