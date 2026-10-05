//! The query AST. Stratum understands one statement shape:
//!
//! ```text
//! select   := SELECT (* | item (, item)*) FROM ident
//!             (WHERE expr)?
//!             (GROUP BY ident (, ident)*)?
//!             (ORDER BY key (ASC | DESC)? (, key (ASC | DESC)?)*)?
//!             (LIMIT int)? (;)?
//! item     := ident (AS ident)?
//!           | agg '(' (* | ident) ')' (AS ident)?
//! agg      := COUNT | SUM | MIN | MAX | AVG
//! key      := ident | int                    -- an int is a 1-based output position
//! expr     := and_expr (OR and_expr)*
//! and_expr := not_expr (AND not_expr)*
//! not_expr := NOT not_expr | '(' expr ')' | predicate
//! predicate:= ident cmp literal | literal cmp ident
//!           | ident IN '(' literal (, literal)* ')'
//!           | ident BETWEEN literal AND literal
//! ```
//!
//! `IN` and `BETWEEN` are sugar: the parser lowers them to `OR`/`AND`
//! trees of plain comparisons, so neither the pruner nor the executor
//! needs to know they exist. No joins, no subqueries, no arithmetic
//! expressions, no `HAVING` — see ARCHITECTURE.md for where the line is
//! and why.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    /// The operator that means the same thing with operands swapped:
    /// `5 < x` is `x > 5`.
    pub fn flipped(self) -> Self {
        match self {
            CompareOp::Eq => CompareOp::Eq,
            CompareOp::Ne => CompareOp::Ne,
            CompareOp::Lt => CompareOp::Gt,
            CompareOp::Le => CompareOp::Ge,
            CompareOp::Gt => CompareOp::Lt,
            CompareOp::Ge => CompareOp::Le,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Str(String),
}

/// A boolean filter expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Compare {
        column: String,
        op: CompareOp,
        value: Literal,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

impl Expr {
    pub fn compare(column: &str, op: CompareOp, value: Literal) -> Self {
        Expr::Compare {
            column: column.to_string(),
            op,
            value,
        }
    }

    /// Every column the expression reads, each once, in first-seen order.
    pub fn columns(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expr::Compare { column, .. } => {
                if !out.contains(&column.as_str()) {
                    out.push(column);
                }
            }
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.collect_columns(out);
                b.collect_columns(out);
            }
            Expr::Not(e) => e.collect_columns(out),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::Avg => "avg",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Column {
        name: String,
        alias: Option<String>,
    },
    Aggregate {
        func: AggFunc,
        /// `None` means `*` — only valid for `COUNT`.
        arg: Option<String>,
        alias: Option<String>,
    },
}

impl SelectItem {
    /// The name this item gets in the result header.
    pub fn output_name(&self) -> String {
        match self {
            SelectItem::Column { name, alias } => alias.clone().unwrap_or_else(|| name.clone()),
            SelectItem::Aggregate { func, arg, alias } => alias
                .clone()
                .unwrap_or_else(|| format!("{}({})", func.name(), arg.as_deref().unwrap_or("*"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectColumns {
    Star,
    Items(Vec<SelectItem>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum OrderTarget {
    /// A column or output alias.
    Name(String),
    /// 1-based position in the select list.
    Position(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub target: OrderTarget,
    pub ascending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub columns: SelectColumns,
    pub table: String,
    pub filter: Option<Expr>,
    pub group_by: Vec<String>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
}

impl SelectStmt {
    pub fn is_aggregate(&self) -> bool {
        !self.group_by.is_empty()
            || matches!(&self.columns, SelectColumns::Items(items)
                if items.iter().any(|i| matches!(i, SelectItem::Aggregate { .. })))
    }
}
