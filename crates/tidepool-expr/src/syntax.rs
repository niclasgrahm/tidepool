//! Expressions as text: parsing, and what an expression refers to.
//!
//! This is the half of the expression crate that needs nothing from the
//! model: it says *which* names an expression mentions and *how* (inside an
//! aggregate or not), and leaves deciding whether those names exist to the
//! model compiler. Type checking and evaluation come later (M3) and build on
//! the same parsed tree.
//!
//! Expressions are standard SQL (Ossie's `OSSIE_SQL_2026` is an ANSI SQL:2003
//! subset), parsed with sqlparser's generic dialect.

use std::ops::ControlFlow;

use sqlparser::ast::{
    DuplicateTreatment, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident,
    ObjectNamePart, Visit, Visitor,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

pub use sqlparser::ast::Expr as Ast;

/// Aggregate functions Tidepool computes. Anything else that is called is a
/// row-level function.
pub const AGGREGATES: [&str; 5] = ["sum", "count", "avg", "min", "max"];

/// A parse failure, as one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse one SQL expression. The whole text must be the expression: trailing
/// tokens are an error rather than silently ignored.
///
/// # Errors
/// The text isn't a single valid SQL expression.
pub fn parse(text: &str) -> Result<Expr, ParseError> {
    let dialect = GenericDialect {};
    let mut parser = Parser::new(&dialect)
        .try_with_sql(text)
        .map_err(|err| ParseError(clean(&err.to_string())))?;
    let expr = parser
        .parse_expr()
        .map_err(|err| ParseError(clean(&err.to_string())))?;
    match parser.peek_token().token {
        Token::EOF => Ok(expr),
        other => Err(ParseError(format!(
            "unexpected `{other}` after the end of the expression"
        ))),
    }
}

/// sqlparser prefixes messages with "sql parser error: "; we say where it came
/// from ourselves.
fn clean(message: &str) -> String {
    message.trim_start_matches("sql parser error: ").to_string()
}

/// A name as written: unquoted names match case-insensitively (as in SQL and
/// Ossie), quoted ones exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub value: String,
    pub quoted: bool,
}

impl From<&Ident> for Name {
    fn from(ident: &Ident) -> Self {
        Self {
            value: ident.value.clone(),
            quoted: ident.quote_style.is_some(),
        }
    }
}

impl Name {
    /// Whether this name refers to `candidate`.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        if self.quoted {
            self.value == candidate
        } else {
            self.value.eq_ignore_ascii_case(candidate)
        }
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.value)
    }
}

/// A reference to a column, field or metric: `amount`, `orders.amount`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub parts: Vec<Name>,
    /// Inside an aggregate's argument or `FILTER` clause.
    pub in_aggregate: bool,
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<&str> = self.parts.iter().map(|p| p.value.as_str()).collect();
        f.write_str(&parts.join("."))
    }
}

/// An aggregate call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Aggregate {
    /// Lowercase: `sum`, `count`, …
    pub function: String,
    pub distinct: bool,
    /// `COUNT(*)`.
    pub star: bool,
    /// Has a `FILTER (WHERE …)` clause.
    pub filtered: bool,
}

/// What an expression mentions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Analysis {
    pub references: Vec<Reference>,
    pub aggregates: Vec<Aggregate>,
    /// Lowercase names of non-aggregate functions called.
    pub functions: Vec<String>,
    /// An aggregate inside another aggregate's argument.
    pub nested_aggregate: bool,
    /// Uses `OVER (…)`.
    pub window: bool,
    /// Contains a subquery.
    pub subquery: bool,
}

impl Analysis {
    /// References outside any aggregate: bare columns in a metric.
    pub fn bare_references(&self) -> impl Iterator<Item = &Reference> {
        self.references.iter().filter(|r| !r.in_aggregate)
    }
}

/// What `expr` refers to and how.
#[must_use]
pub fn analyze(expr: &Expr) -> Analysis {
    let mut collector = Collector {
        analysis: Analysis::default(),
        depth: 0,
        aggregate_stack: Vec::new(),
    };
    let _ = expr.visit(&mut collector);
    collector.analysis
}

struct Collector {
    analysis: Analysis,
    /// How many aggregates we are inside.
    depth: usize,
    /// For each function we entered, whether it was an aggregate, so the
    /// post-visit knows whether to leave one.
    aggregate_stack: Vec<bool>,
}

impl Visitor for Collector {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        match expr {
            Expr::Identifier(ident) => self.analysis.references.push(Reference {
                parts: vec![Name::from(ident)],
                in_aggregate: self.depth > 0,
            }),
            Expr::CompoundIdentifier(idents) => self.analysis.references.push(Reference {
                parts: idents.iter().map(Name::from).collect(),
                in_aggregate: self.depth > 0,
            }),
            Expr::Function(function) => {
                let name = function
                    .name
                    .0
                    .last()
                    .map(|part| match part {
                        ObjectNamePart::Identifier(ident) => ident.value.to_lowercase(),
                        ObjectNamePart::Function(f) => f.name.value.to_lowercase(),
                    })
                    .unwrap_or_default();
                let is_aggregate = AGGREGATES.contains(&name.as_str()) && function.over.is_none();
                if function.over.is_some() {
                    self.analysis.window = true;
                }
                if is_aggregate {
                    if self.depth > 0 {
                        self.analysis.nested_aggregate = true;
                    }
                    let (distinct, star) = match &function.args {
                        FunctionArguments::List(list) => (
                            matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct)),
                            list.args.iter().any(|arg| {
                                matches!(
                                    arg,
                                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                                        | FunctionArg::Named {
                                            arg: FunctionArgExpr::Wildcard,
                                            ..
                                        }
                                )
                            }),
                        ),
                        _ => (false, false),
                    };
                    self.analysis.aggregates.push(Aggregate {
                        function: name,
                        distinct,
                        star,
                        filtered: function.filter.is_some(),
                    });
                    self.depth += 1;
                } else {
                    self.analysis.functions.push(name);
                }
                self.aggregate_stack.push(is_aggregate);
            }
            Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {
                self.analysis.subquery = true;
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if matches!(expr, Expr::Function(_)) && self.aggregate_stack.pop() == Some(true) {
            self.depth -= 1;
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(text: &str) -> Analysis {
        match parse(text) {
            Ok(expr) => analyze(&expr),
            Err(err) => panic!("{text}: {err}"),
        }
    }

    fn refs(analysis: &Analysis) -> Vec<(String, bool)> {
        analysis
            .references
            .iter()
            .map(|r| (r.to_string(), r.in_aggregate))
            .collect()
    }

    #[test]
    fn references_inside_and_outside_aggregates() {
        let a = analysis("SUM(orders.amount) / COUNT(DISTINCT orders.order_id) + bonus");
        assert_eq!(
            refs(&a),
            [
                ("orders.amount".to_string(), true),
                ("orders.order_id".to_string(), true),
                ("bonus".to_string(), false)
            ]
        );
        assert_eq!(a.aggregates.len(), 2);
        assert!(a.aggregates[1].distinct);
        assert!(!a.nested_aggregate);
    }

    #[test]
    fn filter_clauses_count_as_inside_the_aggregate() {
        let a = analysis("SUM(amount) FILTER (WHERE status = 'paid')");
        assert_eq!(
            refs(&a),
            [("amount".to_string(), true), ("status".to_string(), true)]
        );
        assert!(a.aggregates[0].filtered);
    }

    #[test]
    fn count_star_has_no_references() {
        let a = analysis("COUNT(*)");
        assert_eq!(a.references, []);
        assert!(a.aggregates[0].star);
    }

    #[test]
    fn nesting_windows_and_subqueries_are_flagged() {
        assert!(analysis("SUM(MAX(x))").nested_aggregate);
        assert!(analysis("SUM(x) OVER (PARTITION BY y)").window);
        assert!(analysis("x IN (SELECT y FROM t)").subquery);
    }

    #[test]
    fn row_level_functions_are_not_aggregates() {
        let a = analysis("EXTRACT(YEAR FROM ordered_at) + LOWER(name)");
        assert_eq!(a.aggregates, []);
        assert_eq!(a.functions, ["lower"]);
        assert_eq!(a.references.len(), 2);
    }

    #[test]
    fn quoted_names_match_exactly_and_unquoted_ones_ignore_case() {
        let a = analysis("\"Amount\" + amount");
        assert!(a.references[0].parts[0].matches("Amount"));
        assert!(!a.references[0].parts[0].matches("amount"));
        assert!(a.references[1].parts[0].matches("AMOUNT"));
    }

    #[test]
    fn trailing_text_is_an_error() {
        let Err(err) = parse("SUM(x) garbage") else {
            panic!("should not parse");
        };
        assert!(err.0.contains("garbage"), "{err}");
        assert!(parse("SUM(").is_err());
    }
}
