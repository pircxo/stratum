//! Recursive-descent parser over the token stream from `lexer`, producing
//! a [`SelectStmt`]. Each grammar rule in the module doc comment on
//! `query::ast` has a matching `parse_*` method here — that mapping is
//! intentional, so the grammar and the code implementing it never drift
//! apart silently. Operator precedence (`NOT` > `AND` > `OR`) falls out of
//! which rule calls which, the standard recursive-descent way.

use std::fmt;

use super::ast::{
    AggFunc, CompareOp, Expr, Literal, OrderBy, OrderTarget, SelectColumns, SelectItem, SelectStmt,
};
use super::lexer::{LexError, Lexer, Token};

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError(format!("lex error: {}", e.0))
    }
}

pub fn parse(sql: &str) -> Result<SelectStmt, ParseError> {
    let tokens = Lexer::tokenize(sql)?;
    Parser::new(tokens).parse_select()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        tok
    }

    /// Consumes the next token if it is `tok`.
    fn eat(&mut self, tok: &Token) -> bool {
        if self.peek() == tok {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(ParseError(format!(
                "expected {expected:?}, found {:?}",
                self.peek()
            )))
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Token::Ident(s) => Ok(s),
            other => Err(ParseError(format!("expected identifier, found {other:?}"))),
        }
    }

    fn parse_select(&mut self) -> Result<SelectStmt, ParseError> {
        self.expect(&Token::Select)?;
        let columns = self.parse_select_columns()?;
        self.expect(&Token::From)?;
        let table = self.expect_ident()?;

        let filter = if self.eat(&Token::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat(&Token::Group) {
            self.expect(&Token::By)?;
            group_by.push(self.expect_ident()?);
            while self.eat(&Token::Comma) {
                group_by.push(self.expect_ident()?);
            }
        }

        let mut order_by = Vec::new();
        if self.eat(&Token::Order) {
            self.expect(&Token::By)?;
            order_by.push(self.parse_order_key()?);
            while self.eat(&Token::Comma) {
                order_by.push(self.parse_order_key()?);
            }
        }

        let limit = if self.eat(&Token::Limit) {
            match self.advance() {
                Token::Int(n) if n >= 0 => Some(n as usize),
                Token::Int(_) => return Err(ParseError("LIMIT must not be negative".into())),
                other => {
                    return Err(ParseError(format!(
                        "expected integer after LIMIT, found {other:?}"
                    )))
                }
            }
        } else {
            None
        };

        self.eat(&Token::Semicolon);
        if *self.peek() != Token::Eof {
            return Err(ParseError(format!(
                "unexpected trailing input starting at {:?}",
                self.peek()
            )));
        }

        Ok(SelectStmt {
            columns,
            table,
            filter,
            group_by,
            order_by,
            limit,
        })
    }

    fn parse_select_columns(&mut self) -> Result<SelectColumns, ParseError> {
        if self.eat(&Token::Star) {
            return Ok(SelectColumns::Star);
        }
        let mut items = vec![self.parse_select_item()?];
        while self.eat(&Token::Comma) {
            items.push(self.parse_select_item()?);
        }
        Ok(SelectColumns::Items(items))
    }

    fn parse_select_item(&mut self) -> Result<SelectItem, ParseError> {
        let name = self.expect_ident()?;
        let item = if *self.peek() == Token::LParen {
            let func = match name.to_ascii_uppercase().as_str() {
                "COUNT" => AggFunc::Count,
                "SUM" => AggFunc::Sum,
                "MIN" => AggFunc::Min,
                "MAX" => AggFunc::Max,
                "AVG" => AggFunc::Avg,
                _ => return Err(ParseError(format!("unknown function '{name}'"))),
            };
            self.advance();
            let arg = if self.eat(&Token::Star) {
                if func != AggFunc::Count {
                    return Err(ParseError(format!(
                        "{}(*) is not valid; only COUNT(*) is",
                        func.name().to_uppercase()
                    )));
                }
                None
            } else {
                Some(self.expect_ident()?)
            };
            self.expect(&Token::RParen)?;
            SelectItem::Aggregate {
                func,
                arg,
                alias: None,
            }
        } else {
            SelectItem::Column { name, alias: None }
        };

        if self.eat(&Token::As) {
            let alias_name = self.expect_ident()?;
            return Ok(match item {
                SelectItem::Column { name, .. } => SelectItem::Column {
                    name,
                    alias: Some(alias_name),
                },
                SelectItem::Aggregate { func, arg, .. } => SelectItem::Aggregate {
                    func,
                    arg,
                    alias: Some(alias_name),
                },
            });
        }
        Ok(item)
    }

    fn parse_order_key(&mut self) -> Result<OrderBy, ParseError> {
        let target = match self.advance() {
            Token::Ident(name) => OrderTarget::Name(name),
            Token::Int(n) if n >= 1 => OrderTarget::Position(n as usize),
            other => {
                return Err(ParseError(format!(
                    "expected a column name or 1-based position in ORDER BY, found {other:?}"
                )))
            }
        };
        let ascending = if self.eat(&Token::Desc) {
            false
        } else {
            self.eat(&Token::Asc);
            true
        };
        Ok(OrderBy { target, ascending })
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_and_expr()?;
        while self.eat(&Token::Or) {
            let right = self.parse_and_expr()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_not_expr()?;
        while self.eat(&Token::And) {
            let right = self.parse_not_expr()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not_expr(&mut self) -> Result<Expr, ParseError> {
        if self.eat(&Token::Not) {
            return Ok(Expr::Not(Box::new(self.parse_not_expr()?)));
        }
        if self.eat(&Token::LParen) {
            let inner = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            return Ok(inner);
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<Expr, ParseError> {
        // `literal op column`: normalise to `column op' literal` so
        // everything downstream sees one shape.
        if matches!(self.peek(), Token::Int(_) | Token::Str(_)) {
            let value = self.parse_literal()?;
            let op = self.parse_compare_op()?;
            let column = self.expect_ident()?;
            return Ok(Expr::compare(&column, op.flipped(), value));
        }

        let column = self.expect_ident()?;
        match self.peek() {
            Token::In => {
                self.advance();
                self.expect(&Token::LParen)?;
                let mut expr = Expr::compare(&column, CompareOp::Eq, self.parse_literal()?);
                while self.eat(&Token::Comma) {
                    let next = Expr::compare(&column, CompareOp::Eq, self.parse_literal()?);
                    expr = Expr::Or(Box::new(expr), Box::new(next));
                }
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::Between => {
                self.advance();
                let low = self.parse_literal()?;
                self.expect(&Token::And)?;
                let high = self.parse_literal()?;
                Ok(Expr::And(
                    Box::new(Expr::compare(&column, CompareOp::Ge, low)),
                    Box::new(Expr::compare(&column, CompareOp::Le, high)),
                ))
            }
            _ => {
                let op = self.parse_compare_op()?;
                let value = self.parse_literal()?;
                Ok(Expr::compare(&column, op, value))
            }
        }
    }

    fn parse_compare_op(&mut self) -> Result<CompareOp, ParseError> {
        match self.advance() {
            Token::Eq => Ok(CompareOp::Eq),
            Token::Ne => Ok(CompareOp::Ne),
            Token::Lt => Ok(CompareOp::Lt),
            Token::Le => Ok(CompareOp::Le),
            Token::Gt => Ok(CompareOp::Gt),
            Token::Ge => Ok(CompareOp::Ge),
            other => Err(ParseError(format!(
                "expected a comparison operator, found {other:?}"
            ))),
        }
    }

    fn parse_literal(&mut self) -> Result<Literal, ParseError> {
        match self.advance() {
            Token::Int(n) => Ok(Literal::Int(n)),
            Token::Str(s) => Ok(Literal::Str(s)),
            other => Err(ParseError(format!("expected a literal, found {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(col: &str, op: CompareOp, v: i64) -> Expr {
        Expr::compare(col, op, Literal::Int(v))
    }

    fn and(a: Expr, b: Expr) -> Expr {
        Expr::And(Box::new(a), Box::new(b))
    }

    fn or(a: Expr, b: Expr) -> Expr {
        Expr::Or(Box::new(a), Box::new(b))
    }

    #[test]
    fn parses_select_star() {
        let stmt = parse("SELECT * FROM patterns").unwrap();
        assert_eq!(stmt.columns, SelectColumns::Star);
        assert_eq!(stmt.table, "patterns");
        assert!(stmt.filter.is_none());
        assert!(stmt.order_by.is_empty());
        assert!(stmt.limit.is_none());
        assert!(!stmt.is_aggregate());
    }

    #[test]
    fn parses_full_statement() {
        let stmt = parse(
            "SELECT id, value FROM readings WHERE value > 100 AND id != 5 ORDER BY value DESC, id LIMIT 20",
        )
        .unwrap();
        assert_eq!(
            stmt.filter,
            Some(and(
                cmp("value", CompareOp::Gt, 100),
                cmp("id", CompareOp::Ne, 5)
            ))
        );
        assert_eq!(
            stmt.order_by,
            vec![
                OrderBy {
                    target: OrderTarget::Name("value".into()),
                    ascending: false
                },
                OrderBy {
                    target: OrderTarget::Name("id".into()),
                    ascending: true
                },
            ]
        );
        assert_eq!(stmt.limit, Some(20));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        let stmt = parse("SELECT * FROM t WHERE a = 1 OR b = 2 AND c = 3").unwrap();
        assert_eq!(
            stmt.filter.unwrap(),
            or(
                cmp("a", CompareOp::Eq, 1),
                and(cmp("b", CompareOp::Eq, 2), cmp("c", CompareOp::Eq, 3))
            )
        );
    }

    #[test]
    fn parentheses_override_precedence() {
        let stmt = parse("SELECT * FROM t WHERE (a = 1 OR b = 2) AND c = 3").unwrap();
        assert_eq!(
            stmt.filter.unwrap(),
            and(
                or(cmp("a", CompareOp::Eq, 1), cmp("b", CompareOp::Eq, 2)),
                cmp("c", CompareOp::Eq, 3)
            )
        );
    }

    #[test]
    fn not_binds_tighter_than_and() {
        let stmt = parse("SELECT * FROM t WHERE NOT a = 1 AND b = 2").unwrap();
        assert_eq!(
            stmt.filter.unwrap(),
            and(
                Expr::Not(Box::new(cmp("a", CompareOp::Eq, 1))),
                cmp("b", CompareOp::Eq, 2)
            )
        );
    }

    #[test]
    fn in_and_between_lower_to_plain_comparisons() {
        let stmt = parse("SELECT * FROM t WHERE a IN (1, 2, 3) AND b BETWEEN 10 AND 20").unwrap();
        assert_eq!(
            stmt.filter.unwrap(),
            and(
                or(
                    or(cmp("a", CompareOp::Eq, 1), cmp("a", CompareOp::Eq, 2)),
                    cmp("a", CompareOp::Eq, 3)
                ),
                and(cmp("b", CompareOp::Ge, 10), cmp("b", CompareOp::Le, 20))
            )
        );
    }

    #[test]
    fn literal_on_the_left_is_normalised() {
        let stmt = parse("SELECT * FROM t WHERE 100 < value").unwrap();
        assert_eq!(stmt.filter.unwrap(), cmp("value", CompareOp::Gt, 100));
    }

    #[test]
    fn string_literal_predicate() {
        let stmt = parse("SELECT * FROM t WHERE name = 'chestnut'").unwrap();
        assert_eq!(
            stmt.filter.unwrap(),
            Expr::compare("name", CompareOp::Eq, Literal::Str("chestnut".into()))
        );
    }

    #[test]
    fn parses_aggregates_group_by_and_aliases() {
        let stmt = parse(
            "SELECT sensor, COUNT(*) AS n, avg(value) FROM t GROUP BY sensor ORDER BY n DESC, 1",
        )
        .unwrap();
        assert!(stmt.is_aggregate());
        let SelectColumns::Items(items) = &stmt.columns else {
            panic!("expected items");
        };
        assert_eq!(
            items[1],
            SelectItem::Aggregate {
                func: AggFunc::Count,
                arg: None,
                alias: Some("n".into())
            }
        );
        assert_eq!(items[2].output_name(), "avg(value)");
        assert_eq!(stmt.group_by, vec!["sensor"]);
        assert_eq!(stmt.order_by[1].target, OrderTarget::Position(1));
    }

    #[test]
    fn sum_star_is_rejected() {
        assert!(parse("SELECT SUM(*) FROM t").is_err());
        assert!(parse("SELECT MEDIAN(x) FROM t").is_err());
    }

    #[test]
    fn default_order_direction_is_ascending() {
        let stmt = parse("SELECT * FROM t ORDER BY id").unwrap();
        assert!(stmt.order_by[0].ascending);
    }

    #[test]
    fn rejects_malformed_statements() {
        for sql in [
            "SELECT * FROM t EXTRA GARBAGE",
            "SELECT * t",
            "SELECT * FROM t WHERE (a = 1",
            "SELECT * FROM t WHERE a = 1 OR",
            "SELECT * FROM t WHERE a IN ()",
            "SELECT * FROM t ORDER BY 0",
            "SELECT * FROM t LIMIT -1",
            "SELECT * FROM t WHERE a = b",
        ] {
            assert!(parse(sql).is_err(), "should reject: {sql}");
        }
    }
}
