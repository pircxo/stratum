//! Recursive-descent parser over the token stream from `lexer`, producing
//! a [`SelectStmt`]. Each grammar rule in the module doc comment on
//! `query::ast` has a matching `parse_*` method here — that mapping is
//! intentional, so the grammar and the code implementing it never drift
//! apart silently.

use std::fmt;

use super::ast::{CompareOp, Literal, OrderBy, Predicate, SelectColumns, SelectStmt};
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

    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        if self.peek() == expected {
            self.advance();
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

    fn expect_int(&mut self) -> Result<i64, ParseError> {
        match self.advance() {
            Token::Int(n) => Ok(n),
            other => Err(ParseError(format!("expected integer, found {other:?}"))),
        }
    }

    fn parse_select(&mut self) -> Result<SelectStmt, ParseError> {
        self.expect(&Token::Select)?;
        let columns = self.parse_select_columns()?;
        self.expect(&Token::From)?;
        let table = self.expect_ident()?;

        let predicates = if *self.peek() == Token::Where {
            self.advance();
            self.parse_predicate_list()?
        } else {
            Vec::new()
        };

        let order_by = if *self.peek() == Token::Order {
            self.advance();
            self.expect(&Token::By)?;
            let column = self.expect_ident()?;
            let ascending = match self.peek() {
                Token::Asc => {
                    self.advance();
                    true
                }
                Token::Desc => {
                    self.advance();
                    false
                }
                _ => true,
            };
            Some(OrderBy { column, ascending })
        } else {
            None
        };

        let limit = if *self.peek() == Token::Limit {
            self.advance();
            let n = self.expect_int()?;
            if n < 0 {
                return Err(ParseError("LIMIT must not be negative".to_string()));
            }
            Some(n as usize)
        } else {
            None
        };

        if *self.peek() == Token::Semicolon {
            self.advance();
        }
        if *self.peek() != Token::Eof {
            return Err(ParseError(format!(
                "unexpected trailing input starting at {:?}",
                self.peek()
            )));
        }

        Ok(SelectStmt {
            columns,
            table,
            predicates,
            order_by,
            limit,
        })
    }

    fn parse_select_columns(&mut self) -> Result<SelectColumns, ParseError> {
        if *self.peek() == Token::Star {
            self.advance();
            return Ok(SelectColumns::Star);
        }
        let mut cols = vec![self.expect_ident()?];
        while *self.peek() == Token::Comma {
            self.advance();
            cols.push(self.expect_ident()?);
        }
        Ok(SelectColumns::List(cols))
    }

    fn parse_predicate_list(&mut self) -> Result<Vec<Predicate>, ParseError> {
        let mut preds = vec![self.parse_predicate()?];
        while *self.peek() == Token::And {
            self.advance();
            preds.push(self.parse_predicate()?);
        }
        Ok(preds)
    }

    fn parse_predicate(&mut self) -> Result<Predicate, ParseError> {
        let column = self.expect_ident()?;
        let op = self.parse_compare_op()?;
        let value = self.parse_literal()?;
        Ok(Predicate { column, op, value })
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

    #[test]
    fn parses_select_star() {
        let stmt = parse("SELECT * FROM patterns").unwrap();
        assert_eq!(stmt.columns, SelectColumns::Star);
        assert_eq!(stmt.table, "patterns");
        assert!(stmt.predicates.is_empty());
        assert!(stmt.order_by.is_none());
        assert!(stmt.limit.is_none());
    }

    #[test]
    fn parses_full_statement() {
        let stmt = parse(
            "SELECT id, value FROM readings WHERE value > 100 AND id != 5 ORDER BY value DESC LIMIT 20",
        )
        .unwrap();
        assert_eq!(
            stmt.columns,
            SelectColumns::List(vec!["id".to_string(), "value".to_string()])
        );
        assert_eq!(stmt.predicates.len(), 2);
        assert_eq!(stmt.predicates[0].op, CompareOp::Gt);
        assert_eq!(stmt.predicates[1].op, CompareOp::Ne);
        assert_eq!(
            stmt.order_by,
            Some(OrderBy {
                column: "value".to_string(),
                ascending: false
            })
        );
        assert_eq!(stmt.limit, Some(20));
    }

    #[test]
    fn string_literal_predicate() {
        let stmt = parse("SELECT * FROM t WHERE name = 'chestnut'").unwrap();
        assert_eq!(
            stmt.predicates[0].value,
            Literal::Str("chestnut".to_string())
        );
    }

    #[test]
    fn default_order_direction_is_ascending() {
        let stmt = parse("SELECT * FROM t ORDER BY id").unwrap();
        assert!(stmt.order_by.unwrap().ascending);
    }

    #[test]
    fn rejects_garbage_after_a_valid_statement() {
        assert!(parse("SELECT * FROM t EXTRA GARBAGE").is_err());
    }

    #[test]
    fn rejects_or_since_it_is_out_of_scope() {
        // No OR support in v1 — `OR` lexes as an identifier, not a
        // keyword, so this fails to parse as a predicate list rather
        // than silently doing something unintended.
        assert!(parse("SELECT * FROM t WHERE a = 1 OR b = 2").is_err());
    }

    #[test]
    fn missing_from_is_an_error() {
        assert!(parse("SELECT * t").is_err());
    }
}
