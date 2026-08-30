//! Схема и таблица. Таблица append-only: колонки только растут.

use crate::column::{Column, DataType, Value};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: DataType,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<(&str, DataType)>) -> Schema {
        Schema {
            fields: fields
                .into_iter()
                .map(|(name, ty)| Field {
                    name: name.to_string(),
                    ty,
                })
                .collect(),
        }
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        // Имена колонок нечувствительны к регистру, как в большинстве SQL-движков.
        self.fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.index_of(name).map(|i| &self.fields[i])
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    fn validate(&self) -> Result<()> {
        if self.fields.is_empty() {
            bail!("схема без колонок");
        }
        for (i, f) in self.fields.iter().enumerate() {
            if f.name.is_empty() {
                bail!("пустое имя колонки");
            }
            if self.fields[..i]
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&f.name))
            {
                bail!("колонка '{}' объявлена дважды", f.name);
            }
            if let DataType::Vector(0) = f.ty {
                bail!("колонка '{}': VECTOR(0) бессмысленна", f.name);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub schema: Schema,
    columns: Vec<Column>,
    nrows: usize,
}

impl Table {
    pub fn new(name: &str, schema: Schema) -> Result<Table> {
        schema.validate()?;
        let columns = schema.fields.iter().map(|f| Column::empty(f.ty)).collect();
        Ok(Table {
            name: name.to_string(),
            schema,
            columns,
            nrows: 0,
        })
    }

    pub fn nrows(&self) -> usize {
        self.nrows
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.schema.index_of(name).map(|i| &self.columns[i])
    }

    pub fn insert(&mut self, row: &[Value]) -> Result<()> {
        if row.len() != self.columns.len() {
            bail!(
                "строка из {} значений, в таблице {} колонок",
                row.len(),
                self.columns.len()
            );
        }
        // Проверяем всё до записи: полустрока в колоночном хранилище рвёт выравнивание строк.
        for (i, v) in row.iter().enumerate() {
            let want = self.schema.fields[i].ty;
            let got = v.type_of();
            let ok = want == got
                || (want == DataType::F64 && got == DataType::I64)
                || matches!((want, got), (DataType::Vector(a), DataType::Vector(b)) if a == b);
            if !ok {
                bail!(
                    "колонка '{}': ожидался {}, получен {}",
                    self.schema.fields[i].name,
                    want.name(),
                    got.name()
                );
            }
        }
        for (col, v) in self.columns.iter_mut().zip(row) {
            col.push(v)?;
        }
        self.nrows += 1;
        Ok(())
    }

    pub fn insert_many(&mut self, rows: &[Vec<Value>]) -> Result<usize> {
        for c in self.columns.iter_mut() {
            c.reserve(rows.len());
        }
        for r in rows {
            self.insert(r)?;
        }
        Ok(rows.len())
    }

    pub fn row(&self, i: usize) -> Vec<Value> {
        self.columns.iter().map(|c| c.get(i)).collect()
    }

    pub fn bytes_used(&self) -> usize {
        self.columns.iter().map(|c| c.bytes_used()).sum()
    }

    pub(crate) fn from_columns(name: &str, schema: Schema, columns: Vec<Column>) -> Result<Table> {
        schema.validate()?;
        if columns.len() != schema.fields.len() {
            bail!(
                "колонок {}, полей в схеме {}",
                columns.len(),
                schema.fields.len()
            );
        }
        let nrows = columns.first().map(|c| c.len()).unwrap_or(0);
        for (c, f) in columns.iter().zip(&schema.fields) {
            if c.data_type() != f.ty {
                bail!(
                    "колонка '{}': тип {} != {}",
                    f.name,
                    c.data_type().name(),
                    f.ty.name()
                );
            }
            if c.len() != nrows {
                bail!(
                    "колонка '{}': {} строк, ожидалось {}",
                    f.name,
                    c.len(),
                    nrows
                );
            }
        }
        Ok(Table {
            name: name.to_string(),
            schema,
            columns,
            nrows,
        })
    }
}
