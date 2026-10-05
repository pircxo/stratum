//! The (deliberately small) query AST. Stratum understands exactly one
//! statement shape:
//!
//! ```text
//! SELECT (* | col (, col)*)
//! FROM table
//! (WHERE condition (AND condition)*)?
//! (ORDER BY col (ASC | DESC)?)?
//! (LIMIT n)?
//! (;)?
//! ```
//!
//! No `OR`, no parentheses, no joins, no aggregates, no subqueries. That's
//! a real scope limit, not an oversight — see ARCHITECTURE.md for why
//! this is where v1 draws the line and what a v2 would add first.

#[derive(Debug, Clone, PartialEq)]
pub enum SelectColumns {
    Star,
    List(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub column: String,
    pub op: CompareOp,
    pub value: Literal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub column: String,
    pub ascending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub columns: SelectColumns,
    pub table: String,
    /// Every predicate is implicitly AND-ed together — there is no `OR`.
    pub predicates: Vec<Predicate>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
}
