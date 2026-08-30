//! IR запроса: то, во что превращается AST `sqlparser` перед исполнением.
//!
//! Отдельный IR нужен потому, что AST парсера общий на все диалекты и тащит десятки
//! вариантов, которых у нас нет. Здесь остаётся ровно то, что исполнитель умеет.

use crate::column::{DataType, Value};
use crate::table::Schema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    L2,
    Cosine,
    /// Скалярное произведение со знаком минус — чтобы «меньше = ближе», как у остальных.
    NegInnerProduct,
}

impl Metric {
    pub fn from_name(s: &str) -> Option<Metric> {
        match s.to_ascii_lowercase().as_str() {
            "l2_distance" | "l2" | "euclidean_distance" => Some(Metric::L2),
            "cosine_distance" | "cosine" => Some(Metric::Cosine),
            "inner_product" | "dot_product" => Some(Metric::NegInnerProduct),
            _ => None,
        }
    }
}

/// Скалярные функции. Список растёт по факту: добавляем то, что реально
/// потребовалось запросу, а не то, что «бывает в SQL».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    Length,
    Lower,
    Upper,
    Abs,
    Floor,
    Ceil,
    Round,
    Sqrt,
    Substring,
    /// `EXTRACT(<part> FROM <unix-seconds>)` — время у нас хранится как INT.
    ExtractHour,
    ExtractMinute,
    ExtractDay,
    ExtractMonth,
    ExtractYear,
    Coalesce,
    If,
    /// `date_trunc(<единица>, <unix-секунды>)` — округление времени вниз.
    DateTrunc,
}

impl ScalarFunc {
    pub fn from_name(s: &str) -> Option<ScalarFunc> {
        use ScalarFunc::*;
        Some(match s.to_ascii_lowercase().as_str() {
            "length" | "char_length" | "octet_length" => Length,
            "lower" | "lcase" => Lower,
            "upper" | "ucase" => Upper,
            "abs" => Abs,
            "floor" => Floor,
            "ceil" | "ceiling" => Ceil,
            "round" => Round,
            "sqrt" => Sqrt,
            "substring" | "substr" => Substring,
            "to_hour" => ExtractHour,
            "to_minute" => ExtractMinute,
            "to_day" | "to_dayofmonth" => ExtractDay,
            "to_month" => ExtractMonth,
            "to_year" => ExtractYear,
            "coalesce" => Coalesce,
            "if" => If,
            "date_trunc" | "to_start_of" => DateTrunc,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Индекс колонки в схеме источника (базовой таблицы либо промежуточной после GROUP BY).
    Col(usize),
    Lit(Value),
    Binary {
        op: BinOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Unary {
        op: UnOp,
        e: Box<Expr>,
    },
    Like {
        e: Box<Expr>,
        pattern: String,
        negated: bool,
        case_insensitive: bool,
    },
    InList {
        e: Box<Expr>,
        list: Vec<Value>,
        negated: bool,
    },
    Between {
        e: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Func {
        func: ScalarFunc,
        args: Vec<Expr>,
    },
    /// Расстояние от векторной колонки до константного вектора запроса.
    Distance {
        metric: Metric,
        col: usize,
        query: Vec<f32>,
    },
    Agg {
        func: AggFunc,
        arg: Option<Box<Expr>>,
        distinct: bool,
    },
    Cast {
        e: Box<Expr>,
        ty: DataType,
    },
}

impl Expr {
    pub fn lit_i(v: i64) -> Expr {
        Expr::Lit(Value::I64(v))
    }

    /// Обходит дерево сверху вниз; `f` возвращает `true`, чтобы прекратить спуск в поддерево.
    pub fn walk(&self, f: &mut impl FnMut(&Expr) -> bool) {
        if f(self) {
            return;
        }
        match self {
            Expr::Binary { l, r, .. } => {
                l.walk(f);
                r.walk(f);
            }
            Expr::Unary { e, .. } | Expr::Cast { e, .. } | Expr::Like { e, .. } => e.walk(f),
            Expr::InList { e, .. } => e.walk(f),
            Expr::Between { e, low, high, .. } => {
                e.walk(f);
                low.walk(f);
                high.walk(f);
            }
            Expr::Func { args, .. } => args.iter().for_each(|a| a.walk(f)),
            Expr::Agg { arg, .. } => {
                if let Some(a) = arg {
                    a.walk(f)
                }
            }
            Expr::Col(_) | Expr::Lit(_) | Expr::Distance { .. } => {}
        }
    }

    pub fn contains_agg(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if matches!(e, Expr::Agg { .. }) {
                found = true;
            }
            found
        });
        found
    }

    /// Подставляет заготовленные замены за один проход сверху вниз.
    ///
    /// Одним проходом — принципиально: агрегат и ключ группировки живут в одном
    /// индексном пространстве промежуточного кадра, и две последовательные замены
    /// затирали бы результат друг друга (`count(*)`→`Col(1)`, затем `Col(1)`→`Col(0)`
    /// как ключа группировки). Совпавший узел возвращается целиком, вглубь не идём.
    pub fn substitute(&self, pats: &[(Expr, Expr)]) -> Expr {
        for (from, to) in pats {
            if self == from {
                return to.clone();
            }
        }
        let sub = |e: &Expr| Box::new(e.substitute(pats));
        match self {
            Expr::Binary { op, l, r } => Expr::Binary {
                op: *op,
                l: sub(l),
                r: sub(r),
            },
            Expr::Unary { op, e } => Expr::Unary { op: *op, e: sub(e) },
            Expr::Cast { e, ty } => Expr::Cast { e: sub(e), ty: *ty },
            Expr::Like {
                e,
                pattern,
                negated,
                case_insensitive,
            } => Expr::Like {
                e: sub(e),
                pattern: pattern.clone(),
                negated: *negated,
                case_insensitive: *case_insensitive,
            },
            Expr::InList { e, list, negated } => Expr::InList {
                e: sub(e),
                list: list.clone(),
                negated: *negated,
            },
            Expr::Between {
                e,
                low,
                high,
                negated,
            } => Expr::Between {
                e: sub(e),
                low: sub(low),
                high: sub(high),
                negated: *negated,
            },
            Expr::Func { func, args } => Expr::Func {
                func: *func,
                args: args.iter().map(|a| a.substitute(pats)).collect(),
            },
            Expr::Agg {
                func,
                arg,
                distinct,
            } => Expr::Agg {
                func: *func,
                arg: arg.as_ref().map(|a| sub(a)),
                distinct: *distinct,
            },
            other => other.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub expr: Expr,
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    pub expr: Expr,
    pub asc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub table: String,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    /// Фильтр по результату агрегации. Считается после группировки, до сортировки.
    pub having: Option<Expr>,
    pub projection: Vec<Projection>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<usize>,
    pub offset: usize,
}

impl Select {
    pub fn is_aggregate(&self) -> bool {
        !self.group_by.is_empty()
            || self.projection.iter().any(|p| p.expr.contains_agg())
            || self.order_by.iter().any(|o| o.expr.contains_agg())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        schema: Schema,
        if_not_exists: bool,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
    },
    Select(Box<Select>),
    ShowTables,
    Describe(String),
}
