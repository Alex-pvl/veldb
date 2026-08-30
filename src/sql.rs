//! Разбор SQL и построение плана.
//!
//! Парсинг отдан `sqlparser`: приоритеты операторов, кавычки и escape-последовательности —
//! это ровно та работа, которую нет смысла делать заново. Здесь только связывание имён
//! со схемой и сведение общего AST к нашему IR.

use crate::column::{DataType, Value};
use crate::plan::{
    AggFunc, BinOp, Expr, Metric, OrderKey, Projection, ScalarFunc, Select, Statement, UnOp,
};
use crate::table::{Field, Schema};
use anyhow::{anyhow, bail, Context, Result};
use sqlparser::ast as sa;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// Источник схем для связывания имён колонок.
pub trait Catalog {
    fn schema_of(&self, table: &str) -> Option<Schema>;
}

pub fn plan(sql: &str, catalog: &dyn Catalog) -> Result<Statement> {
    let mut stmts = Parser::parse_sql(&GenericDialect {}, sql).context("разбор SQL")?;
    match stmts.len() {
        0 => bail!("пустой запрос"),
        1 => plan_statement(stmts.pop().unwrap(), catalog),
        n => bail!("за раз выполняется один запрос, получено {n}"),
    }
}

fn ident_of(name: &sa::ObjectName) -> Result<String> {
    name.0
        .last()
        .and_then(|p| p.as_ident())
        .map(|i| i.value.clone())
        .ok_or_else(|| anyhow!("не удалось прочитать имя '{name}'"))
}

fn plan_statement(stmt: sa::Statement, catalog: &dyn Catalog) -> Result<Statement> {
    match stmt {
        sa::Statement::CreateTable(ct) => {
            if ct.query.is_some() {
                bail!("CREATE TABLE AS SELECT не поддерживается");
            }
            let fields = ct
                .columns
                .iter()
                .map(|c| {
                    Ok(Field {
                        name: c.name.value.clone(),
                        ty: map_type(&c.data_type)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Statement::CreateTable {
                name: ident_of(&ct.name)?,
                schema: Schema { fields },
                if_not_exists: ct.if_not_exists,
            })
        }
        sa::Statement::Drop {
            object_type,
            if_exists,
            names,
            ..
        } => {
            if object_type != sa::ObjectType::Table {
                bail!("DROP поддерживается только для TABLE");
            }
            let name = names.first().ok_or_else(|| anyhow!("DROP без имени"))?;
            Ok(Statement::DropTable {
                name: ident_of(name)?,
                if_exists,
            })
        }
        sa::Statement::Insert(ins) => plan_insert(ins, catalog),
        sa::Statement::Query(q) => Ok(Statement::Select(Box::new(plan_query(*q, catalog)?))),
        sa::Statement::ShowTables { .. } => Ok(Statement::ShowTables),
        sa::Statement::ExplainTable { table_name, .. } => {
            Ok(Statement::Describe(ident_of(&table_name)?))
        }
        other => bail!("оператор не поддерживается: {other}"),
    }
}

fn map_type(ty: &sa::DataType) -> Result<DataType> {
    use sa::DataType as T;
    Ok(match ty {
        T::TinyInt(_) | T::SmallInt(_) | T::Int(_) | T::Integer(_) | T::BigInt(_) => DataType::I64,
        T::Float(_) | T::Real | T::Double(_) | T::DoublePrecision | T::Decimal(_) => DataType::F64,
        T::Boolean | T::Bool => DataType::Bool,
        T::Char(_) | T::Varchar(_) | T::Text | T::String(_) | T::Nvarchar(_) => DataType::Str,
        // `VECTOR(n)` парсер отдаёт как пользовательский тип: своего варианта у него нет.
        T::Custom(name, args) => {
            let n = ident_of(name)?;
            if !n.eq_ignore_ascii_case("vector") {
                bail!("неизвестный тип '{n}'");
            }
            let dim: usize = args
                .first()
                .ok_or_else(|| anyhow!("VECTOR требует размерность: VECTOR(768)"))?
                .trim()
                .parse()
                .context("размерность VECTOR")?;
            DataType::Vector(dim)
        }
        other => bail!("тип не поддерживается: {other}"),
    })
}

fn plan_insert(ins: sa::Insert, catalog: &dyn Catalog) -> Result<Statement> {
    let table = match &ins.table {
        sa::TableObject::TableName(n) => ident_of(n)?,
        other => bail!("INSERT в {other} не поддерживается"),
    };
    let schema = catalog
        .schema_of(&table)
        .ok_or_else(|| anyhow!("таблица '{table}' не найдена"))?;

    let columns: Option<Vec<String>> = if ins.columns.is_empty() {
        None
    } else {
        Some(ins.columns.iter().map(ident_of).collect::<Result<_>>()?)
    };
    // Целевые типы нужны сразу: без них `'[1,2,3]'` не отличить от обычной строки.
    let target_types: Vec<DataType> = match &columns {
        None => schema.fields.iter().map(|f| f.ty).collect(),
        Some(names) => names
            .iter()
            .map(|n| {
                schema
                    .field(n)
                    .map(|f| f.ty)
                    .ok_or_else(|| anyhow!("в таблице '{table}' нет колонки '{n}'"))
            })
            .collect::<Result<_>>()?,
    };

    let source = ins
        .source
        .ok_or_else(|| anyhow!("INSERT без VALUES не поддерживается"))?;
    let values = match *source.body {
        sa::SetExpr::Values(v) => v,
        _ => bail!("INSERT ... SELECT не поддерживается"),
    };

    let mut rows = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        if row.len() != target_types.len() {
            bail!(
                "в строке {} значений, ожидалось {}",
                row.len(),
                target_types.len()
            );
        }
        rows.push(
            row.iter()
                .zip(&target_types)
                .map(|(e, ty)| literal(e, Some(*ty)))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(Statement::Insert {
        table,
        columns,
        rows,
    })
}

/// Литерал в контексте известного целевого типа.
fn literal(e: &sa::Expr, want: Option<DataType>) -> Result<Value> {
    if let Some(DataType::Vector(dim)) = want {
        let v = vector_literal(e)?;
        if v.len() != dim {
            bail!("вектор длины {} в колонку VECTOR({dim})", v.len());
        }
        return Ok(Value::Vector(v));
    }
    let v = match e {
        sa::Expr::Value(v) => match &v.value {
            sa::Value::Number(n, _) => {
                if let Ok(i) = n.parse::<i64>() {
                    Value::I64(i)
                } else {
                    Value::F64(n.parse().with_context(|| format!("число '{n}'"))?)
                }
            }
            sa::Value::SingleQuotedString(s) | sa::Value::DoubleQuotedString(s) => {
                Value::Str(s.clone())
            }
            sa::Value::Boolean(b) => Value::Bool(*b),
            other => bail!("литерал не поддерживается: {other}"),
        },
        sa::Expr::UnaryOp {
            op: sa::UnaryOperator::Minus,
            expr,
        } => match literal(expr, None)? {
            Value::I64(i) => Value::I64(-i),
            Value::F64(f) => Value::F64(-f),
            other => bail!("унарный минус к {}", other.type_of().name()),
        },
        other => bail!("здесь ожидался литерал, а не '{other}'"),
    };
    // Целое в вещественную колонку расширяем сразу, чтобы не гонять это через исполнитель.
    Ok(match (want, v) {
        (Some(DataType::F64), Value::I64(i)) => Value::F64(i as f64),
        (_, v) => v,
    })
}

/// Вектор пишется либо как `ARRAY[..]`/`[..]`, либо строкой `'[1,2,3]'` —
/// второе переживает любой диалект и любой HTTP-клиент.
fn vector_literal(e: &sa::Expr) -> Result<Vec<f32>> {
    match e {
        sa::Expr::Array(a) => a
            .elem
            .iter()
            .map(|x| {
                literal(x, None)?
                    .as_f64()
                    .map(|f| f as f32)
                    .ok_or_else(|| anyhow!("элемент вектора должен быть числом"))
            })
            .collect(),
        sa::Expr::Value(v) => match &v.value {
            sa::Value::SingleQuotedString(s) | sa::Value::DoubleQuotedString(s) => parse_vec_str(s),
            other => bail!("вектор задаётся как '[1,2,3]', получено {other}"),
        },
        other => bail!("вектор задаётся как '[1,2,3]' или ARRAY[1,2,3], получено '{other}'"),
    }
}

pub fn parse_vec_str(s: &str) -> Result<Vec<f32>> {
    let body = s.trim().trim_start_matches('[').trim_end_matches(']');
    if body.trim().is_empty() {
        bail!("пустой вектор");
    }
    body.split(',')
        .map(|p| {
            p.trim()
                .parse::<f32>()
                .with_context(|| format!("элемент вектора '{p}'"))
        })
        .collect()
}

// --- SELECT -----------------------------------------------------------------

fn plan_query(q: sa::Query, catalog: &dyn Catalog) -> Result<Select> {
    if q.with.is_some() {
        bail!("CTE (WITH) не поддерживаются");
    }
    let select = match *q.body {
        sa::SetExpr::Select(s) => *s,
        _ => bail!("поддерживается только простой SELECT (без UNION и подзапросов)"),
    };
    if select.distinct.is_some() {
        bail!("SELECT DISTINCT не поддерживается; используйте GROUP BY");
    }
    if select.from.len() > 1 || select.from.first().is_some_and(|f| !f.joins.is_empty()) {
        bail!("поддерживается не больше одной таблицы в FROM, без JOIN");
    }
    // FROM может не быть вовсе: `SELECT 1`, `SELECT 2 + 2`. Источником тогда служит
    // одна пустая строка — так это работает во всех знакомых SQL-движках, и на таком
    // запросе принято проверять, что сервер вообще жив.
    let (table, schema) = match select.from.first() {
        None => (None, Schema::default()),
        Some(from) => {
            let name = match &from.relation {
                sa::TableFactor::Table { name, .. } => ident_of(name)?,
                other => bail!("источник '{other}' не поддерживается"),
            };
            let schema = catalog
                .schema_of(&name)
                .ok_or_else(|| anyhow!("таблица '{name}' не найдена"))?;
            (Some(name), schema)
        }
    };

    let filter = select
        .selection
        .as_ref()
        .map(|e| bind(e, &schema))
        .transpose()
        .context("WHERE")?;

    let mut projection = Vec::new();
    for item in &select.projection {
        match item {
            sa::SelectItem::Wildcard(_) => {
                for (i, f) in schema.fields.iter().enumerate() {
                    projection.push(Projection {
                        expr: Expr::Col(i),
                        alias: f.name.clone(),
                    });
                }
            }
            sa::SelectItem::UnnamedExpr(e) => {
                projection.push(Projection {
                    expr: bind(e, &schema)?,
                    alias: e.to_string(),
                });
            }
            sa::SelectItem::ExprWithAlias { expr, alias } => {
                projection.push(Projection {
                    expr: bind(expr, &schema)?,
                    alias: alias.value.clone(),
                });
            }
            other => bail!("элемент SELECT не поддерживается: {other}"),
        }
    }
    if projection.is_empty() {
        bail!("пустой список SELECT");
    }

    // GROUP BY разбирается после проекции: он умеет ссылаться на неё по номеру
    // (`GROUP BY 1, URL` встречается в готовых бенчмарк-запросах).
    let group_by = match &select.group_by {
        sa::GroupByExpr::Expressions(exprs, _) => exprs
            .iter()
            .map(|e| bind_output_ref(e, &schema, &select.projection, &projection))
            .collect::<Result<Vec<_>>>()
            .context("GROUP BY")?,
        sa::GroupByExpr::All(_) => bail!("GROUP BY ALL не поддерживается"),
    };
    if group_by.iter().any(|e| e.contains_agg()) {
        bail!("агрегат в GROUP BY");
    }

    let having = select
        .having
        .as_ref()
        .map(|e| bind(e, &schema))
        .transpose()
        .context("HAVING")?;
    if having.is_some() && group_by.is_empty() && !projection.iter().any(|p| p.expr.contains_agg())
    {
        bail!("HAVING без агрегатов и GROUP BY: используйте WHERE");
    }

    let order_by = match &q.order_by {
        None => Vec::new(),
        Some(ob) => match &ob.kind {
            sa::OrderByKind::All(_) => bail!("ORDER BY ALL не поддерживается"),
            sa::OrderByKind::Expressions(items) => items
                .iter()
                .map(|it| {
                    Ok(OrderKey {
                        expr: bind_output_ref(&it.expr, &schema, &select.projection, &projection)?,
                        asc: it.options.asc.unwrap_or(true),
                    })
                })
                .collect::<Result<Vec<_>>>()
                .context("ORDER BY")?,
        },
    };

    let (limit, offset) = limit_offset(&q.limit_clause)?;

    Ok(Select {
        table,
        filter,
        group_by,
        having,
        projection,
        order_by,
        limit,
        offset,
    })
}

/// ORDER BY и GROUP BY умеют ссылаться на список SELECT тремя способами:
/// порядковым номером, алиасом и дословным текстом выражения. Разрешаем все три,
/// иначе половина готовых бенчмарк-запросов не запустится.
fn bind_output_ref(
    e: &sa::Expr,
    schema: &Schema,
    raw_projection: &[sa::SelectItem],
    bound: &[Projection],
) -> Result<Expr> {
    if let sa::Expr::Value(v) = e {
        if let sa::Value::Number(n, _) = &v.value {
            let idx: usize = n.parse().context("порядковый номер колонки")?;
            if idx == 0 || idx > bound.len() {
                bail!("ссылка на колонку {idx}: в SELECT их {}", bound.len());
            }
            return Ok(bound[idx - 1].expr.clone());
        }
    }
    if let sa::Expr::Identifier(id) = e {
        if let Some(p) = bound
            .iter()
            .find(|p| p.alias.eq_ignore_ascii_case(&id.value))
        {
            // Алиас перекрывает колонку только если такой колонки нет — иначе
            // `SELECT x AS y ... ORDER BY x` вело бы себя неожиданно.
            if schema.index_of(&id.value).is_none() {
                return Ok(p.expr.clone());
            }
        }
    }
    let text = e.to_string();
    for (raw, p) in raw_projection.iter().zip(bound) {
        let raw_text = match raw {
            sa::SelectItem::UnnamedExpr(x) => x.to_string(),
            sa::SelectItem::ExprWithAlias { expr, .. } => expr.to_string(),
            _ => continue,
        };
        if raw_text.eq_ignore_ascii_case(&text) {
            return Ok(p.expr.clone());
        }
    }
    bind(e, schema)
}

fn limit_offset(clause: &Option<sa::LimitClause>) -> Result<(Option<usize>, usize)> {
    let as_usize = |e: &sa::Expr| -> Result<usize> {
        match literal(e, None)? {
            Value::I64(i) if i >= 0 => Ok(i as usize),
            other => bail!("LIMIT/OFFSET должен быть неотрицательным целым, получено {other:?}"),
        }
    };
    Ok(match clause {
        None => (None, 0),
        Some(sa::LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                bail!("LIMIT BY не поддерживается");
            }
            (
                limit.as_ref().map(as_usize).transpose()?,
                offset
                    .as_ref()
                    .map(|o| as_usize(&o.value))
                    .transpose()?
                    .unwrap_or(0),
            )
        }
        Some(sa::LimitClause::OffsetCommaLimit { offset, limit }) => {
            (Some(as_usize(limit)?), as_usize(offset)?)
        }
    })
}

// --- связывание выражений ---------------------------------------------------

fn bind(e: &sa::Expr, schema: &Schema) -> Result<Expr> {
    use sa::Expr as E;
    Ok(match e {
        E::Identifier(id) => Expr::Col(
            schema
                .index_of(&id.value)
                .ok_or_else(|| anyhow!("нет колонки '{}'", id.value))?,
        ),
        // `t.col` — префикс таблицы игнорируем, источник всё равно один.
        E::CompoundIdentifier(parts) => {
            let last = parts.last().ok_or_else(|| anyhow!("пустое имя"))?;
            Expr::Col(
                schema
                    .index_of(&last.value)
                    .ok_or_else(|| anyhow!("нет колонки '{}'", last.value))?,
            )
        }
        E::Value(_) => Expr::Lit(literal(e, None)?),
        E::Nested(inner) => bind(inner, schema)?,
        E::UnaryOp { op, expr } => match op {
            sa::UnaryOperator::Not => Expr::Unary {
                op: UnOp::Not,
                e: Box::new(bind(expr, schema)?),
            },
            sa::UnaryOperator::Minus => match bind(expr, schema)? {
                Expr::Lit(Value::I64(i)) => Expr::Lit(Value::I64(-i)),
                Expr::Lit(Value::F64(f)) => Expr::Lit(Value::F64(-f)),
                other => Expr::Unary {
                    op: UnOp::Neg,
                    e: Box::new(other),
                },
            },
            sa::UnaryOperator::Plus => bind(expr, schema)?,
            other => bail!("унарный оператор не поддерживается: {other}"),
        },
        E::BinaryOp { left, op, right } => Expr::Binary {
            op: map_binop(op)?,
            l: Box::new(bind(left, schema)?),
            r: Box::new(bind(right, schema)?),
        },
        E::IsNull(_) | E::IsNotNull(_) => {
            bail!("IS [NOT] NULL не поддерживается: NULL-значений в veldb нет")
        }
        E::Like {
            negated,
            expr,
            pattern,
            ..
        }
        | E::ILike {
            negated,
            expr,
            pattern,
            ..
        } => {
            let case_insensitive = matches!(e, E::ILike { .. });
            let pat = match literal(pattern, None)? {
                Value::Str(s) => s,
                other => bail!("шаблон LIKE должен быть строкой, получено {other:?}"),
            };
            Expr::Like {
                e: Box::new(bind(expr, schema)?),
                pattern: pat,
                negated: *negated,
                case_insensitive,
            }
        }
        E::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            e: Box::new(bind(expr, schema)?),
            list: list
                .iter()
                .map(|x| literal(x, None))
                .collect::<Result<_>>()?,
            negated: *negated,
        },
        E::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            e: Box::new(bind(expr, schema)?),
            low: Box::new(bind(low, schema)?),
            high: Box::new(bind(high, schema)?),
            negated: *negated,
        },
        E::Cast {
            expr, data_type, ..
        } => Expr::Cast {
            e: Box::new(bind(expr, schema)?),
            ty: map_type(data_type)?,
        },
        E::Extract { field, expr, .. } => {
            let func = match field {
                sa::DateTimeField::Hour => ScalarFunc::ExtractHour,
                sa::DateTimeField::Minute => ScalarFunc::ExtractMinute,
                sa::DateTimeField::Day => ScalarFunc::ExtractDay,
                sa::DateTimeField::Month => ScalarFunc::ExtractMonth,
                sa::DateTimeField::Year => ScalarFunc::ExtractYear,
                other => bail!("EXTRACT({other}) не поддерживается"),
            };
            Expr::Func {
                func,
                args: vec![bind(expr, schema)?],
            }
        }
        // Парсер выделяет SUBSTRING в отдельный узел, до `Function` дело не доходит.
        E::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let mut args = vec![bind(expr, schema)?];
            args.push(match substring_from {
                Some(e) => bind(e, schema)?,
                None => Expr::lit_i(1),
            });
            if let Some(e) = substring_for {
                args.push(bind(e, schema)?);
            }
            Expr::Func {
                func: ScalarFunc::Substring,
                args,
            }
        }
        // CASE сводим к вложенным `if`: отдельного узла в исполнителе он не заслуживает.
        E::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let mut out = match else_result {
                Some(e) => bind(e, schema)?,
                // Без ELSE и без NULL остаётся пустая строка / ноль — тип возьмётся
                // из ветки THEN при вычислении.
                None => Expr::Lit(Value::Str(String::new())),
            };
            for w in conditions.iter().rev() {
                let cond = match operand {
                    // `CASE x WHEN v THEN ...` — сравнение с операндом.
                    Some(op) => Expr::Binary {
                        op: BinOp::Eq,
                        l: Box::new(bind(op, schema)?),
                        r: Box::new(bind(&w.condition, schema)?),
                    },
                    None => bind(&w.condition, schema)?,
                };
                out = Expr::Func {
                    func: ScalarFunc::If,
                    args: vec![cond, bind(&w.result, schema)?, out],
                };
            }
            out
        }
        E::Function(f) => bind_function(f, schema)?,
        other => bail!("выражение не поддерживается: {other}"),
    })
}

fn map_binop(op: &sa::BinaryOperator) -> Result<BinOp> {
    use sa::BinaryOperator as B;
    Ok(match op {
        B::Plus => BinOp::Add,
        B::Minus => BinOp::Sub,
        B::Multiply => BinOp::Mul,
        B::Divide => BinOp::Div,
        B::Modulo => BinOp::Mod,
        B::Eq => BinOp::Eq,
        B::NotEq => BinOp::NotEq,
        B::Lt => BinOp::Lt,
        B::LtEq => BinOp::LtEq,
        B::Gt => BinOp::Gt,
        B::GtEq => BinOp::GtEq,
        B::And => BinOp::And,
        B::Or => BinOp::Or,
        other => bail!("оператор не поддерживается: {other}"),
    })
}

fn bind_function(f: &sa::Function, schema: &Schema) -> Result<Expr> {
    let name = ident_of(&f.name)?;
    let lname = name.to_ascii_lowercase();
    if f.over.is_some() {
        bail!("оконные функции не поддерживаются");
    }

    let (args, distinct) = match &f.args {
        sa::FunctionArguments::List(list) => (
            list.args.as_slice(),
            matches!(
                list.duplicate_treatment,
                Some(sa::DuplicateTreatment::Distinct)
            ),
        ),
        sa::FunctionArguments::None => (&[][..], false),
        sa::FunctionArguments::Subquery(_) => bail!("подзапрос в аргументах функции"),
    };
    let wildcard = args
        .iter()
        .any(|a| matches!(a, sa::FunctionArg::Unnamed(sa::FunctionArgExpr::Wildcard)));

    let plain: Vec<&sa::Expr> = args
        .iter()
        .filter_map(|a| match a {
            sa::FunctionArg::Unnamed(sa::FunctionArgExpr::Expr(e)) => Some(e),
            sa::FunctionArg::Named {
                arg: sa::FunctionArgExpr::Expr(e),
                ..
            } => Some(e),
            _ => None,
        })
        .collect();

    // Агрегаты.
    let agg = match lname.as_str() {
        "count" => Some(AggFunc::Count),
        "sum" => Some(AggFunc::Sum),
        "avg" | "mean" => Some(AggFunc::Avg),
        "min" => Some(AggFunc::Min),
        "max" => Some(AggFunc::Max),
        // `uniq` в ClickHouse приблизительный, у нас точный: медленнее, но воспроизводимый.
        "uniq" | "count_distinct" => Some(AggFunc::Count),
        _ => None,
    };
    if let Some(func) = agg {
        let distinct = distinct || lname == "uniq" || lname == "count_distinct";
        if wildcard || plain.is_empty() {
            if func != AggFunc::Count {
                bail!("{name}(*) не имеет смысла");
            }
            return Ok(Expr::Agg {
                func,
                arg: None,
                distinct: false,
            });
        }
        if plain.len() != 1 {
            bail!("{name} принимает один аргумент");
        }
        return Ok(Expr::Agg {
            func,
            arg: Some(Box::new(bind(plain[0], schema)?)),
            distinct,
        });
    }

    // Векторные метрики: `l2_distance(col, '[...]')`.
    if let Some(metric) = Metric::from_name(&lname) {
        if plain.len() != 2 {
            bail!("{name} принимает (векторная_колонка, вектор_запроса)");
        }
        let col = match bind(plain[0], schema)? {
            Expr::Col(i) => i,
            _ => bail!("первый аргумент {name} должен быть векторной колонкой"),
        };
        let dim = match schema.fields[col].ty {
            DataType::Vector(d) => d,
            other => bail!(
                "колонка '{}' имеет тип {}, а не VECTOR",
                schema.fields[col].name,
                other.name()
            ),
        };
        let query = vector_literal(plain[1])?;
        if query.len() != dim {
            bail!(
                "вектор запроса длины {}, колонка VECTOR({dim})",
                query.len()
            );
        }
        return Ok(Expr::Distance { metric, col, query });
    }

    let func = ScalarFunc::from_name(&lname)
        .ok_or_else(|| anyhow!("функция '{name}' не поддерживается"))?;
    let bound: Vec<Expr> = plain
        .iter()
        .map(|e| bind(e, schema))
        .collect::<Result<_>>()?;
    let arity_ok = match func {
        ScalarFunc::Substring => (2..=3).contains(&bound.len()),
        ScalarFunc::If => bound.len() == 3,
        ScalarFunc::Coalesce => !bound.is_empty(),
        ScalarFunc::Round => (1..=2).contains(&bound.len()),
        ScalarFunc::DateTrunc => bound.len() == 2,
        _ => bound.len() == 1,
    };
    if !arity_ok {
        bail!("{name}: неверное число аргументов ({})", bound.len());
    }
    Ok(Expr::Func { func, args: bound })
}
