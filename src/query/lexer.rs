//! A hand-written tokenizer — no parser-generator, no regex. Keywords are
//! matched case-insensitively (`SELECT`, `select`, and `Select` are the
//! same token); identifiers and string literal *contents* keep whatever
//! case was typed. Aggregate names (`COUNT`, `SUM`, ...) are *not*
//! keywords — they lex as identifiers and the parser recognises them by
//! the `(` that follows, so a column may still be called `count`.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Select,
    From,
    Where,
    And,
    Or,
    Not,
    In,
    Between,
    As,
    Group,
    Order,
    By,
    Asc,
    Desc,
    Limit,

    Star,
    Comma,
    Semicolon,
    LParen,
    RParen,

    Ident(String),
    Int(i64),
    Str(String),

    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,

    Eof,
}

pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LexError(pub String);

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            chars: input.chars().peekable(),
        }
    }

    pub fn tokenize(input: &'a str) -> Result<Vec<Token>, LexError> {
        let mut lexer = Lexer::new(input);
        let mut tokens = Vec::new();
        loop {
            let tok = lexer.next_token()?;
            let done = tok == Token::Eof;
            tokens.push(tok);
            if done {
                break;
            }
        }
        Ok(tokens)
    }

    fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_whitespace();

        let Some(&c) = self.chars.peek() else {
            return Ok(Token::Eof);
        };

        match c {
            '*' => {
                self.chars.next();
                Ok(Token::Star)
            }
            ',' => {
                self.chars.next();
                Ok(Token::Comma)
            }
            ';' => {
                self.chars.next();
                Ok(Token::Semicolon)
            }
            '(' => {
                self.chars.next();
                Ok(Token::LParen)
            }
            ')' => {
                self.chars.next();
                Ok(Token::RParen)
            }
            '=' => {
                self.chars.next();
                Ok(Token::Eq)
            }
            '<' => {
                self.chars.next();
                match self.chars.peek() {
                    Some('=') => {
                        self.chars.next();
                        Ok(Token::Le)
                    }
                    Some('>') => {
                        self.chars.next();
                        Ok(Token::Ne)
                    }
                    _ => Ok(Token::Lt),
                }
            }
            '>' => {
                self.chars.next();
                match self.chars.peek() {
                    Some('=') => {
                        self.chars.next();
                        Ok(Token::Ge)
                    }
                    _ => Ok(Token::Gt),
                }
            }
            '!' => {
                self.chars.next();
                match self.chars.peek() {
                    Some('=') => {
                        self.chars.next();
                        Ok(Token::Ne)
                    }
                    other => Err(LexError(format!(
                        "expected '=' after '!', found {:?}",
                        other
                    ))),
                }
            }
            '\'' => self.read_string(),
            '0'..='9' | '-' => self.read_number(),
            c if c.is_alphabetic() || c == '_' => self.read_ident_or_keyword(),
            other => Err(LexError(format!("unexpected character: {other:?}"))),
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.chars.next();
        }
    }

    fn read_string(&mut self) -> Result<Token, LexError> {
        self.chars.next(); // opening quote
        let mut s = String::new();
        loop {
            match self.chars.next() {
                Some('\'') if self.chars.peek() == Some(&'\'') => {
                    self.chars.next();
                    s.push('\'');
                }
                Some('\'') => return Ok(Token::Str(s)),
                Some(c) => s.push(c),
                None => return Err(LexError("unterminated string literal".to_string())),
            }
        }
    }

    fn read_number(&mut self) -> Result<Token, LexError> {
        let mut s = String::new();
        if self.chars.peek() == Some(&'-') {
            s.push('-');
            self.chars.next();
        }
        let mut saw_digit = false;
        while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
            saw_digit = true;
            s.push(self.chars.next().unwrap());
        }
        if !saw_digit {
            return Err(LexError(format!("invalid number literal: {s:?}")));
        }
        s.parse::<i64>()
            .map(Token::Int)
            .map_err(|e| LexError(format!("invalid integer literal {s:?}: {e}")))
    }

    fn read_ident_or_keyword(&mut self) -> Result<Token, LexError> {
        let mut s = String::new();
        while matches!(self.chars.peek(), Some(c) if c.is_alphanumeric() || *c == '_') {
            s.push(self.chars.next().unwrap());
        }
        Ok(keyword(&s).unwrap_or(Token::Ident(s)))
    }
}

fn keyword(word: &str) -> Option<Token> {
    Some(match word.to_ascii_uppercase().as_str() {
        "SELECT" => Token::Select,
        "FROM" => Token::From,
        "WHERE" => Token::Where,
        "AND" => Token::And,
        "OR" => Token::Or,
        "NOT" => Token::Not,
        "IN" => Token::In,
        "BETWEEN" => Token::Between,
        "AS" => Token::As,
        "GROUP" => Token::Group,
        "ORDER" => Token::Order,
        "BY" => Token::By,
        "ASC" => Token::Asc,
        "DESC" => Token::Desc,
        "LIMIT" => Token::Limit,
        _ => return None,
    })
}

/// Whether `word` lexes as a keyword (case-insensitively) — such a name
/// could never be referenced as a column, so the writer rejects it.
pub fn is_keyword(word: &str) -> bool {
    keyword(word).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_a_full_statement() {
        let tokens = Lexer::tokenize(
            "SELECT id, name FROM users WHERE age >= 18 AND country = 'GE' ORDER BY id DESC LIMIT 10;",
        )
        .unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Select,
                Token::Ident("id".into()),
                Token::Comma,
                Token::Ident("name".into()),
                Token::From,
                Token::Ident("users".into()),
                Token::Where,
                Token::Ident("age".into()),
                Token::Ge,
                Token::Int(18),
                Token::And,
                Token::Ident("country".into()),
                Token::Eq,
                Token::Str("GE".into()),
                Token::Order,
                Token::By,
                Token::Ident("id".into()),
                Token::Desc,
                Token::Limit,
                Token::Int(10),
                Token::Semicolon,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let tokens = Lexer::tokenize("select * from t").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Select,
                Token::Star,
                Token::From,
                Token::Ident("t".into()),
                Token::Eof
            ]
        );
    }

    #[test]
    fn negative_numbers_and_not_equal_variants() {
        assert_eq!(
            Lexer::tokenize("-5").unwrap(),
            vec![Token::Int(-5), Token::Eof]
        );
        assert_eq!(Lexer::tokenize("<>").unwrap(), vec![Token::Ne, Token::Eof]);
        assert_eq!(Lexer::tokenize("!=").unwrap(), vec![Token::Ne, Token::Eof]);
    }

    #[test]
    fn doubled_quote_escapes_a_quote() {
        assert_eq!(
            Lexer::tokenize("'it''s'").unwrap(),
            vec![Token::Str("it's".into()), Token::Eof]
        );
    }

    #[test]
    fn boolean_keywords_and_parens() {
        assert_eq!(
            Lexer::tokenize("NOT (a IN (1)) or b BETWEEN 1 AND 2").unwrap(),
            vec![
                Token::Not,
                Token::LParen,
                Token::Ident("a".into()),
                Token::In,
                Token::LParen,
                Token::Int(1),
                Token::RParen,
                Token::RParen,
                Token::Or,
                Token::Ident("b".into()),
                Token::Between,
                Token::Int(1),
                Token::And,
                Token::Int(2),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn lone_minus_is_an_error() {
        assert!(Lexer::tokenize("a - b").is_err());
    }

    #[test]
    fn unterminated_string_is_an_error() {
        assert!(Lexer::tokenize("'oops").is_err());
    }

    #[test]
    fn doubled_quotes_escape_sql_strings() {
        assert_eq!(
            Lexer::tokenize("'O''Brien'").unwrap(),
            vec![Token::Str("O'Brien".into()), Token::Eof]
        );
        assert_eq!(
            Lexer::tokenize("''").unwrap(),
            vec![Token::Str(String::new()), Token::Eof]
        );
    }
}
