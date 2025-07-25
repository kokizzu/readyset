use std::fmt;

use itertools::Itertools;
use readyset_util::fmt::fmt_with;
use serde::{Deserialize, Serialize};
use test_strategy::Arbitrary;

use crate::{AstConversionError, Dialect, DialectDisplay, ast::*};

/// Type of index hint.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Arbitrary)]
pub enum IndexHintType {
    /// Use index hint.
    Use,

    /// Ignore index hint.
    Ignore,

    /// Force index hint.
    Force,
}

impl TryFrom<&str> for IndexHintType {
    type Error = AstConversionError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Ok(match s {
            "USE" => IndexHintType::Use,
            "IGNORE" => IndexHintType::Ignore,
            "FORCE" => IndexHintType::Force,
            e => unsupported!("Unsupported index hint type: {e}")?,
        })
    }
}

impl TryFrom<&&str> for IndexHintType {
    type Error = AstConversionError;

    fn try_from(s: &&str) -> Result<Self, Self::Error> {
        IndexHintType::try_from(*s)
    }
}

impl From<IndexHintType> for &str {
    fn from(t: IndexHintType) -> &'static str {
        match t {
            IndexHintType::Use => "USE",
            IndexHintType::Ignore => "IGNORE",
            IndexHintType::Force => "FORCE",
        }
    }
}

/* TODO(mvzink): verify we don't need this
impl<'a> From<LocatedSpan<&'a [u8]>> for IndexHintType {
    fn from(span: LocatedSpan<&'a [u8]>) -> Self {
        let s = span.fragment();
        let str_slice = std::str::from_utf8(s).expect("Invalid UTF-8 string");
        IndexHintType::from(str_slice)
    }
}
*/

/// Type of index or key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Arbitrary)]
pub enum IndexOrKeyType {
    /// Index.
    Index,

    /// Key.
    Key,
}

impl TryFrom<&str> for IndexOrKeyType {
    type Error = AstConversionError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Ok(match s {
            "INDEX" => IndexOrKeyType::Index,
            "KEY" => IndexOrKeyType::Key,
            e => unsupported!("Invalid index or key type: {e}")?,
        })
    }
}

impl TryFrom<&&str> for IndexOrKeyType {
    type Error = AstConversionError;

    fn try_from(s: &&str) -> Result<Self, Self::Error> {
        IndexOrKeyType::try_from(*s)
    }
}

impl From<IndexOrKeyType> for &str {
    fn from(t: IndexOrKeyType) -> &'static str {
        match t {
            IndexOrKeyType::Index => "INDEX",
            IndexOrKeyType::Key => "KEY",
        }
    }
}

/// Index usage type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Arbitrary)]
pub enum IndexUsageType {
    /// FOR JOIN
    Join,

    /// FOR ORDER BY
    OrderBy,

    /// FOR GROUP BY
    GroupBy,
}

impl TryFrom<&str> for IndexUsageType {
    type Error = AstConversionError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Ok(match s {
            "FOR JOIN" => IndexUsageType::Join,
            "FOR ORDER BY" => IndexUsageType::OrderBy,
            "FOR GROUP BY" => IndexUsageType::GroupBy,
            e => unsupported!("Invalid index usage type: {e}")?,
        })
    }
}

impl TryFrom<&&str> for IndexUsageType {
    type Error = AstConversionError;

    fn try_from(s: &&str) -> Result<Self, Self::Error> {
        IndexUsageType::try_from(*s)
    }
}

impl From<IndexUsageType> for &str {
    fn from(t: IndexUsageType) -> &'static str {
        match t {
            IndexUsageType::Join => " FOR JOIN",
            IndexUsageType::OrderBy => " FOR ORDER BY",
            IndexUsageType::GroupBy => " FOR GROUP BY",
        }
    }
}
/// Index hints for a query.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Arbitrary)]
pub struct IndexHint {
    /// Type of index hint.
    pub hint_type: IndexHintType,

    /// Type of index or key.
    pub index_or_key: IndexOrKeyType,

    /// Index usage type.
    pub index_usage_type: Option<IndexUsageType>,

    /// List of indexes.
    pub index_list: Vec<SqlIdentifier>,
}

impl Default for IndexHint {
    /// Create a new index hint.
    fn default() -> Self {
        Self {
            hint_type: IndexHintType::Use,
            index_or_key: IndexOrKeyType::Index,
            index_usage_type: None,
            index_list: vec![],
        }
    }
}

impl DialectDisplay for IndexHint {
    fn display(&self, _dialect: Dialect) -> impl fmt::Display + '_ {
        fmt_with(move |f| {
            let hint_type: &str = self.hint_type.clone().into();
            let index_or_key: &str = self.index_or_key.clone().into();
            let index_usage: &str = self
                .index_usage_type
                .as_ref()
                .map(|t| t.clone().into())
                .unwrap_or("");
            let index_list = self.index_list.iter().map(|t| t.to_owned()).join(", ");
            write!(f, "{hint_type} {index_or_key}{index_usage} ({index_list})")
        })
    }
}
