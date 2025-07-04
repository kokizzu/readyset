use readyset_sql::ast::{
    BinaryOperator, DeleteStatement, Expr, Literal, SelectStatement, SqlQuery, UpdateStatement,
};

pub trait StripPostFilters {
    /// Remove all filters from the given query that cannot be done as nodes in the query graph, and
    /// require a post-lookup filter. Currently, this is LIKE and ILIKE against a placeholder.
    #[must_use]
    fn strip_post_filters(self) -> Self;
}

impl StripPostFilters for Option<Expr> {
    fn strip_post_filters(self) -> Self {
        self.and_then(|conds| match conds {
            Expr::BinaryOp { lhs, op, rhs } => match (lhs.as_ref(), op, rhs.as_ref()) {
                (
                    Expr::Column(_),
                    BinaryOperator::ILike | BinaryOperator::Like,
                    Expr::Literal(Literal::Placeholder(_)),
                ) => None,
                _ => match (
                    Some(*lhs).strip_post_filters(),
                    Some(*rhs).strip_post_filters(),
                ) {
                    (None, None) => None,
                    (Some(cond), None) | (None, Some(cond)) => Some(cond),
                    (Some(left), Some(right)) => Some(Expr::BinaryOp {
                        op,
                        lhs: Box::new(left),
                        rhs: Box::new(right),
                    }),
                },
            },
            _ => Some(conds),
        })
    }
}

impl StripPostFilters for SelectStatement {
    fn strip_post_filters(mut self) -> Self {
        self.where_clause = self.where_clause.strip_post_filters();
        self
    }
}

impl StripPostFilters for DeleteStatement {
    fn strip_post_filters(mut self) -> Self {
        self.where_clause = self.where_clause.strip_post_filters();
        self
    }
}

impl StripPostFilters for UpdateStatement {
    fn strip_post_filters(mut self) -> Self {
        self.where_clause = self.where_clause.strip_post_filters();
        self
    }
}

impl StripPostFilters for SqlQuery {
    fn strip_post_filters(self) -> Self {
        match self {
            SqlQuery::Select(select) => SqlQuery::Select(select.strip_post_filters()),
            SqlQuery::Delete(del) => SqlQuery::Delete(del.strip_post_filters()),
            SqlQuery::CompoundSelect(mut compound_select) => {
                compound_select.selects = compound_select
                    .selects
                    .drain(..)
                    .map(|(op, stmt)| (op, stmt.strip_post_filters()))
                    .collect();
                SqlQuery::CompoundSelect(compound_select)
            }
            SqlQuery::Update(upd) => SqlQuery::Update(upd.strip_post_filters()),
            _ => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use readyset_sql::{Dialect, DialectDisplay};
    use readyset_sql_parsing::parse_query;

    use super::*;

    #[test]
    fn strip_ilike() {
        let query =
            parse_query(Dialect::MySQL, "SELECT id FROM posts WHERE title ILIKE ?;").unwrap();
        let expected = parse_query(Dialect::MySQL, "SELECT id FROM posts;").unwrap();
        let result = query.strip_post_filters();
        assert_eq!(
            result,
            expected,
            "result = {}",
            result.display(readyset_sql::Dialect::MySQL)
        );
    }

    #[test]
    fn strip_ilike_with_other_conds() {
        let query = parse_query(
            Dialect::MySQL,
            "SELECT id FROM posts WHERE title ILIKE ? AND id < 5;",
        )
        .unwrap();
        let expected = parse_query(Dialect::MySQL, "SELECT id FROM posts WHERE id < 5;").unwrap();
        let result = query.strip_post_filters();
        assert_eq!(
            result,
            expected,
            "result = {}",
            result.display(readyset_sql::Dialect::MySQL)
        );
    }
}
