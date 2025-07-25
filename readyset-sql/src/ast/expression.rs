use std::{fmt, iter, mem};

use concrete_iter::concrete_iter;
use derive_more::derive::From;
use itertools::Itertools;
use proptest::prelude::{Arbitrary, BoxedStrategy};
use readyset_util::fmt::fmt_with;
use serde::{Deserialize, Serialize};
use test_strategy::Arbitrary;

use crate::{
    AstConversionError, Dialect, DialectDisplay, IntoDialect, TryFromDialect, TryIntoDialect,
    ast::*,
};

/// Function call expressions
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, Serialize, Deserialize, Arbitrary)]
pub enum FunctionExpr {
    /// `AVG` aggregation. The boolean argument is `true` if `DISTINCT`
    Avg { expr: Box<Expr>, distinct: bool },

    /// `COUNT` aggregation
    Count { expr: Box<Expr>, distinct: bool },

    /// `COUNT(*)` aggregation
    CountStar,

    Extract {
        field: TimestampField,
        expr: Box<Expr>,
    },

    /// The SQL `LOWER`/`UPPER` functions.
    ///
    /// The supported syntax for MySQL dialect is:
    ///
    /// `LOWER(string)`
    /// `UPPER(string)`
    /// Note, `collation` will always be None for MySQL dialect
    ///
    /// The supported syntax for Postgres dialect is:
    ///
    /// `LOWER(string [COLLATE collation_name])`
    /// `UPPER(string [COLLATE collation_name])`
    Lower {
        expr: Box<Expr>,
        collation: Option<CollationName>,
    },
    Upper {
        expr: Box<Expr>,
        collation: Option<CollationName>,
    },

    /// `SUM` aggregation
    Sum { expr: Box<Expr>, distinct: bool },

    /// `MAX` aggregation
    Max(Box<Expr>),

    /// `MIN` aggregation
    Min(Box<Expr>),

    /// `GROUP_CONCAT` aggregation
    GroupConcat {
        expr: Box<Expr>,
        separator: Option<String>,
    },

    /// The SQL `SUBSTRING`/`SUBSTR` function.
    ///
    /// The supported syntax is one of:
    ///
    /// `SUBSTR[ING](string FROM pos FOR len)`
    /// `SUBSTR[ING](string FROM pos)`
    /// `SUBSTR[ING](string FOR len)`
    /// `SUBSTR[ING](string, pos)`
    /// `SUBSTR[ING](string, pos, len)`
    Substring {
        string: Box<Expr>,
        pos: Option<Box<Expr>>,
        len: Option<Box<Expr>>,
    },

    JsonObjectAgg {
        key: Box<Expr>,
        value: Box<Expr>,
        allow_duplicate_keys: bool,
    },

    /// Generic function call expression
    Call {
        /// Name of the function, always lowercased even if we don't recognize it.
        name: SqlIdentifier,
        /// Arguments to the function, or `None` if called without parentheses. With parens but no
        /// arguments is `Some(vec![])`.
        arguments: Option<Vec<Expr>>,
    },
}

impl FunctionExpr {
    pub fn alias(&self, dialect: Dialect) -> Option<String> {
        Some(match self {
            FunctionExpr::Avg { expr, .. } => format!("avg({})", expr.alias(dialect)?),
            FunctionExpr::Count { expr, .. } => format!("count({})", expr.alias(dialect)?),
            FunctionExpr::Sum { expr, .. } => format!("sum({})", expr.alias(dialect)?),
            FunctionExpr::Max(col) => format!("max({})", col.alias(dialect)?),
            FunctionExpr::Min(col) => format!("min({})", col.alias(dialect)?),
            FunctionExpr::Extract { field, expr } => {
                format!("extract({field} from {})", expr.alias(dialect)?)
            }
            FunctionExpr::Lower { expr, collation } => format!(
                "lower({}{})",
                expr.alias(dialect)?,
                if let Some(c) = collation {
                    format!(" COLLATE \"{c}\"")
                } else {
                    "".to_string()
                }
            ),
            FunctionExpr::Upper { expr, collation } => format!(
                "upper({}{})",
                expr.alias(dialect)?,
                if let Some(c) = collation {
                    format!(" COLLATE \"{c}\"")
                } else {
                    "".to_string()
                }
            ),
            FunctionExpr::GroupConcat { expr, separator } => format!(
                "group_concat({}, {})",
                expr.alias(dialect)?,
                separator
                    .as_ref()
                    .map(|s| format!("'{}'", s.replace('\'', "''").replace('\\', "\\\\")))
                    .unwrap_or_default(),
            ),
            FunctionExpr::Substring { string, pos, len } => format!(
                "substring({}, {}, {})",
                string.alias(dialect)?,
                pos.as_ref()
                    .map(|pos| pos.alias(dialect).unwrap_or_default())
                    .unwrap_or_default(),
                len.as_ref()
                    .map(|len| len.alias(dialect).unwrap_or_default())
                    .unwrap_or_default(),
            ),
            FunctionExpr::JsonObjectAgg {
                key,
                value,
                allow_duplicate_keys,
            } => {
                format!(
                    "{}({}, {})",
                    if *allow_duplicate_keys {
                        "json_object_agg"
                    } else {
                        "jsonb_object_agg"
                    },
                    key.alias(dialect)?,
                    value.alias(dialect)?
                )
            }
            FunctionExpr::CountStar => "count(*)".to_string(),
            FunctionExpr::Call {
                name,
                arguments: None,
            } => name.to_string(),
            FunctionExpr::Call {
                name,
                arguments: Some(arguments),
            } => format!(
                "{}({})",
                name,
                arguments
                    .iter()
                    .map(|arg| arg.alias(dialect))
                    .collect::<Option<Vec<_>>>()?
                    .join(", ") //FIXME
            ),
        })
    }
}

impl FunctionExpr {
    /// Returns an iterator over all the direct arguments passed to the given function call
    /// expression
    #[concrete_iter]
    pub fn arguments<'a>(&'a self) -> impl Iterator<Item = &'a Expr> {
        match self {
            FunctionExpr::Avg { expr: arg, .. }
            | FunctionExpr::Count { expr: arg, .. }
            | FunctionExpr::Sum { expr: arg, .. }
            | FunctionExpr::Max(arg)
            | FunctionExpr::Min(arg)
            | FunctionExpr::GroupConcat { expr: arg, .. }
            | FunctionExpr::Extract { expr: arg, .. }
            | FunctionExpr::Lower { expr: arg, .. }
            | FunctionExpr::Upper { expr: arg, .. } => {
                concrete_iter!(iter::once(arg.as_ref()))
            }
            FunctionExpr::JsonObjectAgg { key, value, .. } => {
                concrete_iter!(iter::once(key.as_ref()).chain(iter::once(value.as_ref())))
            }
            FunctionExpr::CountStar => concrete_iter!(iter::empty()),
            FunctionExpr::Call {
                arguments: None, ..
            } => concrete_iter!(iter::empty()),
            FunctionExpr::Call {
                arguments: Some(arguments),
                ..
            } => concrete_iter!(arguments),
            FunctionExpr::Substring { string, pos, len } => {
                concrete_iter!(
                    iter::once(string.as_ref())
                        .chain(pos.iter().map(|p| p.as_ref()))
                        .chain(len.iter().map(|p| p.as_ref()))
                )
            }
        }
    }
}

impl DialectDisplay for FunctionExpr {
    fn display(&self, dialect: Dialect) -> impl fmt::Display + '_ {
        fmt_with(move |f| match self {
            FunctionExpr::Avg {
                expr,
                distinct: true,
            } => write!(f, "avg(distinct {})", expr.display(dialect)),
            FunctionExpr::Count {
                expr,
                distinct: true,
            } => write!(f, "count(distinct {})", expr.display(dialect)),
            FunctionExpr::Sum {
                expr,
                distinct: true,
            } => write!(f, "sum(distinct {})", expr.display(dialect)),
            FunctionExpr::Avg { expr, .. } => write!(f, "avg({})", expr.display(dialect)),
            FunctionExpr::Count { expr, .. } => write!(f, "count({})", expr.display(dialect)),
            FunctionExpr::CountStar => write!(f, "count(*)"),
            FunctionExpr::Sum { expr, .. } => write!(f, "sum({})", expr.display(dialect)),
            FunctionExpr::Max(col) => write!(f, "max({})", col.display(dialect)),
            FunctionExpr::Min(col) => write!(f, "min({})", col.display(dialect)),
            FunctionExpr::GroupConcat { expr, separator } => {
                write!(f, "group_concat({}", expr.display(dialect),)?;
                if let Some(separator) = separator {
                    write!(
                        f,
                        " separator '{}'",
                        separator.replace('\'', "''").replace('\\', "\\\\")
                    )?;
                }
                write!(f, ")")
            }
            FunctionExpr::Call {
                name,
                arguments: None,
            } => write!(f, "{name}"),
            FunctionExpr::Call {
                name,
                arguments: Some(arguments),
            } => {
                write!(
                    f,
                    "{}({})",
                    name,
                    arguments.iter().map(|arg| arg.display(dialect)).join(", ")
                )
            }
            FunctionExpr::Substring { string, pos, len } => {
                write!(f, "substring({}", string.display(dialect))?;

                if let Some(pos) = pos {
                    write!(f, " from {}", pos.display(dialect))?;
                }

                if let Some(len) = len {
                    write!(f, " for {}", len.display(dialect))?;
                }

                write!(f, ")")
            }
            FunctionExpr::JsonObjectAgg {
                key,
                value,
                allow_duplicate_keys,
            } => {
                write!(
                    f,
                    "{}({}, {})",
                    if *allow_duplicate_keys {
                        "json_object_agg"
                    } else {
                        "jsonb_object_agg"
                    },
                    key.display(dialect),
                    value.display(dialect)
                )
            }
            FunctionExpr::Extract { field, expr } => {
                write!(f, "extract({field} FROM {})", expr.display(dialect))
            }
            FunctionExpr::Lower { expr, collation } => {
                write!(f, "lower({}", expr.display(dialect))?;
                if let Some(c) = collation {
                    write!(f, " COLLATE \"{c}\"")?;
                }
                write!(f, ")")
            }
            FunctionExpr::Upper { expr, collation } => {
                write!(f, "upper({}", expr.display(dialect))?;
                if let Some(c) = collation {
                    write!(f, " COLLATE \"{c}\"")?;
                }
                write!(f, ")")
            }
        })
    }
}

/// Binary infix operators with [`Expr`] on both the left- and right-hand sides
///
/// This type is used as the operator in [`Expr::BinaryOp`].
///
/// Note that because all binary operators have expressions on both sides, SQL `IN` is not a binary
/// operator - since it must have either a subquery or a list of expressions on its right-hand side
#[derive(
    Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord, Serialize, Deserialize, Arbitrary,
)]
pub enum BinaryOperator {
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `LIKE`
    Like,
    /// `NOT LIKE`
    NotLike,
    /// `ILIKE`
    ILike,
    /// `NOT ILIKE`
    NotILike,
    /// `=`
    Equal,
    /// `!=` or `<>`
    NotEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterOrEqual,
    /// `<`
    Less,
    /// `<=`
    LessOrEqual,
    /// `IS`
    Is,
    /// `IS NOT`
    IsNot,
    /// `+`
    Add,
    /// `-`
    Subtract,

    /// Postgres-specific `AT TIME ZONE` operator.
    AtTimeZone,

    /// `#-`
    ///
    /// Postgres-specific JSONB operator that takes JSONB and returns JSONB with the value at a
    /// key/index path removed.
    HashSubtract,

    /// `*`
    Multiply,
    /// `/`
    Divide,

    /// `?`
    ///
    /// Postgres-specific JSONB operator. Looks for the given string as an object key or an array
    /// element and returns a boolean indicating the presence or absence of that string.
    QuestionMark,

    /// `?|`
    ///
    /// Postgres-specific JSONB operator. Takes an array of strings and checks whether *any* of
    /// those strings appear as object keys or array elements in the provided JSON value.
    QuestionMarkPipe,

    /// `?&`
    ///
    /// Postgres-specific JSONB operator. Takes an array of strings and checks whether *all* of
    /// those strings appear as object keys or array elements in the provided JSON value.
    QuestionMarkAnd,

    /// `||`
    ///
    /// This can represent a few different operators in different contexts. In MySQL it can
    /// represent a boolean OR operation or a string concat operation depending on whether
    /// `PIPES_AS_CONCAT` is enabled in the SQL mode. In Postgres it can either represent string
    /// concatenation or JSON concatenation, depending on the context.
    DoublePipe,

    /// `->`
    ///
    /// This extracts JSON values as JSON:
    /// - MySQL: `json -> jsonpath` to `json` (unimplemented)
    /// - PostgreSQL: `json[b] -> {text,integer}` to `json[b]`
    Arrow1,

    /// `->>`
    ///
    /// This extracts JSON values and applies a transformation:
    /// - MySQL: `json ->> jsonpath` to unquoted `text` (unimplemented)
    /// - PostgreSQL: `json[b] ->> {text,integer}` to `text`
    Arrow2,

    /// PostgreSQL `#>`
    ///
    /// This extracts JSON values as JSON: `json[b] #> text[]` to `json[b]`.
    HashArrow1,

    /// PostgreSQL `#>>`
    ///
    /// This extracts JSON values as JSON: `json[b] #>> text[]` to `text`.
    HashArrow2,

    /// `@>`
    ///
    /// Postgres-specific JSONB operator. Takes two JSONB values and determines whether the
    /// left-side values immediately contain all of the right-side values.
    AtArrowRight,

    /// `<@`
    ///
    /// Postgres-specific JSONB operator. Behaves like [`BinaryOperator::AtArrowRight`] with
    /// switched sides for the operands.
    AtArrowLeft,
}

impl BinaryOperator {
    /// Returns true if this operator represents an ordered comparison
    pub fn is_ordering_comparison(&self) -> bool {
        use BinaryOperator::*;
        matches!(self, Greater | GreaterOrEqual | Less | LessOrEqual)
    }

    /// If this operator is an ordered comparison, invert its meaning. (i.e. Greater becomes
    /// Less)
    pub fn flip_ordering_comparison(self) -> Result<Self, Self> {
        use BinaryOperator::*;
        match self {
            Greater => Ok(Less),
            GreaterOrEqual => Ok(LessOrEqual),
            Less => Ok(Greater),
            LessOrEqual => Ok(GreaterOrEqual),
            _ => Err(self),
        }
    }
}

impl TryFrom<sqlparser::ast::BinaryOperator> for BinaryOperator {
    type Error = AstConversionError;

    fn try_from(value: sqlparser::ast::BinaryOperator) -> Result<Self, Self::Error> {
        use sqlparser::ast::BinaryOperator as BinOp;
        Ok(match value {
            BinOp::And => Self::And,
            BinOp::Arrow => Self::Arrow1,
            BinOp::ArrowAt => Self::AtArrowLeft,
            BinOp::AtArrow => Self::AtArrowRight,
            BinOp::AtAt => unsupported!("@@ {value:?}")?,
            BinOp::AtQuestion => unsupported!("@? {value:?}")?,
            BinOp::BitwiseAnd => unsupported!("& {value:?}")?,
            BinOp::BitwiseOr => unsupported!("| {value:?}")?,
            BinOp::BitwiseXor => unsupported!("^ {value:?}")?,
            BinOp::Custom(_) => unsupported!("Custom operator {value:?}")?,
            BinOp::Divide => Self::Divide,
            BinOp::DuckIntegerDivide => unsupported!("DuckDB // {value:?}")?,
            BinOp::Eq => Self::Equal,
            BinOp::Gt => Self::Greater,
            BinOp::GtEq => Self::GreaterOrEqual,
            BinOp::HashArrow => Self::HashArrow1,
            BinOp::HashLongArrow => Self::HashArrow2,
            BinOp::HashMinus => Self::HashSubtract,
            BinOp::LongArrow => Self::Arrow2,
            BinOp::Lt => Self::Less,
            BinOp::LtEq => Self::LessOrEqual,
            BinOp::Minus => Self::Subtract,
            BinOp::Modulo => unsupported!("% {value:?}")?,
            BinOp::Multiply => Self::Multiply,
            BinOp::MyIntegerDivide => unsupported!("MySQL DIV {value:?}")?,
            BinOp::NotEq => Self::NotEqual,
            BinOp::Or => Self::Or,
            BinOp::Overlaps => unsupported!("OVERLAPS {value:?}")?,
            BinOp::PGBitwiseShiftLeft => unsupported!("PGBitwiseShiftLeft '<<'")?,
            BinOp::PGBitwiseShiftRight => unsupported!("PGBitwiseShiftRight '>>'")?,
            BinOp::PGBitwiseXor => unsupported!("PGBitwiseXor '#'")?,
            BinOp::PGCustomBinaryOperator(_vec) => unsupported!("PGCustomBinaryOperator")?,
            BinOp::PGExp => unsupported!("PGExp '^'")?,
            BinOp::PGILikeMatch => Self::ILike,
            BinOp::PGLikeMatch => Self::Like,
            BinOp::PGNotILikeMatch => Self::NotILike,
            BinOp::PGNotLikeMatch => Self::NotLike,
            BinOp::PGOverlap => unsupported!("PGOverlap '&&'")?,
            BinOp::PGRegexIMatch => unsupported!("PGRegexIMatch '~*'")?,
            BinOp::PGRegexMatch => unsupported!("PGRegexMatch '~'")?,
            BinOp::PGRegexNotIMatch => unsupported!("PGRegexNotIMatch '!~*'")?,
            BinOp::PGRegexNotMatch => unsupported!("PGRegexNotMatch '!~'")?,
            BinOp::PGStartsWith => unsupported!("PGStartsWith '^@'")?,
            BinOp::Plus => Self::Add,
            BinOp::Question => Self::QuestionMark,
            BinOp::QuestionAnd => Self::QuestionMarkAnd,
            BinOp::QuestionPipe => Self::QuestionMarkPipe,
            BinOp::Spaceship => unsupported!("Spaceship '<=>'")?,
            BinOp::StringConcat => Self::DoublePipe,
            BinOp::Xor => unsupported!("XOR operator")?,
            BinOp::DoubleHash => unsupported!("DoubleHash '##'")?,
            BinOp::LtDashGt => unsupported!("LtDashGt '<->'")?,
            BinOp::AndLt => unsupported!("AndLt '&<'")?,
            BinOp::AndGt => unsupported!("AndGt '&>'")?,
            BinOp::LtLtPipe => unsupported!("LtLtPipe '<<|'")?,
            BinOp::PipeGtGt => unsupported!("PipeGtGt '|>>'")?,
            BinOp::AndLtPipe => unsupported!("AndLtPipe '&<|'")?,
            BinOp::PipeAndGt => unsupported!("PipeAndGt '|&>'")?,
            BinOp::LtCaret => unsupported!("LtCaret '<^'")?,
            BinOp::GtCaret => unsupported!("GtCaret '>^'")?,
            BinOp::QuestionHash => unsupported!("QuestionHash '?#'")?,
            BinOp::QuestionDash => unsupported!("QuestionDash '?-'")?,
            BinOp::QuestionDashPipe => unsupported!("QuestionDashPipe '?-|'")?,
            BinOp::QuestionDoublePipe => unsupported!("QuestionDoublePipe '?||'")?,
            BinOp::At => unsupported!("At '@'")?,
            BinOp::TildeEq => unsupported!("TildeEq '~='")?,
            BinOp::Assignment => unsupported!("Assignment ':='")?,
            BinOp::Match => unsupported!("MATCH operator")?,
            BinOp::Regexp => unsupported!("REGEXP operator")?,
        })
    }
}

impl fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let op = match *self {
            Self::And => "AND",
            Self::Or => "OR",
            Self::Like => "LIKE",
            Self::NotLike => "NOT LIKE",
            Self::ILike => "ILIKE",
            Self::NotILike => "NOT ILIKE",
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Is => "IS",
            Self::IsNot => "IS NOT",
            Self::Add => "+",
            Self::Subtract => "-",
            Self::HashSubtract => "#-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::QuestionMark => "?",
            Self::QuestionMarkPipe => "?|",
            Self::QuestionMarkAnd => "?&",
            Self::DoublePipe => "||",
            Self::Arrow1 => "->",
            Self::Arrow2 => "->>",
            Self::HashArrow1 => "#>",
            Self::HashArrow2 => "#>>",
            Self::AtArrowRight => "@>",
            Self::AtArrowLeft => "<@",
            Self::AtTimeZone => "AT TIME ZONE",
        };
        f.write_str(op)
    }
}

#[derive(
    Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord, Serialize, Deserialize, Arbitrary,
)]
pub enum UnaryOperator {
    Neg,
    Not,
}

impl TryFrom<sqlparser::ast::UnaryOperator> for UnaryOperator {
    type Error = AstConversionError;

    fn try_from(value: sqlparser::ast::UnaryOperator) -> Result<Self, Self::Error> {
        use sqlparser::ast::UnaryOperator as UnOp;
        match value {
            UnOp::Plus => not_yet_implemented!("Unary + operator"),
            UnOp::Minus => Ok(Self::Neg),
            UnOp::Not => Ok(Self::Not),
            UnOp::PGBitwiseNot
            | UnOp::PGSquareRoot
            | UnOp::PGCubeRoot
            | UnOp::PGPostfixFactorial
            | UnOp::PGPrefixFactorial
            | UnOp::PGAbs => unsupported!("unsupported postgres unary operator"),
            UnOp::BangNot
            | UnOp::Hash
            | UnOp::AtDashAt
            | UnOp::DoubleAt
            | UnOp::QuestionDash
            | UnOp::QuestionPipe => unsupported!("unsupported unary operator {value}"),
        }
    }
}

impl fmt::Display for UnaryOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnaryOperator::Neg => write!(f, "-"),
            UnaryOperator::Not => write!(f, "NOT"),
        }
    }
}

/// Right-hand side of IN
#[derive(
    Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Serialize, Deserialize, From, Arbitrary,
)]
pub enum InValue {
    Subquery(Box<SelectStatement>),
    List(Vec<Expr>),
}

impl DialectDisplay for InValue {
    fn display(&self, dialect: Dialect) -> impl fmt::Display + '_ {
        fmt_with(move |f| match self {
            InValue::Subquery(stmt) => write!(f, "{}", stmt.display(dialect)),
            InValue::List(exprs) => write!(
                f,
                "{}",
                exprs.iter().map(|expr| expr.display(dialect)).join(", ")
            ),
        })
    }
}

/// A single branch of a `CASE WHEN` statement
#[derive(
    Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Serialize, Deserialize, From, Arbitrary,
)]
pub struct CaseWhenBranch {
    pub condition: Expr,
    pub body: Expr,
}

impl DialectDisplay for CaseWhenBranch {
    fn display(&self, dialect: Dialect) -> impl fmt::Display + '_ {
        fmt_with(move |f| {
            write!(
                f,
                "WHEN {} THEN {}",
                self.condition.display(dialect),
                self.body.display(dialect)
            )
        })
    }
}

/// SQL Expression AST
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Serialize, Deserialize, From)]
pub enum Expr {
    /// Function call expressions
    ///
    /// TODO(aspen): Eventually, the members of FunctionExpr should be inlined here
    Call(FunctionExpr),

    /// Literal values
    Literal(Literal),

    /// Binary operator
    BinaryOp {
        lhs: Box<Expr>,
        op: BinaryOperator,
        rhs: Box<Expr>,
    },

    /// `<expr> <op> ANY (<expr>)` ([PostgreSQL docs][pg-docs])
    ///
    /// [pg-docs]: https://www.postgresql.org/docs/current/functions-comparisons.html#id-1.5.8.30.16
    #[from(ignore)]
    OpAny {
        lhs: Box<Expr>,
        op: BinaryOperator,
        rhs: Box<Expr>,
    },

    /// `<expr> <op> SOME (<expr>)` ([PostgreSQL docs][pg-docs])
    ///
    /// [pg-docs]: https://www.postgresql.org/docs/current/functions-comparisons.html#id-1.5.8.30.16
    #[from(ignore)]
    OpSome {
        lhs: Box<Expr>,
        op: BinaryOperator,
        rhs: Box<Expr>,
    },

    /// `<expr> <op> ALL (<expr>)` ([PostgreSQL docs][pg-docs])
    ///
    /// [pg-docs]: https://www.postgresql.org/docs/current/functions-comparisons.html#id-1.5.8.30.16
    #[from(ignore)]
    OpAll {
        lhs: Box<Expr>,
        op: BinaryOperator,
        rhs: Box<Expr>,
    },

    /// Unary operator
    UnaryOp { op: UnaryOperator, rhs: Box<Expr> },

    /// CASE (WHEN condition THEN then_expr)... ELSE else_expr
    CaseWhen {
        branches: Vec<CaseWhenBranch>,
        else_expr: Option<Box<Expr>>,
    },

    /// A reference to a column
    Column(Column),

    /// EXISTS (select)
    #[from(ignore)]
    Exists(Box<SelectStatement>),

    /// operand BETWEEN min AND max
    Between {
        operand: Box<Expr>,
        min: Box<Expr>,
        max: Box<Expr>,
        negated: bool,
    },

    /// A nested SELECT query
    NestedSelect(Box<SelectStatement>),

    /// An IN (or NOT IN) predicate
    ///
    /// Per the ANSI SQL standard, IN is its own AST node, not a binary operator
    In {
        lhs: Box<Expr>,
        rhs: InValue,
        negated: bool,
    },

    /// `CAST(expression AS type)`.
    Cast {
        expr: Box<Expr>,
        ty: SqlType,
        /// If true indicates that the expression used the Postgres syntax (expr::type)
        postgres_style: bool,
    },

    /// `ARRAY[expr1, expr2, ...]`
    Array(Vec<Expr>),

    /// `ROW` constructor: `ROW(expr1, expr2, ...)` or `(expr1, expr2, ...)`
    Row {
        /// Is the `ROW` keyword explicit?
        explicit: bool,
        exprs: Vec<Expr>,
    },

    /// A variable reference
    Variable(Variable),

    /// Expr [COLLATE collation]
    /// This is here because that's how sqlparser represents it
    /// and it should be desugared before lowering
    Collate {
        expr: Box<Expr>,
        collation: CollationName,
    },
}

impl Expr {
    pub fn alias(&self, dialect: Dialect) -> Option<SqlIdentifier> {
        // TODO: Match upstream naming (unquoted identifiers, function name without args, etc ..)
        let mut alias = match self {
            Expr::Column(col) => col.name.to_string(), // strip the table's name
            Expr::BinaryOp { lhs, op, rhs } => {
                let left = lhs.alias(dialect)?;
                let right = rhs.alias(dialect)?;
                format!("{left} {op} {right}")
            }
            Expr::Call(function) => function.alias(dialect)?,

            // Placeholders in select are not GA'd, but just in case
            Expr::Literal(Literal::Placeholder(_)) => return None,
            Expr::Variable(_) => return None,

            // FIXME: follow dialect's naming convention
            e => e.display(dialect).to_string(),
        };

        alias = alias.chars().take(64).collect();

        Some(alias.into())
    }

    /// If this is a `Self::BinaryOp` where the right hand side is an ANY or ALL function call,
    /// extract it to turn this into a `Self::AllOp` or `Self::AnyOp`.
    ///
    /// This is necessary because for some binary operators (namely, `LIKE` and its variants),
    /// sqlparser-rs does not parse them as binary operators, but as special expression variants. As
    /// a result, it does not recognize the `ALL` or `ANY` comparison on the right, and parses it as
    /// a regular function call (in the case of `ALL`) or with a special flag on the `Expr::Like`
    /// variant (in the case of `ANY`). So for `LIKE`/`ILIKE`, which have the special `any` flag,
    /// this pass does nothing: but it should catch any other operators which behave this way and
    /// *don't* have a special flag for `any`.
    ///
    /// It may be worth the effort to make these representations more uniform on the sqlparser-rs
    /// side; see issue [#1770](https://github.com/apache/datafusion-sqlparser-rs/issues/1770).
    fn extract_all_any_op(self) -> Result<Self, AstConversionError> {
        if let Expr::BinaryOp { lhs, op, rhs } = self {
            match *rhs {
                Expr::Call(FunctionExpr::Call {
                    name,
                    arguments: Some(arguments),
                }) if name.eq_ignore_ascii_case("ALL") => {
                    Ok(Self::OpAll {
                        lhs,
                        op,
                        rhs: Box::new(arguments.into_iter().exactly_one().map_err(|_| {
                            failed_err!("Wrong number of arguments for ALL operator")
                        })?),
                    })
                }
                Expr::Call(FunctionExpr::Call {
                    name,
                    arguments: Some(arguments),
                }) if name.eq_ignore_ascii_case("ANY") => {
                    Ok(Self::OpAny {
                        lhs,
                        op,
                        rhs: Box::new(arguments.into_iter().exactly_one().map_err(|_| {
                            failed_err!("Wrong number of arguments for ANY operator")
                        })?),
                    })
                }
                _ => Ok(Expr::BinaryOp { lhs, op, rhs }),
            }
        } else {
            Ok(self)
        }
    }
}

impl TryFromDialect<sqlparser::ast::Expr> for Expr {
    fn try_from_dialect(
        value: sqlparser::ast::Expr,
        dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        use sqlparser::ast::Expr::*;
        match value {
            AllOp {
                left,
                compare_op,
                right,
            } => Ok(Self::OpAll {
                lhs: left.try_into_dialect(dialect)?,
                op: compare_op.try_into()?,
                rhs: right.try_into_dialect(dialect)?,
            }),
            AnyOp {
                left,
                compare_op,
                right,
                is_some: false,
            } => Ok(Self::OpAny {
                lhs: left.try_into_dialect(dialect)?,
                op: compare_op.try_into()?,
                rhs: right.try_into_dialect(dialect)?,
            }),
            AnyOp {
                left,
                compare_op,
                right,
                is_some: true,
            } => Ok(Self::OpSome {
                lhs: left.try_into_dialect(dialect)?,
                op: compare_op.try_into()?,
                rhs: right.try_into_dialect(dialect)?,
            }),
            Array(array) => Ok(Self::Array(array.elem.try_into_dialect(dialect)?)),
            AtTimeZone {
                timestamp,
                time_zone,
            } => Ok(Self::BinaryOp {
                lhs: timestamp.try_into_dialect(dialect)?,
                op: BinaryOperator::AtTimeZone,
                rhs: time_zone.try_into_dialect(dialect)?,
            }),
            Between {
                expr,
                negated,
                low,
                high,
            } => Ok(Self::Between {
                operand: expr.try_into_dialect(dialect)?,
                min: low.try_into_dialect(dialect)?,
                max: high.try_into_dialect(dialect)?,
                negated,
            }),
            BinaryOp { left, op, right } => Ok(Self::BinaryOp {
                lhs: left.try_into_dialect(dialect)?,
                op: op.try_into()?,
                rhs: right.try_into_dialect(dialect)?,
            }),
            Case {
                operand: None,
                conditions,
                else_result,
                ..
            } => Ok(Self::CaseWhen {
                branches: conditions
                    .into_iter()
                    .map(|condition| {
                        Ok(CaseWhenBranch {
                            condition: condition.condition.try_into_dialect(dialect)?,
                            body: condition.result.try_into_dialect(dialect)?,
                        })
                    })
                    .try_collect()?,
                else_expr: else_result.try_into_dialect(dialect)?,
            }),
            Case {
                operand: Some(expr),
                conditions,
                else_result,
                ..
            } => Ok(Self::CaseWhen {
                branches: conditions
                    .into_iter()
                    .map(|condition| {
                        Ok(CaseWhenBranch {
                            condition: Expr::BinaryOp {
                                lhs: expr.clone().try_into_dialect(dialect)?,
                                op: BinaryOperator::Equal,
                                rhs: Box::new(condition.condition.try_into_dialect(dialect)?),
                            },
                            body: condition.result.try_into_dialect(dialect)?,
                        })
                    })
                    .try_collect()?,
                else_expr: else_result.try_into_dialect(dialect)?,
            }),
            Cast {
                kind,
                expr,
                data_type,
                format: _, // TODO: I think this is where we would support `AT TIMEZONE` syntax
            } => Ok(Self::Cast {
                expr: expr.try_into_dialect(dialect)?,
                ty: data_type.try_into_dialect(dialect)?,
                postgres_style: kind == sqlparser::ast::CastKind::DoubleColon,
            }),
            Ceil { expr: _, field: _ } => not_yet_implemented!("CEIL"),
            Collate { expr, collation } => Ok(Self::Collate {
                expr: expr.try_into_dialect(dialect)?,
                collation: collation.try_into_dialect(dialect)?,
            }),
            CompoundIdentifier(idents) => {
                let is_variable = if let Some(first) = idents.first() {
                    first.quote_style.is_none() && first.value.starts_with('@')
                } else {
                    false
                };
                if is_variable {
                    Ok(Self::Variable(idents.try_into_dialect(dialect)?))
                } else {
                    let column: Column = idents.into_dialect(dialect);
                    Ok(Self::Column(column))
                }
            }
            Convert {
                expr: _,
                data_type: _,
                charset: _,
                target_before_value: _,
                styles: _,
                is_try: _,
            } => unsupported!("CONVERT"), // XXX: this could be supported in some cases by rewriting to `CAST`
            Cube(_vec) => not_yet_implemented!("CUBE"),
            Dictionary(_vec) => not_yet_implemented!("DICTIONARY"),
            Exists { subquery, negated } => {
                if negated {
                    Ok(Self::UnaryOp {
                        op: crate::ast::UnaryOperator::Not,
                        rhs: Box::new(Self::Exists(subquery.try_into_dialect(dialect)?)),
                    })
                } else {
                    Ok(Self::Exists(subquery.try_into_dialect(dialect)?))
                }
            }
            Extract {
                field,
                syntax: _, // We only support FROM
                expr,
            } => Ok(Self::Call(FunctionExpr::Extract {
                field: field.try_into()?,
                expr: expr.try_into_dialect(dialect)?,
            })),
            Floor { expr: _, field: _ } => not_yet_implemented!("FLOOR"),
            Function(function) => function.try_into_dialect(dialect),
            GroupingSets(_vec) => unsupported!("GROUPING SETS"),
            Identifier(ident) => Ok(ident.try_into_dialect(dialect)?),
            InList {
                expr,
                list,
                negated,
            } => Ok(Self::In {
                lhs: expr.try_into_dialect(dialect)?,
                rhs: crate::ast::InValue::List(list.try_into_dialect(dialect)?),
                negated,
            }),
            InSubquery {
                expr,
                subquery,
                negated,
            } => Ok(Self::In {
                lhs: expr.try_into_dialect(dialect)?,
                rhs: crate::ast::InValue::Subquery(subquery.try_into_dialect(dialect)?),
                negated,
            }),
            Interval(_interval) => not_yet_implemented!("INTERVAL"),
            InUnnest {
                expr: _,
                array_expr: _,
                negated: _,
            } => not_yet_implemented!("IN UNNEST"),
            IsFalse(expr) => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: BinaryOperator::Is,
                rhs: Box::new(Expr::Literal(crate::ast::Literal::Boolean(false))),
            }),
            IsNotFalse(expr) => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: BinaryOperator::IsNot,
                rhs: Box::new(Expr::Literal(crate::ast::Literal::Boolean(false))),
            }),
            IsTrue(expr) => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: BinaryOperator::Is,
                rhs: Box::new(Expr::Literal(crate::ast::Literal::Boolean(true))),
            }),
            IsNotTrue(expr) => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: BinaryOperator::IsNot,
                rhs: Box::new(Expr::Literal(crate::ast::Literal::Boolean(true))),
            }),
            IsNotNull(expr) => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: BinaryOperator::IsNot,
                rhs: Box::new(Expr::Literal(crate::ast::Literal::Null)),
            }),
            IsNull(expr) => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: BinaryOperator::Is,
                rhs: Box::new(Expr::Literal(crate::ast::Literal::Null)),
            }),
            IsDistinctFrom(_expr, _expr1) => not_yet_implemented!("IS DISTINCT FROM"),
            IsNotDistinctFrom(_expr, _expr1) => not_yet_implemented!("IS NOT DISTINCT FROM"),
            IsUnknown(_expr) => not_yet_implemented!("IS UNKNOWN"),
            IsNotUnknown(_expr) => not_yet_implemented!("IS NOT UNKNOWN"),
            JsonAccess { value: _, path: _ } => not_yet_implemented!("JSON access"),
            Lambda(_lambda_function) => unsupported!("LAMBDA"),
            Like {
                negated,
                expr,
                pattern,
                escape_char: _,
                any: false,
            } => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: if negated {
                    BinaryOperator::NotLike
                } else {
                    BinaryOperator::Like
                },
                rhs: pattern.try_into_dialect(dialect)?,
            }
            .extract_all_any_op()?),
            Like {
                negated,
                expr,
                pattern,
                escape_char: _,
                any: true,
            } => Ok(Self::OpAny {
                lhs: expr.try_into_dialect(dialect)?,
                op: if negated {
                    BinaryOperator::NotLike
                } else {
                    BinaryOperator::Like
                },
                rhs: pattern.try_into_dialect(dialect)?,
            }),
            ILike {
                negated,
                expr,
                pattern,
                escape_char: _,
                any: false,
            } => Ok(Self::BinaryOp {
                lhs: expr.try_into_dialect(dialect)?,
                op: if negated {
                    BinaryOperator::NotILike
                } else {
                    BinaryOperator::ILike
                },
                rhs: pattern.try_into_dialect(dialect)?,
            }
            .extract_all_any_op()?),
            ILike {
                negated,
                expr,
                pattern,
                escape_char: _,
                any: true,
            } => Ok(Self::OpAny {
                lhs: expr.try_into_dialect(dialect)?,
                op: if negated {
                    BinaryOperator::NotILike
                } else {
                    BinaryOperator::ILike
                },
                rhs: pattern.try_into_dialect(dialect)?,
            }),
            Map(_map) => not_yet_implemented!("MAP"),
            MatchAgainst {
                columns: _,
                match_value: _,
                opt_search_modifier: _,
            } => not_yet_implemented!("MATCH AGAINST"),
            Named { expr: _, name: _ } => unsupported!("BigQuery named expression"),
            Nested(expr) => expr.try_into_dialect(dialect),
            OuterJoin(_expr) => not_yet_implemented!("OUTER JOIN"),
            Overlay {
                expr: _,
                overlay_what: _,
                overlay_from: _,
                overlay_for: _,
            } => unsupported!("OVERLAY"),
            Position { expr: _, r#in: _ } => not_yet_implemented!("POSITION"),
            Prior(_expr) => not_yet_implemented!("PRIOR"),
            RLike {
                negated: _,
                expr: _,
                pattern: _,
                regexp: _,
            } => unsupported!("RLIKE"),
            Rollup(_vec) => unsupported!("ROLLUP"),
            SimilarTo {
                negated: _,
                expr: _,
                pattern: _,
                escape_char: _,
            } => unsupported!("SIMILAR TO"),
            Struct {
                values: _,
                fields: _,
            } => unsupported!("STRUCT"),
            Subquery(query) => Ok(Self::NestedSelect(query.try_into_dialect(dialect)?)),
            Substring {
                expr,
                substring_from,
                substring_for,
                special: false,
                shorthand: _,
            } => Ok(Self::Call(FunctionExpr::Substring {
                string: expr.try_into_dialect(dialect)?,
                pos: substring_from
                    .map(|expr| expr.try_into_dialect(dialect))
                    .transpose()?,
                len: substring_for
                    .map(|expr| expr.try_into_dialect(dialect))
                    .transpose()?,
            })),
            Substring {
                expr,
                substring_from,
                substring_for,
                special: true,
                shorthand,
            } => {
                let mut arguments = vec![expr.try_into_dialect(dialect)?];
                if let Some(pos) = substring_from.try_into_dialect(dialect)? {
                    arguments.push(pos);
                }
                if let Some(len) = substring_for.try_into_dialect(dialect)? {
                    arguments.push(len);
                }
                let name = if shorthand {
                    "substr".into_dialect(dialect)
                } else {
                    "substring".into_dialect(dialect)
                };
                Ok(Self::Call(FunctionExpr::Call {
                    name,
                    arguments: Some(arguments),
                }))
            }
            Trim {
                expr: _,
                trim_where: _,
                trim_what: _,
                trim_characters: _,
            } => not_yet_implemented!("TRIM"),
            Tuple(vec) => Ok(Self::Row {
                exprs: vec.try_into_dialect(dialect)?,
                explicit: false, // TODO: Fix upstrem in sqlparser
            }),
            TypedString {
                data_type: _,
                value: _,
            } => unsupported!("TYPED STRING"),
            // TODO(mvzink): Remove these negation special cases once we disable nom-sql; they're
            // just here for checking parity
            UnaryOp {
                op: sqlparser::ast::UnaryOperator::Minus,
                expr,
            } => match expr.try_into_dialect(dialect)? {
                Expr::Literal(Literal::UnsignedInteger(i)) => {
                    let literal = i64::try_from(i)
                        .ok()
                        .and_then(|i| i.checked_neg())
                        .map(Literal::Integer)
                        .unwrap_or_else(|| Literal::Number(format!("-{i}")));
                    Ok(Self::Literal(literal))
                }
                Expr::Literal(Literal::Integer(i)) => Ok(Self::Literal(Literal::Integer(-i))),
                Expr::Literal(Literal::Number(s)) if !s.starts_with('-') => {
                    Ok(Self::Literal(Literal::Number(format!("-{s}"))))
                }
                Expr::Literal(Literal::Number(s)) if s.starts_with('-') => {
                    Ok(Self::Literal(Literal::Number(s[1..].to_string())))
                }
                expr => Ok(Self::UnaryOp {
                    op: UnaryOperator::Neg,
                    rhs: Box::new(expr),
                }),
            },
            UnaryOp { op, expr } => Ok(Self::UnaryOp {
                op: op.try_into()?,
                rhs: expr.try_into_dialect(dialect)?,
            }),
            Value(value) => Ok(Self::Literal(value.try_into()?)),
            cfa @ CompoundFieldAccess {
                root: _,
                access_chain: _,
            } => {
                unsupported!("Compound field access a la `foo['bar'].baz[1]`: `{cfa}` = {cfa:?}")
            }
            // not sure what these are for, parity tests seem to be
            // passing normally though
            Wildcard(_token) => unsupported!("wildcard expression in this context"),
            QualifiedWildcard(_object_name, _token) => {
                unsupported!("qualified wildcard expression in this context")
            }
            IsNormalized { .. } => unsupported!("IS NORMALIZED"),
            // Prefixed expression like introducer string
            // https://dev.mysql.com/doc/refman/8.0/en/charset-introducer.html
            // prefix is ignored for now
            Prefixed { value, .. } => value.try_into_dialect(dialect),
        }
    }
}

impl TryFromDialect<Box<sqlparser::ast::Expr>> for Box<Expr> {
    fn try_from_dialect(
        value: Box<sqlparser::ast::Expr>,
        dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        Ok(Box::new(value.try_into_dialect(dialect)?))
    }
}

impl TryFromDialect<Box<sqlparser::ast::Expr>> for Expr {
    fn try_from_dialect(
        value: Box<sqlparser::ast::Expr>,
        dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        (*value).try_into_dialect(dialect)
    }
}

impl TryFromDialect<sqlparser::ast::ObjectName> for CollationName {
    fn try_from_dialect(
        value: sqlparser::ast::ObjectName,
        _dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        // strip the quoting style from the ObjectName
        let ident = value
            .0
            .iter()
            .map(|s| s.as_ident().unwrap().value.clone())
            .join(".");
        Ok(CollationName::Unquoted(ident.into()))
    }
}

/// Convert a sqlparser-rs's `Ident` into a `Expr`; special handling because it might be a variable
/// or a column and sqlparser doesn't distinguish them.
///
/// TODO(mvzink): This may not actually be necessary for recent sqlparser versions: check for usage
/// of `CompoundIdentifier`; also check whether this needs to know the dialect for re-parsing the
/// variable name.
impl TryFromDialect<sqlparser::ast::Ident> for Expr {
    fn try_from_dialect(
        value: sqlparser::ast::Ident,
        dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        if dialect == Dialect::MySQL && value.quote_style.is_none() && value.value.starts_with('@')
        {
            Ok(Self::Variable(value.try_into_dialect(dialect)?))
        } else if value.quote_style.is_none()
            && (value.value.starts_with('$') || value.value == "?" || value.value.starts_with(':'))
        {
            Ok(Self::Literal(Literal::Placeholder(
                (&value.value).try_into()?,
            )))
        } else {
            Ok(Self::Column(value.into_dialect(dialect)))
        }
    }
}

/// Convert a function call into an expression.
///
/// We don't turn every function into a [`FunctionExpr`], because we have some special cases that
/// turn into other kinds of expressions, such as `DATE(x)` into `CAST(x AS DATE)`.
impl TryFromDialect<sqlparser::ast::Function> for Expr {
    fn try_from_dialect(
        value: sqlparser::ast::Function,
        dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        // TODO: handle null treatment and other stuff
        let sqlparser::ast::Function { args, name, .. } = value;

        let sqlparser::ast::ObjectNamePart::Identifier(mut ident) = name
            .0
            .into_iter()
            .exactly_one()
            .map_err(|_| unsupported_err!("non-builtin function (UDF)"))?;

        // Special case for `COUNT(*)`
        if ident.value.eq_ignore_ascii_case("COUNT") {
            use sqlparser::ast::{
                FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments,
            };
            match args {
                FunctionArguments::List(FunctionArgumentList { args, .. })
                    if args == vec![FunctionArg::Unnamed(FunctionArgExpr::Wildcard)] =>
                {
                    return Ok(Self::Call(FunctionExpr::CountStar));
                }
                _ => {}
            }
        }

        let (args, distinct, separator) = match args {
            sqlparser::ast::FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                args,
                duplicate_treatment,
                clauses, // TODO: handle other stuff like order/limit, etc.
            }) => (
                args,
                duplicate_treatment == Some(sqlparser::ast::DuplicateTreatment::Distinct),
                clauses.into_iter().find_map(|clause| match clause {
                    sqlparser::ast::FunctionArgumentClause::Separator(separator) => {
                        Some(sqlparser_value_into_string(separator))
                    }
                    _ => None,
                }),
            ),
            sqlparser::ast::FunctionArguments::None => {
                ident.value.make_ascii_lowercase();
                return Ok(Self::Call(FunctionExpr::Call {
                    name: ident.into_dialect(dialect),
                    arguments: None,
                }));
            }
            other => {
                return not_yet_implemented!(
                    "subquery function call argument for {ident}: {other:?}"
                );
            }
        };

        let mut exprs = args.into_iter().map(|arg| arg.try_into_dialect(dialect));
        let mut next_expr = || {
            exprs
                .next()
                .ok_or_else(|| failed_err!("not enough arguments for {ident}"))?
                .map(Box::new)
        };

        let expr = if ident.value.eq_ignore_ascii_case("AVG") {
            Self::Call(FunctionExpr::Avg {
                expr: next_expr()?,
                distinct,
            })
        } else if ident.value.eq_ignore_ascii_case("COUNT") {
            Self::Call(FunctionExpr::Count {
                expr: next_expr()?,
                distinct,
            })
        } else if ident.value.eq_ignore_ascii_case("DATE") {
            // TODO: Arguably, this should be in a SQL rewrite pass to preserve input when rendering
            Self::Cast {
                expr: next_expr()?,
                ty: crate::ast::SqlType::Date,
                postgres_style: false,
            }
        } else if ident.value.eq_ignore_ascii_case("EXTRACT") {
            return failed!("{ident} should have been converted earlier");
        } else if ident.value.eq_ignore_ascii_case("GROUP_CONCAT") {
            Self::Call(FunctionExpr::GroupConcat {
                expr: next_expr()?,
                separator,
            })
        } else if ident.value.eq_ignore_ascii_case("JSON_OBJECT_AGG") {
            Self::Call(FunctionExpr::JsonObjectAgg {
                key: next_expr()?,
                value: next_expr()?,
                allow_duplicate_keys: true,
            })
        } else if ident.value.eq_ignore_ascii_case("JSONB_OBJECT_AGG")
            || ident.value.eq_ignore_ascii_case("JSON_OBJECTAGG")
        {
            Self::Call(FunctionExpr::JsonObjectAgg {
                key: next_expr()?,
                value: next_expr()?,
                allow_duplicate_keys: false,
            })
        } else if ident.value.eq_ignore_ascii_case("LOWER") {
            let expr = next_expr()?;
            match *expr {
                Self::Collate { expr, collation } => Self::Call(FunctionExpr::Lower {
                    expr,
                    collation: Some(collation),
                }),
                _ => Self::Call(FunctionExpr::Lower {
                    expr,
                    collation: None,
                }),
            }
        } else if ident.value.eq_ignore_ascii_case("MAX") {
            Self::Call(FunctionExpr::Max(next_expr()?))
        } else if ident.value.eq_ignore_ascii_case("MIN") {
            Self::Call(FunctionExpr::Min(next_expr()?))
        } else if ident.value.eq_ignore_ascii_case("ROW") {
            Self::Row {
                explicit: true,
                exprs: exprs.try_collect()?,
            }
        } else if ident.value.eq_ignore_ascii_case("SUM") {
            Self::Call(FunctionExpr::Sum {
                expr: next_expr()?,
                distinct,
            })
        } else if ident.value.eq_ignore_ascii_case("UPPER") {
            let expr = next_expr()?;
            match *expr {
                Self::Collate { expr, collation } => Self::Call(FunctionExpr::Upper {
                    expr,
                    collation: Some(collation),
                }),
                _ => Self::Call(FunctionExpr::Upper {
                    expr,
                    collation: None,
                }),
            }
        } else {
            ident.value.make_ascii_lowercase();
            Self::Call(FunctionExpr::Call {
                name: ident.into_dialect(dialect),
                arguments: Some(exprs.try_collect()?),
            })
        };
        Ok(expr)
    }
}

fn sqlparser_value_into_string(value: sqlparser::ast::Value) -> String {
    use sqlparser::ast::Value::*;
    match value {
        Number(s, _)
        | SingleQuotedString(s)
        | DollarQuotedString(sqlparser::ast::DollarQuotedString { value: s, .. })
        | TripleSingleQuotedString(s)
        | TripleDoubleQuotedString(s)
        | EscapedStringLiteral(s)
        | UnicodeStringLiteral(s)
        | SingleQuotedByteStringLiteral(s)
        | DoubleQuotedByteStringLiteral(s)
        | TripleSingleQuotedByteStringLiteral(s)
        | TripleDoubleQuotedByteStringLiteral(s)
        | SingleQuotedRawStringLiteral(s)
        | DoubleQuotedRawStringLiteral(s)
        | TripleSingleQuotedRawStringLiteral(s)
        | TripleDoubleQuotedRawStringLiteral(s)
        | NationalStringLiteral(s)
        | DoubleQuotedString(s)
        | HexStringLiteral(s)
        | Placeholder(s) => s,
        Boolean(b) => b.to_string(),
        Null => "NULL".to_string(),
    }
}

impl TryFromDialect<sqlparser::ast::FunctionArg> for Expr {
    fn try_from_dialect(
        value: sqlparser::ast::FunctionArg,
        dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        use sqlparser::ast::FunctionArg::*;
        match value {
            Named { arg, .. } | ExprNamed { arg, .. } | Unnamed(arg) => {
                arg.try_into_dialect(dialect)
            }
        }
    }
}

impl TryFromDialect<sqlparser::ast::FunctionArgExpr> for Expr {
    fn try_from_dialect(
        value: sqlparser::ast::FunctionArgExpr,
        dialect: Dialect,
    ) -> Result<Self, AstConversionError> {
        use sqlparser::ast::FunctionArgExpr::*;
        match value {
            Expr(expr) => expr.try_into_dialect(dialect),
            QualifiedWildcard(object_name) => Ok(Self::Column(object_name.into_dialect(dialect))),
            Wildcard => not_yet_implemented!("wildcard expression in function argument"),
        }
    }
}

impl DialectDisplay for Expr {
    fn display(&self, dialect: Dialect) -> impl fmt::Display + '_ {
        fmt_with(move |f| match self {
            Expr::Call(fe) => write!(f, "{}", fe.display(dialect)),
            Expr::Literal(l) => write!(f, "{}", l.display(dialect)),
            Expr::Column(col) => write!(f, "{}", col.display(dialect)),
            Expr::CaseWhen {
                branches,
                else_expr,
            } => {
                write!(f, "CASE ")?;
                for branch in branches {
                    write!(f, "{} ", branch.display(dialect))?;
                }
                if let Some(else_expr) = else_expr {
                    write!(f, "ELSE {} ", else_expr.display(dialect))?;
                }
                write!(f, "END")
            }
            Expr::BinaryOp { lhs, op, rhs } => write!(
                f,
                "({} {op} {})",
                lhs.display(dialect),
                rhs.display(dialect)
            ),
            Expr::OpAny { lhs, op, rhs } => write!(
                f,
                "{} {op} ANY ({})",
                lhs.display(dialect),
                rhs.display(dialect)
            ),
            Expr::OpSome { lhs, op, rhs } => write!(
                f,
                "{} {op} SOME ({})",
                lhs.display(dialect),
                rhs.display(dialect)
            ),
            Expr::OpAll { lhs, op, rhs } => write!(
                f,
                "{} {op} ALL ({})",
                lhs.display(dialect),
                rhs.display(dialect)
            ),
            Expr::UnaryOp {
                op: UnaryOperator::Neg,
                rhs,
            } => write!(f, "(-{})", rhs.display(dialect)),
            Expr::UnaryOp { op, rhs } => write!(f, "({op} {})", rhs.display(dialect)),
            Expr::Exists(statement) => write!(f, "EXISTS ({})", statement.display(dialect)),

            Expr::Between {
                operand,
                min,
                max,
                negated,
            } => {
                write!(
                    f,
                    "{} {}BETWEEN {} AND {}",
                    operand.display(dialect),
                    if *negated { "NOT " } else { "" },
                    min.display(dialect),
                    max.display(dialect)
                )
            }
            Expr::In { lhs, rhs, negated } => {
                write!(f, "{}", lhs.display(dialect))?;
                if *negated {
                    write!(f, " NOT")?;
                }
                write!(f, " IN ({})", rhs.display(dialect))
            }
            Expr::NestedSelect(q) => write!(f, "({})", q.display(dialect)),
            Expr::Cast {
                expr,
                ty,
                postgres_style,
            } if *postgres_style => {
                write!(f, "({}::{})", expr.display(dialect), ty.display(dialect))
            }
            Expr::Cast { expr, ty, .. } => write!(
                f,
                "CAST({} as {})",
                expr.display(dialect),
                ty.display(dialect)
            ),

            Expr::Array(exprs) => {
                fn write_value(
                    expr: &Expr,
                    dialect: Dialect,
                    f: &mut fmt::Formatter,
                ) -> fmt::Result {
                    match expr {
                        Expr::Array(elems) => {
                            write!(f, "[")?;
                            for (i, elem) in elems.iter().enumerate() {
                                if i != 0 {
                                    write!(f, ",")?;
                                }
                                write_value(elem, dialect, f)?;
                            }
                            write!(f, "]")
                        }
                        _ => write!(f, "{}", expr.display(dialect)),
                    }
                }

                write!(f, "ARRAY[")?;
                for (i, expr) in exprs.iter().enumerate() {
                    if i != 0 {
                        write!(f, ",")?;
                    }
                    write_value(expr, dialect, f)?;
                }
                write!(f, "]")
            }
            Expr::Row { explicit, exprs } => {
                if *explicit {
                    write!(f, "ROW")?;
                }
                write!(f, "(")?;
                for (i, expr) in exprs.iter().enumerate() {
                    if i != 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", expr.display(dialect))?;
                }
                write!(f, ")")
            }
            Expr::Variable(var) => write!(f, "{}", var.display(dialect)),
            Expr::Collate { expr, collation } => {
                write!(f, "{} COLLATE {}", expr.display(dialect), collation)
            }
        })
    }
}

impl Expr {
    /// If this expression is a [binary operator application](Expr::BinaryOp), returns a tuple
    /// of the left-hand side, the operator, and the right-hand side, otherwise returns None
    pub fn as_binary_op(&self) -> Option<(&Expr, BinaryOperator, &Expr)> {
        match self {
            Expr::BinaryOp { lhs, op, rhs } => Some((lhs.as_ref(), *op, rhs.as_ref())),
            _ => None,
        }
    }

    /// Returns true if any variables are present in the expression
    pub fn contains_vars(&self) -> bool {
        match self {
            Expr::Variable(_) => true,
            _ => self.recursive_subexpressions().any(Self::contains_vars),
        }
    }

    /// Functions similarly to mem::take, replacing the argument with a meaningless placeholder and
    /// returning the value from the method, thus providing a way to move ownership more easily.
    pub fn take(&mut self) -> Self {
        // If Expr implemented Default we could use mem::take directly for this purpose, but we
        // decided that it felt semantically weird and arbitrary to have a Default implementation
        // for Expr that returned a null literal, since there isn't really such a thing as a
        // "default" expression.
        mem::replace(self, Expr::Literal(Literal::Null))
    }
}

impl Arbitrary for Expr {
    type Parameters = ();

    type Strategy = BoxedStrategy<Expr>;

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        use proptest::option;
        use proptest::prelude::*;

        prop_oneof![
            any::<Literal>().prop_map(Expr::Literal),
            any::<Column>().prop_map(Expr::Column),
            any::<Variable>().prop_map(Expr::Variable),
        ]
        .prop_recursive(4, 8, 4, |element| {
            let box_expr = element.clone().prop_map(Box::new);
            prop_oneof![
                prop_oneof![
                    (box_expr.clone(), any::<bool>())
                        .prop_map(|(expr, distinct)| FunctionExpr::Avg { expr, distinct }),
                    (box_expr.clone(), any::<bool>())
                        .prop_map(|(expr, distinct)| FunctionExpr::Count { expr, distinct }),
                    Just(FunctionExpr::CountStar),
                    (box_expr.clone(), any::<bool>())
                        .prop_map(|(expr, distinct)| FunctionExpr::Sum { expr, distinct }),
                    (box_expr.clone(), any::<TimestampField>())
                        .prop_map(|(expr, field)| FunctionExpr::Extract { expr, field }),
                    box_expr.clone().prop_map(FunctionExpr::Max),
                    box_expr.clone().prop_map(FunctionExpr::Min),
                    (box_expr.clone(), any::<Option<String>>()).prop_map(|(expr, separator)| {
                        FunctionExpr::GroupConcat { expr, separator }
                    }),
                    (
                        box_expr.clone(),
                        option::of(box_expr.clone()),
                        option::of(box_expr.clone())
                    )
                        .prop_map(|(string, pos, len)| {
                            FunctionExpr::Substring { string, pos, len }
                        }),
                    (
                        any::<SqlIdentifier>(),
                        proptest::collection::vec(element.clone(), 0..24)
                    )
                        .prop_map(|(name, arguments)| FunctionExpr::Call {
                            name,
                            arguments: Some(arguments)
                        })
                ]
                .prop_map(Expr::Call),
                (box_expr.clone(), any::<BinaryOperator>(), box_expr.clone(),)
                    .prop_map(|(lhs, op, rhs)| Expr::BinaryOp { lhs, op, rhs },),
                (box_expr.clone(), any::<BinaryOperator>(), box_expr.clone(),)
                    .prop_map(|(lhs, op, rhs)| Expr::OpAny { lhs, op, rhs },),
                (box_expr.clone(), any::<BinaryOperator>(), box_expr.clone(),)
                    .prop_map(|(lhs, op, rhs)| Expr::OpSome { lhs, op, rhs },),
                (box_expr.clone(), any::<BinaryOperator>(), box_expr.clone(),)
                    .prop_map(|(lhs, op, rhs)| Expr::OpAll { lhs, op, rhs },),
                (any::<UnaryOperator>(), box_expr.clone(),)
                    .prop_map(|(op, rhs)| Expr::UnaryOp { op, rhs },),
                (
                    proptest::collection::vec(
                        (element.clone(), element.clone())
                            .prop_map(|(condition, body)| CaseWhenBranch { condition, body }),
                        1..24
                    ),
                    option::of(box_expr.clone())
                )
                    .prop_map(|(branches, else_expr)| Expr::CaseWhen {
                        branches,
                        else_expr
                    }),
                (
                    box_expr.clone(),
                    box_expr.clone(),
                    box_expr.clone(),
                    any::<bool>(),
                )
                    .prop_map(|(operand, min, max, negated)| Expr::Between {
                        operand,
                        min,
                        max,
                        negated
                    }),
                (
                    box_expr.clone(),
                    /* TODO: IN (subquery) */
                    proptest::collection::vec(element.clone(), 1..24).prop_map(InValue::List),
                    any::<bool>(),
                )
                    .prop_map(|(lhs, rhs, negated)| Expr::In { lhs, rhs, negated }),
                (box_expr, any::<SqlType>(), any::<bool>()).prop_map(
                    |(expr, ty, postgres_style)| {
                        Expr::Cast {
                            expr,
                            ty,
                            postgres_style,
                        }
                    }
                ),
                proptest::collection::vec(element, 0..24).prop_map(Expr::Array),
                // TODO: once we have Arbitrary for SelectStatement
                // any::<Box<SelectStatement>>().prop_map(Expr::NestedSelect),
                // any::<Box<SelectStatement>>().prop_map(Expr::Exists),
            ]
        })
        .boxed()
    }
}

/// Suffixes which can be supplied to operators to convert them into predicates on arrays or
/// subqueries.
///
/// Used for support of `<expr> <op> ANY ...`, `<expr> <op> SOME ...`, and `<expr> <op> ALL ...`
#[derive(Debug, Clone, Copy)]
pub enum OperatorSuffix {
    Any,
    Some,
    All,
}
