//! SQL frontend.
//!
//! Today this is a thin wrapper around `sqlparser-rs`: tokenize and parse
//! a SQL string into a vector of [`Statement`]s. No binding, no planning —
//! callers get the AST and decide what to do with it. The binder and
//! lowering passes will land as siblings here.

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

pub use sqlparser::ast::Statement;
pub use sqlparser::parser::ParserError;

pub fn parse(sql: &str) -> Result<Vec<Statement>, ParserError> {
    Parser::parse_sql(&PostgreSqlDialect {}, sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_select_one() {
        let stmts = parse("SELECT 1").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(matches!(stmts[0], Statement::Query(_)));
    }

    #[test]
    fn parses_multiple_statements() {
        let stmts = parse("SELECT 1; SELECT 2").unwrap();
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("NOT SQL AT ALL ###").is_err());
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(parse("SELECT 'oops").is_err());
    }
}
