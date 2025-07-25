//! A deterministic, exhaustive, parametric generator for SQL queries, and associated DDL.
//!
//! The intent of this library is to provide a hook for generating SQL queries both
//! *deterministically*, via exhaustively iterating over all permutations of all sets of operations
//! that are supported, while also allowing *randomly* generating queries (aka "fuzz testing"),
//! permuting over parameters to operations with a larger state space.
//!
//! This serves a dual purpose:
//!
//! - Deterministically generating queries allows us to write benchmark suites that run on every
//!   commit, and give us an isolated comparative metric of how our performance changes over time
//! - Randomly generating queries and seed data allows us to generate test cases (with the
//!   `noria-logictest` crate elsewhere in the repository) to evaluate the correctness of our system
//!   and catch regressions.
//!
//! # Examples
//!
//! Generating a simple query, with a single query parameter and a single inner join:
//!
//! ```rust
//! use readyset_sql::{ast::JoinOperator, Dialect, DialectDisplay};
//! use query_generator::{GeneratorState, QueryOperation, QuerySeed};
//!
//! let mut gen = GeneratorState::default();
//! let query = gen.generate_query(QuerySeed::new(
//!     vec![
//!         QueryOperation::SingleParameter,
//!         QueryOperation::Join(JoinOperator::InnerJoin),
//!     ],
//!     vec![],
//! ));
//! let query_str = query.statement.display(Dialect::MySQL).to_string();
//! assert_eq!(
//!     query_str,
//!     "SELECT `table_1`.`column_2` AS `alias_1`, `table_2`.`column_2` AS `alias_2` \
//! FROM `table_1` \
//! INNER JOIN `table_2` ON (`table_1`.`column_1` = `table_2`.`column_1`) \
//! WHERE (`table_1`.`column_1` = ?)"
//! );
//! ```
//!
//! # Architecture
//!
//! - There's a [`QueryOperation`] enum which enumerates, in some sense, the individual "operations"
//!   that can be performed as part of a SQL query
//! - Each [`QueryOperation`] knows how to [add itself to a SQL query][0]
//!   - To support that, there's a [`GeneratorState`] struct, to which mutable references get passed
//!     around, which knows how to summon up [new tables][1] and [columns][2] for use in queries
//! - Many [`QueryOperation`]s have extra fields, such as [`QueryOperation::TopK::limit`], which are
//!   hardcoded when exhaustively permuting combinations of operations, but allowed to be generated
//!   *randomly* when generating random queries via the [`Arbitrary`] impl
//! - The set of [`QueryOperation`]s for a query, plus the set of [`Subquery`]s that that query
//!   contains, are wrapped up together into a [`QuerySeed`] struct, which is passed to
//!   [`GeneratorState::generate_query`] to actually generate a SQL query
//!
//! [0]: QueryOperation::add_to_query
//! [1]: GeneratorState::fresh_table_mut
//! [2]: TableSpec::fresh_column
//! [3]: QueryOperation::permute

mod types;

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::error::Error;
use std::hash::Hash;
use std::iter::{self, FromIterator};
use std::ops::{Bound, DerefMut};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::anyhow;
use clap::Parser;
use data_generator::{
    random_value_of_type, unique_value_of_type, ColumnGenerationSpec, ColumnGenerator,
    DistributionAnnotation,
};
use derive_more::{Deref, Display, From, Into};
use itertools::{Either, Itertools};
use lazy_static::lazy_static;
use parking_lot::Mutex;
use proptest::arbitrary::{any, any_with, Arbitrary};
use proptest::sample::Select;
use proptest::strategy::{BoxedStrategy, Strategy};
use readyset_data::{Collation, DfType, DfValue, Dialect};
use readyset_sql::analysis::{contains_aggregate, ReferredColumns};
use readyset_sql::ast::{
    BinaryOperator, Column, ColumnConstraint, ColumnSpecification, CommonTableExpr,
    CreateTableBody, CreateTableStatement, Expr, FieldDefinitionExpr, FieldReference, FunctionExpr,
    InValue, ItemPlaceholder, JoinClause, JoinConstraint, JoinOperator, JoinRightSide, LimitClause,
    LimitValue, Literal, OrderBy, OrderClause, OrderType, Relation, SelectStatement, SqlIdentifier,
    SqlType, SqlTypeArbitraryOptions, TableExpr, TableExprInner, TableKey,
};
use readyset_sql::{Dialect as ParseDialect, TryFromDialect as _, TryIntoDialect as _};
use readyset_sql_passes::outermost_table_exprs;
use readyset_util::intervals::{BoundPair, IterBoundPair};
use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};
use test_strategy::Arbitrary;

use crate::types::{arbitrary_numeric_type, arbitrary_postgres_min_max_arg_type};

/// Query dialect to use when generating queries.
///
/// Used as the parameters to the Arbitrary impls for multiple types in this crate
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deref, From, Into)]
pub struct QueryDialect(pub ParseDialect);

impl Default for QueryDialect {
    fn default() -> Self {
        Self(ParseDialect::MySQL)
    }
}

impl PartialEq<ParseDialect> for QueryDialect {
    fn eq(&self, other: &ParseDialect) -> bool {
        self.0 == *other
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, From, Into, Display, Clone)]
#[repr(transparent)]
pub struct TableName(SqlIdentifier);

impl Borrow<SqlIdentifier> for TableName {
    fn borrow(&self) -> &SqlIdentifier {
        &self.0
    }
}

impl Borrow<str> for TableName {
    fn borrow(&self) -> &str {
        self.0.as_str()
    }
}

impl From<TableName> for Relation {
    fn from(name: TableName) -> Self {
        Relation {
            name: name.0,
            schema: None,
        }
    }
}

impl<'a> From<&'a TableName> for &'a str {
    fn from(tn: &'a TableName) -> Self {
        &tn.0
    }
}

impl From<&str> for TableName {
    fn from(tn: &str) -> Self {
        TableName(tn.into())
    }
}

impl From<&SqlIdentifier> for TableName {
    fn from(tn: &SqlIdentifier) -> Self {
        TableName(tn.clone())
    }
}

impl FromStr for TableName {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}

impl From<String> for TableName {
    fn from(s: String) -> Self {
        Self(s.into())
    }
}

#[derive(
    Debug, Eq, PartialEq, Ord, PartialOrd, Hash, From, Into, Display, Clone, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct ColumnName(SqlIdentifier);

impl From<ColumnName> for Column {
    fn from(name: ColumnName) -> Self {
        Self {
            name: name.0,
            table: None,
        }
    }
}

impl From<&str> for ColumnName {
    fn from(col: &str) -> Self {
        Self(col.into())
    }
}

impl From<&SqlIdentifier> for ColumnName {
    fn from(tn: &SqlIdentifier) -> Self {
        ColumnName(tn.clone())
    }
}

impl FromStr for ColumnName {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}

impl From<Column> for ColumnName {
    fn from(col: Column) -> Self {
        col.name.into()
    }
}

/// Try to find the [`ColumnSpecification`] for the primary key of the given create table statement
///
/// TODO(aspen): Ideally, this would reuse the `key_def_coalescing` rewrite pass, but that's buried
/// deep inside readyset-server - if we ever get a chance to extract rewrite passes to their own
/// crate, this should be updated to use that
pub fn find_primary_keys(stmt: &CreateTableStatement) -> Option<&ColumnSpecification> {
    let body = stmt.body.as_ref().unwrap();
    body.fields
        .iter()
        // Look for a column with a PRIMARY KEY constraint on the spec first
        .find(|f| f.constraints.contains(&ColumnConstraint::PrimaryKey))
        // otherwise, find a column corresponding to a standalone PRIMARY KEY table constraint
        .or_else(|| {
            body.keys
                .iter()
                .flatten()
                .find_map(|k| match k {
                    // TODO(aspen): This doesn't support compound primary keys
                    TableKey::PrimaryKey { columns, .. } => columns.first(),
                    _ => None,
                })
                .and_then(|col| body.fields.iter().find(|f| f.column == *col))
        })
}

#[derive(Debug)]
pub struct ColumnDataGeneration {
    pub generator: ColumnGenerator,
    /// Values per column that should be present in that column at least some of the time.
    ///
    /// This is used to ensure that queries that filter on constant values get at least some
    /// results
    expected_values: HashSet<DfValue>,
}

/// Column data type and data generation information.
#[derive(Debug, Clone)]
pub struct ColumnSpec {
    pub sql_type: SqlType,
    pub gen_spec: Arc<Mutex<ColumnDataGeneration>>,
}

#[derive(Debug, Clone)]
pub struct TableSpec {
    pub name: TableName,
    pub columns: HashMap<ColumnName, ColumnSpec>,
    column_name_counter: u32,

    /// Name of the primary key column for the table, if any
    pub primary_key: Option<ColumnName>,
}

impl From<CreateTableStatement> for TableSpec {
    fn from(stmt: CreateTableStatement) -> Self {
        let primary_key: Option<ColumnName> =
            find_primary_keys(&stmt).map(|cspec| cspec.column.clone().into());

        let body = stmt.body.unwrap();

        let mut spec = TableSpec {
            name: stmt.table.name.into(),
            columns: body
                .fields
                .iter()
                .map(|field| {
                    let sql_type = field.sql_type.clone();
                    let collation = field
                        .get_collation()
                        .map(|name| Collation::get_or_default(Dialect::DEFAULT_MYSQL, name));
                    let df_type = DfType::from_sql_type(
                        &sql_type,
                        Dialect::DEFAULT_MYSQL,
                        |_| None,
                        collation,
                    )
                    .unwrap();

                    let generator = if let Some(d) = field.has_default().and_then(|l| {
                        DfValue::try_from_dialect(l, Dialect::DEFAULT_MYSQL.into()).ok()
                    }) {
                        // Prefer the specified default value for a field
                        ColumnGenerator::Constant(
                            d.coerce_to(&df_type, &DfType::Unknown).unwrap().into(),
                        )
                    } else {
                        // Otherwise default to generating fields with a constant value.
                        ColumnGenerator::Constant(sql_type.clone().into())
                    };

                    (
                        field.column.name.clone().into(),
                        ColumnSpec {
                            sql_type,
                            gen_spec: Arc::new(Mutex::new(ColumnDataGeneration {
                                generator,
                                expected_values: HashSet::new(),
                            })),
                        },
                    )
                })
                .collect(),
            column_name_counter: 0,
            primary_key: primary_key.clone(),
        };

        for col in body
            .keys
            .into_iter()
            .flatten()
            .flat_map(|k| match k {
                    TableKey::PrimaryKey{columns: ks, .. }
                    | TableKey::UniqueKey { columns: ks, .. }
                      // HACK(aspen): To get foreign keys filled, we just mark them as unique, which
                      // given that we (currently) generate the same number of rows for each table
                      // means we're coincidentally guaranteed to get values matching the other side
                      // of the fk. This isn't super robust (unsurprisingly) and should probably be
                      // replaced with something smarter in the future.
                    | TableKey::ForeignKey { columns: ks, .. } => ks,
                    _ => vec![],
                })
            .map(|c| ColumnName::from(c.name))
            .chain(primary_key)
        {
            // Unwrap: Unique key columns come from the CreateTableStatement we just
            // generated the TableSpec from. They should be valid columns.
            let col_spec = spec.columns.get_mut(&col).unwrap();
            col_spec.gen_spec.lock().generator =
                ColumnGenerator::Unique(col_spec.sql_type.clone().into());
        }

        // Apply annotations in the end
        for field in body.fields.iter() {
            if let Some(d) = field
                .comment
                .as_deref()
                .and_then(|s| s.parse::<DistributionAnnotation>().ok())
            {
                let col_spec = spec
                    .columns
                    .get_mut(&ColumnName::from(field.column.name.as_str()))
                    .unwrap();

                let generator = d.spec.generator_for_col(field.sql_type.clone());
                col_spec.gen_spec.lock().generator = if d.unique {
                    generator.into_unique()
                } else {
                    generator
                }
            }
        }

        spec
    }
}

impl From<TableSpec> for CreateTableStatement {
    fn from(spec: TableSpec) -> Self {
        CreateTableStatement {
            if_not_exists: false,
            table: spec.name.into(),
            body: Ok(CreateTableBody {
                fields: spec
                    .columns
                    .into_iter()
                    .map(|(col_name, col_type)| ColumnSpecification {
                        column: col_name.into(),
                        sql_type: col_type.sql_type,
                        generated: None,
                        constraints: vec![],
                        comment: None,
                    })
                    .collect(),
                keys: spec.primary_key.map(|cn| {
                    vec![TableKey::PrimaryKey {
                        index_name: None,
                        constraint_name: None,
                        constraint_timing: None,
                        columns: vec![cn.into()],
                    }]
                }),
            }),
            options: Ok(vec![]),
        }
    }
}

impl TableSpec {
    pub fn new(name: TableName) -> Self {
        Self {
            name,
            columns: Default::default(),
            column_name_counter: 0,
            primary_key: None,
        }
    }

    /// Generate a new, unique column in this table (of an unspecified type) and return its name
    pub fn fresh_column(&mut self) -> ColumnName {
        self.fresh_column_with_type(SqlType::Int(None))
    }

    /// Generate a new, unique column in this table with the specified type and return its name.
    pub fn fresh_column_with_type(&mut self, col_type: SqlType) -> ColumnName {
        self.column_name_counter += 1;
        let column_name = ColumnName(format!("column_{}", self.column_name_counter).into());
        self.columns.insert(
            column_name.clone(),
            ColumnSpec {
                sql_type: col_type.clone(),
                gen_spec: Arc::new(Mutex::new(ColumnDataGeneration {
                    generator: ColumnGenerator::Constant(col_type.into()),
                    expected_values: HashSet::new(),
                })),
            },
        );
        column_name
    }

    /// Returns the name of *some* column in this table which passes filter, potentially generating
    /// a new column using `default_type` as the type if necessary
    pub fn some_column_name_filtered<T, F>(&mut self, default_type: T, mut filter: F) -> ColumnName
    where
        F: FnMut(&ColumnName, &ColumnSpec) -> bool,
        T: FnOnce() -> SqlType,
    {
        self.columns
            .iter()
            .filter(|(n, c)| filter(n, c))
            .map(|(n, _)| n)
            .next()
            .cloned()
            .unwrap_or_else(|| self.fresh_column_with_type(default_type()))
    }

    /// Returns the name of *some* column in this table, potentially generating a new column if
    /// necessary
    pub fn some_column_name(&mut self) -> ColumnName {
        self.some_column_name_filtered(|| SqlType::Int(None), |_, _| true)
    }

    /// Returns the name of *some* column in this table with the given type, potentially generating
    /// a new column if necessary
    pub fn some_column_with_type(&mut self, col_type: SqlType) -> ColumnName {
        self.columns
            .iter()
            .find_map(|(n, t)| {
                if t.sql_type == col_type {
                    Some(n)
                } else {
                    None
                }
            })
            .cloned()
            .unwrap_or_else(|| self.fresh_column_with_type(col_type))
    }

    /// Returns the name of *some* column in this table with the given type but different than the
    /// one specified, potentially generating a new column if necessary
    pub fn some_column_with_type_different_than(
        &mut self,
        col_type: SqlType,
        name: &ColumnName,
    ) -> ColumnName {
        self.columns
            .iter()
            .find_map(|(n, t)| {
                if t.sql_type == col_type && n != name {
                    Some(n)
                } else {
                    None
                }
            })
            .cloned()
            .unwrap_or_else(|| self.fresh_column_with_type(col_type))
    }

    /// Specifies that the column given by `column_name` should be a primary key value
    /// and generate unique column data.
    pub fn set_primary_key_column(&mut self, column_name: &ColumnName) {
        assert!(self.columns.contains_key(column_name));
        let col_spec = self.columns.get_mut(column_name).unwrap();
        col_spec.gen_spec.lock().generator =
            ColumnGenerator::Unique(col_spec.sql_type.clone().into());
    }

    /// Record that the column given by `column_name` should contain `value` at least some of the
    /// time.
    ///
    /// This can be used, for example, to ensure that queries that filter comparing against a
    /// constant value return at least some results
    pub fn expect_value(&mut self, column_name: ColumnName, value: DfValue) {
        assert!(self.columns.contains_key(&column_name));
        self.columns
            .get_mut(&column_name)
            .unwrap()
            .gen_spec
            .lock()
            .expected_values
            .insert(value);
    }

    /// Overrides the existing `gen_spec` for a column with `spec`.
    pub fn set_column_generator_spec(
        &mut self,
        column_name: ColumnName,
        spec: ColumnGenerationSpec,
    ) {
        assert!(self.columns.contains_key(&column_name));
        let col_spec = self.columns.get_mut(&column_name).unwrap();
        self.columns
            .get_mut(&column_name)
            .unwrap()
            .gen_spec
            .lock()
            .generator = spec.generator_for_col(col_spec.sql_type.clone());
    }

    /// Overrides the existing `gen_spec` for a set of columns..
    pub fn set_column_generator_specs(&mut self, specs: &[(ColumnName, ColumnGenerationSpec)]) {
        for s in specs {
            self.set_column_generator_spec(s.0.clone(), s.1.clone());
        }
    }

    fn generate_row(&mut self, index: usize, random: bool) -> HashMap<ColumnName, DfValue> {
        self.columns
            .iter_mut()
            .map(
                |(
                    col_name,
                    ColumnSpec {
                        sql_type: col_type,
                        gen_spec: col_spec,
                    },
                )| {
                    let mut spec = col_spec.lock();
                    let ColumnDataGeneration {
                        generator,
                        expected_values,
                    } = spec.deref_mut();
                    let value = match generator {
                        // Allow using the `index` for key columns which are specified
                        // as Unique.
                        ColumnGenerator::Unique(u) => u.gen(),
                        _ if index % 2 == 0 && !expected_values.is_empty() => expected_values
                            .iter()
                            .nth(index / 2 % expected_values.len())
                            .unwrap()
                            .clone(),
                        _ if random => random_value_of_type(col_type, rand::rng()),
                        ColumnGenerator::Constant(c) => c.gen(),
                        ColumnGenerator::Uniform(u) => u.gen(),
                        ColumnGenerator::Random(r) => r.gen(),
                        ColumnGenerator::RandomString(r) => r.gen(),
                        ColumnGenerator::RandomChars(r) => r.gen(),
                        ColumnGenerator::Zipfian(z) => z.gen(),
                        ColumnGenerator::NonRepeating(r) => r.gen(),
                    };

                    (col_name.clone(), value)
                },
            )
            .collect()
    }

    /// Generate `num_rows` rows of data for this table. If `random` is true, columns
    /// that are not unique and do not need to yield expected values, have their
    /// DataGenerationSpec overridden with DataGenerationSpec::Random.
    pub fn generate_data(
        &mut self,
        num_rows: usize,
        random: bool,
    ) -> Vec<HashMap<ColumnName, DfValue>> {
        self.generate_data_from_index(num_rows, 0, random)
    }

    /// Generate `num_rows` rows of data for this table starting with the index:
    /// `index`. If `random` is true, columns that are not unique and do not
    /// need to yield expected values, have their DataGenerationSpec overridden
    /// with DataGenerationSpec::Random.
    pub fn generate_data_from_index(
        &mut self,
        num_rows: usize,
        index: usize,
        random: bool,
    ) -> Vec<HashMap<ColumnName, DfValue>> {
        (index..index + num_rows)
            .map(|n| self.generate_row(n, random))
            .collect()
    }

    /// Ensure this table has a primary key column, and return its name
    pub fn primary_key(&mut self) -> &ColumnName {
        if self.primary_key.is_none() {
            let col = self.fresh_column_with_type(SqlType::Int(None));
            self.set_primary_key_column(&col);
            self.primary_key = Some(col)
        }

        // unwrap: we just set it to Some
        self.primary_key.as_ref().unwrap()
    }
}

/// How to add parameters to the query during generation
#[derive(Debug, Clone, Copy, Default)]
pub enum ParameterMode {
    /// Add positional (`?`) parameters
    #[default]
    Positional,
    /// Add numbered (`$1`, `$2`, ...) parameters
    Numbered,
}

#[derive(Debug, Default)]
pub struct GeneratorState {
    tables: HashMap<TableName, TableSpec>,
    table_name_counter: u32,
    parameter_mode: ParameterMode,
}

impl GeneratorState {
    /// Create a new [`GeneratorState`] with the given mode for adding new parameters
    pub fn with_parameter_mode(parameter_mode: ParameterMode) -> Self {
        Self {
            parameter_mode,
            ..Default::default()
        }
    }

    /// Create a new, unique, empty table, and return a mutable reference to that table
    pub fn fresh_table_mut(&mut self) -> &mut TableSpec {
        self.table_name_counter += 1;
        let table_name: TableName = format!("table_{}", self.table_name_counter).as_str().into();
        self.tables
            .entry(table_name)
            .or_insert_with_key(|tn| TableSpec::new(tn.clone()))
    }

    /// Returns a reference to the table with the given name, if it exists
    pub fn table<'a, TN>(&'a self, name: &TN) -> Option<&'a TableSpec>
    where
        TableName: Borrow<TN>,
        TN: Eq + Hash + ?Sized,
    {
        self.tables.get(name)
    }

    /// Returns a mutable reference to the table with the given name, if it exists
    pub fn table_mut<'a, TN>(&'a mut self, name: &TN) -> Option<&'a mut TableSpec>
    where
        TableName: Borrow<TN>,
        TN: Eq + Hash + ?Sized,
    {
        self.tables.get_mut(name)
    }

    /// Returns an iterator over all the names of tables created for queries by this generator state
    pub fn table_names(&self) -> impl Iterator<Item = &TableName> {
        self.tables.keys()
    }

    /// Return a mutable reference to *some* table in the schema - the implication being that the
    /// caller doesn't care which table
    pub fn some_table_mut(&mut self) -> &mut TableSpec {
        if self.tables.is_empty() {
            self.fresh_table_mut()
        } else {
            self.tables.values_mut().next().unwrap()
        }
    }

    pub fn new_query(&mut self) -> QueryState<'_> {
        QueryState::new(self)
    }

    /// Generate a new query using the given [`QuerySeed`]
    pub fn generate_query(&mut self, seed: QuerySeed) -> Query {
        let mut state = self.new_query();
        let query = seed.generate(&mut state);

        Query::new(state, query)
    }

    /// Return an iterator over `CreateTableStatement`s for all the tables in the schema
    pub fn into_ddl(self) -> impl Iterator<Item = CreateTableStatement> {
        self.tables.into_values().map(|tbl| tbl.into())
    }

    /// Return an iterator over clones of `CreateTableStatement`s for all the tables in the schema
    pub fn ddl(&self) -> impl Iterator<Item = CreateTableStatement> + '_ {
        self.tables.values().map(|tbl| tbl.clone().into())
    }

    /// Generate `num_rows` rows of data for the table given by `table_name`.
    /// If `random` is passed on column data will be random in length for
    /// variable length data, and value for fixed-length data.
    ///
    /// # Panics
    ///
    /// Panics if `table_name` is not a known table
    pub fn generate_data_for_table(
        &mut self,
        table_name: &TableName,
        num_rows: usize,
        random: bool,
    ) -> Vec<HashMap<ColumnName, DfValue>> {
        self.tables
            .get_mut(table_name)
            .unwrap()
            .generate_data(num_rows, random)
    }

    /// Get a reference to the generator state's tables.
    pub fn tables(&self) -> &HashMap<TableName, TableSpec> {
        &self.tables
    }

    /// Get a mutable reference to the generator state's tables.
    pub fn tables_mut(&mut self) -> &mut HashMap<TableName, TableSpec> {
        &mut self.tables
    }
}

impl From<Vec<CreateTableStatement>> for GeneratorState {
    fn from(stmts: Vec<CreateTableStatement>) -> Self {
        GeneratorState {
            tables: stmts
                .into_iter()
                .map(|stmt| (stmt.table.name.clone().into(), stmt.into()))
                .collect(),
            ..Default::default()
        }
    }
}

pub struct QueryParameter {
    table_name: TableName,
    column_name: ColumnName,
    /// Index of this parameter in the list of parameters with the same table and column name, if
    /// any. This value is used when generating values for query parameters to generate multiple
    /// values when the same column appears in multiple parameters
    index: Option<u32>,
    generator: Arc<Mutex<ColumnGenerator>>,
}

pub struct QueryState<'a> {
    gen: &'a mut GeneratorState,
    tables: HashSet<TableName>,
    parameters: Vec<QueryParameter>,
    unique_parameters: HashMap<TableName, Vec<(ColumnName, DfValue)>>,
    alias_counter: u32,
    value_counter: u8,
}

impl<'a> QueryState<'a> {
    pub fn new(gen: &'a mut GeneratorState) -> Self {
        Self {
            gen,
            tables: HashSet::new(),
            unique_parameters: HashMap::new(),
            parameters: Vec::new(),
            alias_counter: 0,
            value_counter: 0,
        }
    }

    /// Returns the next placeholder that will be used according to the configured parameter mode
    pub fn next_placeholder(&self) -> ItemPlaceholder {
        match self.gen.parameter_mode {
            ParameterMode::Positional => ItemPlaceholder::QuestionMark,
            ParameterMode::Numbered => {
                ItemPlaceholder::DollarNumber((self.parameters.len() + 1).try_into().unwrap())
            }
        }
    }

    /// Generate a new, unique column alias for the query
    pub fn fresh_alias(&mut self) -> SqlIdentifier {
        self.alias_counter += 1;
        format!("alias_{}", self.alias_counter).into()
    }

    /// Return a mutable reference to *some* table in the schema - the implication being that the
    /// caller doesn't care which table
    pub fn some_table_mut(&mut self) -> &mut TableSpec {
        if let Some(table) = self.tables.iter().last() {
            self.gen.table_mut(table).unwrap()
        } else {
            let table = self.gen.some_table_mut();
            self.tables.insert(table.name.clone());
            table
        }
    }

    /// Returns a mutable reference to some table referenced in the given query.
    ///
    /// Adds a table to the query if none exist
    pub fn some_table_in_query_mut<'b>(
        &'b mut self,
        query: &mut SelectStatement,
    ) -> &'b mut TableSpec {
        match query
            .tables
            .iter()
            .chain(query.join.iter().filter_map(|jc| match &jc.right {
                JoinRightSide::Table(tbl) => Some(tbl),
                _ => None,
            }))
            .filter_map(|te| te.inner.as_table())
            .next()
        {
            Some(tbl) => self.gen.table_mut(tbl.name.as_str()).unwrap(),
            None => {
                let table = self.some_table_mut();
                query.tables.push(TableExpr {
                    inner: TableExprInner::Table(table.name.clone().into()),
                    alias: None,
                });
                table
            }
        }
    }

    /// Returns a mutable reference to some table *not* referenced in the given query
    pub fn some_table_not_in_query_mut<'b>(
        &'b mut self,
        query: &SelectStatement,
    ) -> &'b mut TableSpec {
        let tables_in_query = outermost_table_exprs(query)
            .map(|tbl| {
                tbl.alias
                    .as_ref()
                    .or_else(|| tbl.inner.as_table().map(|t| &t.name))
                    .unwrap()
            })
            .collect::<HashSet<_>>();
        if let Some(table) = self
            .tables
            .iter()
            .find(|tbl| !tables_in_query.contains(&tbl.0))
        {
            self.gen.table_mut(table).unwrap()
        } else {
            self.fresh_table_mut()
        }
    }

    /// Create a new, unique, empty table, and return a mutable reference to that table
    pub fn fresh_table_mut(&mut self) -> &mut TableSpec {
        let table = self.gen.fresh_table_mut();
        self.tables.insert(table.name.clone());
        table
    }

    /// Generate `rows_per_table` rows of data for all the tables referenced in the query for this
    /// QueryState.
    ///
    /// If `make_unique` is true and `make_unique_key` was previously called, the returned rows
    /// are modified to match the key returned by `make_unique_key`.
    pub fn generate_data(
        &mut self,
        rows_per_table: usize,
        make_unique: bool,
        random: bool,
    ) -> HashMap<TableName, Vec<HashMap<ColumnName, DfValue>>> {
        let table_names = self.tables.clone();
        table_names
            .iter()
            .map(|table_name| {
                let mut rows = self
                    .gen
                    .generate_data_for_table(table_name, rows_per_table, random);
                if make_unique {
                    if let Some(column_data) = self.unique_parameters.get(table_name) {
                        for row in &mut rows {
                            for (column, data) in column_data {
                                row.insert(column.clone(), data.clone());
                            }
                        }
                    }
                }
                (table_name.clone(), rows)
            })
            .collect()
    }

    /// Record a new (positional) parameter for the query, comparing against the given column of the
    /// given table
    pub fn add_parameter(&mut self, table_name: TableName, column_name: ColumnName) {
        let col_type = self.gen.table(&table_name).unwrap().columns[&column_name]
            .sql_type
            .clone();
        self.parameters.push(QueryParameter {
            table_name,
            column_name,
            index: None,
            generator: Arc::new(Mutex::new(ColumnGenerator::Constant(col_type.into()))),
        })
    }

    /// Record a new (positional) parameter for the query, comparing against the given column
    /// of the given table, and with the given value recorded for the key.
    ///
    /// It is the responsibility of the caller to ensure, by calling methods like
    /// [`TableSpec::set_column_generator_spec`], that the value given will match rows at least some
    /// of the time.
    pub fn add_parameter_with_value<V>(
        &mut self,
        table_name: TableName,
        column_name: ColumnName,
        value: V,
    ) where
        DfValue: From<V>,
    {
        self.parameters.push(QueryParameter {
            table_name,
            column_name,
            index: None,
            generator: Arc::new(Mutex::new(ColumnGenerator::Constant(
                DfValue::from(value).into(),
            ))),
        })
    }

    /// Record a new (positional) parameter for the query, comparing against the given column of the
    /// given table, and with the given *index*, used to distinguish between duplicate instances of
    /// the same parameter in the query.
    pub fn add_parameter_with_index(
        &mut self,
        table_name: TableName,
        column_name: ColumnName,
        index: u32,
    ) {
        let table = self.gen.table_mut(&table_name).unwrap();
        let sql_type = table.columns[&column_name].sql_type.clone();
        let val = unique_value_of_type(&sql_type, index);
        table.expect_value(column_name.clone(), val);

        self.parameters.push(QueryParameter {
            table_name,
            column_name,
            index: Some(index),
            generator: Arc::new(Mutex::new(ColumnGenerator::Unique(sql_type.into()))),
        });
    }

    /// Make a new, unique key for all the parameters in the query.
    ///
    /// To get data that matches this key, call `generate_data()` after calling this function.
    pub fn make_unique_key(&mut self) -> Vec<DfValue> {
        let mut ret = Vec::with_capacity(self.parameters.len());
        for QueryParameter {
            table_name,
            column_name,
            ..
        } in self.parameters.iter()
        {
            let val = unique_value_of_type(
                &self.gen.tables[table_name].columns[column_name].sql_type,
                self.value_counter as u32,
            );
            self.unique_parameters
                .entry(table_name.clone())
                .or_default()
                .push((column_name.clone(), val.clone()));
            self.value_counter += 1;
            ret.push(val);
        }
        ret
    }

    /// Returns a lookup key for the parameters in the query that will return results
    pub fn key(&self) -> Vec<DfValue> {
        self.parameters
            .iter()
            .map(
                |QueryParameter {
                     table_name,
                     column_name,
                     index,
                     generator,
                 }| {
                    let sql_type = &self.gen.tables[table_name].columns[column_name].sql_type;
                    match index {
                        Some(idx) => unique_value_of_type(sql_type, *idx),
                        None => generator.lock().gen(),
                    }
                },
            )
            .collect()
    }
}

pub struct Query<'gen> {
    pub state: QueryState<'gen>,
    pub statement: SelectStatement,
}

impl<'gen> Query<'gen> {
    pub fn new(state: QueryState<'gen>, statement: SelectStatement) -> Self {
        Self { state, statement }
    }
}

fn min_max_arg_type(dialect: ParseDialect) -> impl Strategy<Value = SqlType> {
    match dialect {
        ParseDialect::MySQL => any_with::<SqlType>(SqlTypeArbitraryOptions {
            generate_arrays: false,
            dialect: Some(dialect),
            ..Default::default()
        })
        .boxed(),
        ParseDialect::PostgreSQL => arbitrary_postgres_min_max_arg_type().boxed(),
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize, Arbitrary)]
#[arbitrary(args = QueryDialect)]
pub enum AggregateType {
    Count {
        #[any(generate_arrays = false, dialect = Some(args_shared.0))]
        column_type: SqlType,
        distinct: bool,
    },
    Sum {
        #[strategy(arbitrary_numeric_type(Some(args.0)))]
        column_type: SqlType,
        distinct: bool,
    },
    Avg {
        #[strategy(arbitrary_numeric_type(Some(args.0)))]
        column_type: SqlType,
        distinct: bool,
    },
    #[weight(u32::from(*args_shared == ParseDialect::MySQL))]
    GroupConcat,
    Max {
        #[strategy(min_max_arg_type(args.0))]
        column_type: SqlType,
    },
    Min {
        #[strategy(min_max_arg_type(args.0))]
        column_type: SqlType,
    },
}

impl AggregateType {
    pub fn column_type(&self) -> SqlType {
        match self {
            AggregateType::Count { column_type, .. } => column_type.clone(),
            AggregateType::Sum { column_type, .. } => column_type.clone(),
            AggregateType::Avg { column_type, .. } => column_type.clone(),
            AggregateType::GroupConcat => SqlType::Text,
            AggregateType::Max { column_type } => column_type.clone(),
            AggregateType::Min { column_type } => column_type.clone(),
        }
    }

    pub fn is_distinct(&self) -> bool {
        match self {
            AggregateType::Count { distinct, .. } => *distinct,
            AggregateType::Sum { distinct, .. } => *distinct,
            _ => false,
        }
    }
}

/// Parameters for generating an arbitrary FilterRhs
#[derive(Clone)]
pub struct FilterRhsArgs {
    column_type: SqlType,
}

impl Default for FilterRhsArgs {
    fn default() -> Self {
        Self {
            column_type: SqlType::Int(None),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize, Arbitrary)]
#[arbitrary(args = FilterRhsArgs)]
pub enum FilterRHS {
    Constant(#[strategy(Literal::arbitrary_with_type(&args.column_type))] Literal),
    Column,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, EnumIter, Serialize, Deserialize, Arbitrary)]
pub enum LogicalOp {
    And,
    Or,
}

impl From<LogicalOp> for BinaryOperator {
    fn from(op: LogicalOp) -> Self {
        match op {
            LogicalOp::And => BinaryOperator::And,
            LogicalOp::Or => BinaryOperator::Or,
        }
    }
}

fn filter_op(ty: &SqlType) -> impl Strategy<Value = BinaryOperator> {
    use BinaryOperator::*;
    let mut variants = vec![Equal, NotEqual, Greater, GreaterOrEqual, Less, LessOrEqual];

    if ty.is_any_text() {
        variants.extend([Like, NotLike]);
    }

    proptest::sample::select(variants)
}

/// An individual filter operation
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize, Arbitrary)]
#[arbitrary(args = FilterRhsArgs)]
pub enum FilterOp {
    /// Compare a column with either another column, or a value
    Comparison {
        #[strategy(filter_op(&args.column_type))]
        op: BinaryOperator,

        #[strategy(any_with::<FilterRHS>((*args).clone()))]
        rhs: FilterRHS,
    },

    /// A BETWEEN comparison on a column and two values
    Between {
        negated: bool,

        #[strategy(any_with::<FilterRHS>((*args).clone()))]
        min: FilterRHS,

        #[strategy(any_with::<FilterRHS>((*args).clone()))]
        max: FilterRHS,
    },

    /// An IS NULL comparison on a column
    IsNull { negated: bool },
}

/// A full representation of a filter to be added to a query
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct Filter {
    /// How to add the filter to the WHERE clause of the query
    pub extend_where_with: LogicalOp,

    /// The actual filter operation to add
    pub operation: FilterOp,

    /// The type of the column that's being filtered on
    pub column_type: SqlType,
}

impl Arbitrary for Filter {
    type Parameters = QueryDialect;

    type Strategy = BoxedStrategy<Filter>;

    fn arbitrary_with(dialect: Self::Parameters) -> Self::Strategy {
        (
            any_with::<SqlType>(SqlTypeArbitraryOptions {
                generate_arrays: false, // TODO: Set to true once we're targeting Postgres as well
                generate_other: false,
                // PG's json doesn't support comparison operators
                generate_json: dialect == ParseDialect::MySQL,
                generate_unsupported: false,
                dialect: Some(dialect.0),
            }),
            any::<LogicalOp>(),
        )
            .prop_flat_map(|(column_type, extend_where_with)| {
                any_with::<FilterOp>(FilterRhsArgs {
                    column_type: column_type.clone(),
                })
                .prop_map(move |operation| Self {
                    column_type: column_type.clone(),
                    operation,
                    extend_where_with,
                })
            })
            .boxed()
    }
}

impl Filter {
    fn all_with_operator(operator: BinaryOperator) -> impl Iterator<Item = Self> {
        ALL_FILTER_RHS
            .iter()
            .cloned()
            .cartesian_product(LogicalOp::iter())
            .map(move |(rhs, extend_where_with)| Self {
                operation: FilterOp::Comparison { op: operator, rhs },
                extend_where_with,
                column_type: SqlType::Int(None),
            })
    }
}

// The names of the built-in functions we can generate for use in a project expression
#[derive(Debug, Eq, PartialEq, Clone, Copy, EnumIter, Serialize, Deserialize)]
pub enum BuiltinFunction {
    ConvertTZ,
    DayOfWeek,
    IfNull,
    Month,
    Timediff,
    Addtime,
    Round,
}

impl Arbitrary for BuiltinFunction {
    type Parameters = QueryDialect;
    type Strategy = Select<BuiltinFunction>;

    fn arbitrary_with(dialect: Self::Parameters) -> Self::Strategy {
        use BuiltinFunction::*;

        let mut variants = vec![Round];
        if dialect == ParseDialect::MySQL {
            variants.extend([
                ConvertTZ, DayOfWeek, IfNull, Month, Timediff, Addtime, Round,
            ])
        }

        proptest::sample::select(variants)
    }
}

/// A representation for where in a query a subquery is located
///
/// When we support them, subqueries in `IN` clauses should go here as well
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize, Arbitrary)]
#[arbitrary(args = QueryDialect)]
pub enum SubqueryPosition {
    Cte(JoinOperator),
    Join(JoinOperator),
    /// TODO, once we support them:
    ///
    /// - `extend_where_with: LogicalOp`
    /// - `negated: bool`
    Exists {
        /// If correlated, contains the type of the column that is compared
        #[strategy(proptest::option::of(any_with::<SqlType>(SqlTypeArbitraryOptions {
            generate_arrays: false,
            dialect: Some(args.0),
            ..Default::default()
        })))]
        correlated: Option<SqlType>,
    },
}

/// Parameters for generating an arbitrary [`QueryOperation`]
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct QueryOperationArgs {
    pub dialect: QueryDialect,
}

/// Operations that can be performed as part of a SQL query
///
/// Members of this enum represent some sense of an individual operation that can be performed on an
/// arbitrary SQL query. Each operation knows how to add itself to a given SQL query (via
/// [`add_to_query`](QueryOperation::add_to_query)) with the aid of a mutable reference to a
/// [`GeneratorState`].
///
/// Some operations are parameterized on fields that, due to having too large of a state space to
/// enumerate exhaustively, are hardcoded when query operations are built from a user-supplied
/// string on the command-line (via [`Operations`]), and can only be changed when generating queries
/// randomly via the proptest [`Arbitrary`] implementation. See [this design doc][0] for more
/// information
///
/// Note that not every operation that ReadySet supports is currently included in this enum -
/// planned for the future are:
///
/// - arithmetic projections
/// - union
/// - order by
/// - ilike
///
/// each of which should be relatively straightforward to add here.
///
/// [0]: https://docs.google.com/document/d/1rb-AU_PsH2Z40XFLjmLP7DcyeJzlwKI4Aa-GQgEoWKA
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize, Arbitrary)]
#[arbitrary(args = QueryOperationArgs)]
pub enum QueryOperation {
    ColumnAggregate(#[any(args_shared.dialect)] AggregateType),
    Filter(#[any(args_shared.dialect)] Filter),
    Distinct,
    Join(JoinOperator),
    ProjectLiteral,
    SingleParameter,
    MultipleParameters,
    InParameter {
        #[strategy(1..=100u8)]
        num_values: u8,
    },
    RangeParameter,
    MultipleRangeParameters,
    ProjectBuiltinFunction(#[any(args_shared.dialect)] BuiltinFunction),
    TopK {
        order_type: OrderType,
        #[strategy(0..=100u64)]
        limit: u64,
    },
    Paginate {
        order_type: OrderType,
        #[strategy(0..=100u64)]
        limit: u64,
        #[strategy(0..=100u64)]
        page_number: u64,
    },
    #[weight(0)]
    Subquery(SubqueryPosition),
}

const ALL_FILTER_RHS: &[FilterRHS] = &[FilterRHS::Column, FilterRHS::Constant(Literal::Integer(1))];

const COMPARISON_OPS: &[BinaryOperator] = &[
    BinaryOperator::Equal,
    BinaryOperator::NotEqual,
    BinaryOperator::Greater,
    BinaryOperator::GreaterOrEqual,
    BinaryOperator::Less,
    BinaryOperator::LessOrEqual,
];

const JOIN_OPERATORS: &[JoinOperator] = &[
    JoinOperator::LeftJoin,
    JoinOperator::LeftOuterJoin,
    JoinOperator::InnerJoin,
];

const DEFAULT_LIMIT: u64 = 3;

const ALL_TOPK: &[QueryOperation] = &[
    QueryOperation::TopK {
        order_type: OrderType::OrderAscending,
        limit: DEFAULT_LIMIT,
    },
    QueryOperation::TopK {
        order_type: OrderType::OrderDescending,
        limit: DEFAULT_LIMIT,
    },
];

const ALL_PAGINATE: &[QueryOperation] = &[
    QueryOperation::Paginate {
        order_type: OrderType::OrderAscending,
        limit: DEFAULT_LIMIT,
        page_number: 0,
    },
    QueryOperation::Paginate {
        order_type: OrderType::OrderDescending,
        limit: DEFAULT_LIMIT,
        page_number: 0,
    },
    QueryOperation::Paginate {
        order_type: OrderType::OrderAscending,
        limit: DEFAULT_LIMIT,
        page_number: 1,
    },
    QueryOperation::Paginate {
        order_type: OrderType::OrderDescending,
        limit: DEFAULT_LIMIT,
        page_number: 1,
    },
];

const ALL_AGGREGATE_TYPES: &[AggregateType] = &[
    AggregateType::Count {
        column_type: SqlType::Int(None),
        distinct: true,
    },
    AggregateType::Count {
        column_type: SqlType::Int(None),
        distinct: false,
    },
    AggregateType::Sum {
        column_type: SqlType::Int(None),
        distinct: true,
    },
    AggregateType::Sum {
        column_type: SqlType::Int(None),
        distinct: false,
    },
    AggregateType::Avg {
        column_type: SqlType::Int(None),
        distinct: true,
    },
    AggregateType::Avg {
        column_type: SqlType::Int(None),
        distinct: false,
    },
    AggregateType::GroupConcat,
    AggregateType::Max {
        column_type: SqlType::Int(None),
    },
    AggregateType::Min {
        column_type: SqlType::Int(None),
    },
];

const ALL_SUBQUERY_POSITIONS: &[SubqueryPosition] = &[
    SubqueryPosition::Join(JoinOperator::InnerJoin),
    SubqueryPosition::Cte(JoinOperator::InnerJoin),
];

lazy_static! {
    static ref ALL_COMPARISON_FILTER_OPS: Vec<FilterOp> = {
        COMPARISON_OPS
            .iter()
            .cartesian_product(ALL_FILTER_RHS.iter().cloned())
            .map(|(operator, rhs)| FilterOp::Comparison {
                    op: *operator,
                    rhs,
                },
            )
            .collect()
    };

    static ref ALL_BETWEEN_OPS: Vec<FilterOp> = {
        [true, false]
            .into_iter()
            .map(|negated| {
                FilterOp::Between {
                    negated,
                    min: FilterRHS::Constant(Literal::Integer(1)),
                    max: FilterRHS::Constant(Literal::Integer(5))
                }
            })
            .collect()
    };

    static ref ALL_FILTER_OPS: Vec<FilterOp> = {
        ALL_COMPARISON_FILTER_OPS
            .iter()
            .cloned()
            .chain(ALL_BETWEEN_OPS.clone())
            .chain(iter::once(FilterOp::IsNull { negated: true }))
            .chain(iter::once(FilterOp::IsNull { negated: false }))
            .collect()
    };

    static ref ALL_FILTERS: Vec<Filter> = {
        ALL_FILTER_OPS
            .iter()
            .cloned()
            .cartesian_product(LogicalOp::iter())
            .map(|(operation, extend_where_with)| Filter {
                extend_where_with,
                operation,
                column_type: SqlType::Int(None)
            })
            .collect()
    };

    /// A list of all possible [`QueryOperation`]s
    pub static ref ALL_OPERATIONS: Vec<QueryOperation> = {
        ALL_AGGREGATE_TYPES
            .iter()
            .cloned()
            .map(QueryOperation::ColumnAggregate)
            .chain(iter::once(QueryOperation::Distinct))
            .chain(JOIN_OPERATORS.iter().cloned().map(QueryOperation::Join))
            .chain(iter::once(QueryOperation::ProjectLiteral))
            .chain(iter::once(QueryOperation::SingleParameter))
            .chain(iter::once(QueryOperation::InParameter { num_values: 3 }))
            .chain(BuiltinFunction::iter().map(QueryOperation::ProjectBuiltinFunction))
            .chain(ALL_TOPK.iter().cloned())
            .chain(ALL_SUBQUERY_POSITIONS.iter().cloned().map(QueryOperation::Subquery))
            .collect()
    };
}

fn extend_where(query: &mut SelectStatement, op: LogicalOp, cond: Expr) {
    query.where_clause = Some(match query.where_clause.take() {
        Some(existing_cond) => Expr::BinaryOp {
            op: op.into(),
            lhs: Box::new(existing_cond),
            rhs: Box::new(cond),
        },
        None => cond,
    })
}

fn and_where(query: &mut SelectStatement, cond: Expr) {
    extend_where(query, LogicalOp::And, cond)
}

fn query_has_aggregate(query: &SelectStatement) -> bool {
    query.fields.iter().any(|fde| {
        matches!(
            fde,
            FieldDefinitionExpr::Expr { expr, .. } if contains_aggregate(expr),
        )
    })
}

fn column_in_query_filtered<T, F>(
    state: &mut QueryState<'_>,
    query: &mut SelectStatement,
    default_type: T,
    mut filter: F,
) -> Column
where
    F: FnMut(TableName, &ColumnName, &ColumnSpec) -> bool,
    T: FnOnce() -> SqlType,
{
    match query
        .tables
        .iter()
        .chain(query.join.iter().filter_map(|jc| match &jc.right {
            JoinRightSide::Table(tbl) => Some(tbl),
            _ => None,
        }))
        .filter_map(|te| te.inner.as_table())
        .next()
    {
        Some(tbl) => {
            let column = state
                .gen
                .table_mut(tbl.name.as_str())
                .unwrap()
                .some_column_name_filtered(default_type, move |column_name, column_spec| {
                    filter(TableName(tbl.name.clone()), column_name, column_spec)
                });
            Column {
                name: column.into(),
                table: Some(tbl.clone()),
            }
        }
        None => {
            let table = state.some_table_mut();
            query
                .tables
                .push(TableExpr::from(Relation::from(table.name.clone())));
            let table_name = table.name.clone();
            let colname =
                table.some_column_name_filtered(default_type, move |column_name, column_spec| {
                    filter(table_name.clone(), column_name, column_spec)
                });
            Column {
                name: colname.into(),
                table: Some(table.name.clone().into()),
            }
        }
    }
}

fn column_in_query(state: &mut QueryState<'_>, query: &mut SelectStatement) -> Column {
    column_in_query_filtered(state, query, || SqlType::Int(None), |_, _, _| true)
}

fn parameter_column_in_query_filtered<F>(
    state: &mut QueryState<'_>,
    query: &mut SelectStatement,
    mut filter: F,
) -> Column
where
    F: FnMut(TableName, &ColumnName, &ColumnSpec) -> bool,
{
    let existing_parameters = state
        .parameters
        .iter()
        .map(|param| (param.table_name.clone(), param.column_name.clone()))
        .collect::<HashSet<_>>();
    column_in_query_filtered(
        state,
        query,
        || SqlType::Int(None), /* TODO: generate this! */
        |table_name, column_name, col| {
            !existing_parameters.contains(&(table_name.clone(), column_name.clone()))
                && !matches!(
                    col.sql_type,
                    SqlType::Bool | SqlType::Array(_) | SqlType::Other(_)
                )
                && filter(table_name, column_name, col)
        },
    )
}

fn parameter_column_in_query(state: &mut QueryState<'_>, query: &mut SelectStatement) -> Column {
    parameter_column_in_query_filtered(state, query, |_, _, _| true)
}

impl QueryOperation {
    /// Returns true if this query operation is supported inside of subqueries. If this function
    /// returns false, `add_to_query` will not be called on this query operation when adding it to a
    /// subquery.
    fn supported_in_subqueries(&self) -> bool {
        // We don't currently support query parameters in subqueries
        !matches!(
            self,
            QueryOperation::MultipleParameters
                | QueryOperation::SingleParameter
                | QueryOperation::InParameter { .. }
                | QueryOperation::RangeParameter
                | QueryOperation::MultipleRangeParameters
                | QueryOperation::Paginate { .. }
        )
    }

    /// Add this query operation to `query`, recording information about new tables and columns in
    /// `state`.
    fn add_to_query(&self, state: &mut QueryState<'_>, query: &mut SelectStatement) {
        match self {
            QueryOperation::ColumnAggregate(agg) => {
                use AggregateType::*;

                let alias = state.fresh_alias();
                let tbl = state.some_table_in_query_mut(query);

                if query.tables.is_empty() {
                    query
                        .tables
                        .push(TableExpr::from(Relation::from(tbl.name.clone())));
                }

                let col = tbl.fresh_column_with_type(agg.column_type());

                let expr = Box::new(Expr::Column(Column {
                    name: col.into(),
                    table: Some(tbl.name.clone().into()),
                }));

                let func = match *agg {
                    Count { distinct, .. } => FunctionExpr::Count { expr, distinct },
                    Sum { distinct, .. } => FunctionExpr::Sum { expr, distinct },
                    Avg { distinct, .. } => FunctionExpr::Avg { expr, distinct },
                    GroupConcat => FunctionExpr::GroupConcat {
                        expr,
                        separator: Some(", ".to_owned()),
                    },
                    Max { .. } => FunctionExpr::Max(expr),
                    Min { .. } => FunctionExpr::Min(expr),
                };

                query.fields.push(FieldDefinitionExpr::Expr {
                    alias: Some(alias),
                    expr: Expr::Call(func),
                });
            }

            QueryOperation::Filter(filter) => {
                let alias = state.fresh_alias();
                let tbl = state.some_table_in_query_mut(query);
                let col = tbl.some_column_with_type(filter.column_type.clone());

                if query.tables.is_empty() {
                    query
                        .tables
                        .push(TableExpr::from(Relation::from(tbl.name.0.as_str())));
                }

                let col_expr = Expr::Column(Column {
                    table: Some(Relation::from(tbl.name.0.as_str())),
                    ..col.clone().into()
                });

                query.fields.push(FieldDefinitionExpr::Expr {
                    expr: col_expr.clone(),
                    alias: Some(alias),
                });

                let mut filter_rhs_to_expr = |rhs: &FilterRHS| match rhs {
                    FilterRHS::Constant(val) => {
                        tbl.expect_value(
                            col.clone(),
                            val.clone()
                                .try_into_dialect(Dialect::DEFAULT_MYSQL.into())
                                .unwrap(),
                        );
                        Expr::Literal(val.clone())
                    }
                    FilterRHS::Column => {
                        let col = tbl
                            .some_column_with_type_different_than(filter.column_type.clone(), &col);
                        Expr::Column(Column {
                            table: Some(tbl.name.clone().into()),
                            ..col.into()
                        })
                    }
                };

                let cond = match &filter.operation {
                    FilterOp::Comparison { op, rhs } => Expr::BinaryOp {
                        op: *op,
                        lhs: Box::new(col_expr),
                        rhs: Box::new(filter_rhs_to_expr(rhs)),
                    },
                    FilterOp::Between { negated, min, max } => Expr::Between {
                        operand: Box::new(col_expr),
                        min: Box::new(filter_rhs_to_expr(min)),
                        max: Box::new(filter_rhs_to_expr(max)),
                        negated: *negated,
                    },
                    FilterOp::IsNull { negated } => {
                        tbl.expect_value(col, DfValue::None);
                        Expr::BinaryOp {
                            lhs: Box::new(col_expr),
                            op: if *negated {
                                BinaryOperator::Is
                            } else {
                                BinaryOperator::IsNot
                            },
                            rhs: Box::new(Expr::Literal(Literal::Null)),
                        }
                    }
                };

                extend_where(query, filter.extend_where_with, cond);
            }

            QueryOperation::Distinct => {
                query.distinct = true;
                if let Some(order) = &query.order {
                    for OrderBy { field, .. } in &order.order_by {
                        let expr = match field {
                            FieldReference::Numeric(_) => {
                                unreachable!(
                                    "We dont currently ever generate numeric field references"
                                )
                            }
                            FieldReference::Expr(expr) => expr.clone(),
                        };

                        query.fields.push(FieldDefinitionExpr::Expr {
                            expr,
                            alias: Some(state.fresh_alias()),
                        })
                    }
                }
            }

            QueryOperation::Join(operator) => {
                let left_table = state.some_table_in_query_mut(query);
                let left_table_name = left_table.name.clone();
                let left_join_key = left_table.some_column_with_type(SqlType::Int(None));
                let left_projected = left_table.fresh_column();

                if query.tables.is_empty() {
                    query
                        .tables
                        .push(TableExpr::from(Relation::from(left_table_name.clone())));
                }

                let right_table = state.fresh_table_mut();
                let right_table_name = right_table.name.clone();
                let right_join_key = right_table.some_column_with_type(SqlType::Int(None));
                let right_projected = right_table.fresh_column();

                query.join.push(JoinClause {
                    operator: *operator,
                    right: JoinRightSide::Table(TableExpr::from(Relation::from(
                        right_table.name.clone(),
                    ))),
                    constraint: JoinConstraint::On(Expr::BinaryOp {
                        op: BinaryOperator::Equal,
                        lhs: Box::new(Expr::Column(Column {
                            table: Some(left_table_name.clone().into()),
                            ..left_join_key.into()
                        })),
                        rhs: Box::new(Expr::Column(Column {
                            table: Some(right_table_name.clone().into()),
                            ..right_join_key.into()
                        })),
                    }),
                });

                query.fields.push(FieldDefinitionExpr::Expr {
                    expr: Expr::Column(Column {
                        table: Some(left_table_name.into()),
                        ..left_projected.into()
                    }),
                    alias: Some(state.fresh_alias()),
                });
                query.fields.push(FieldDefinitionExpr::Expr {
                    expr: Expr::Column(Column {
                        table: Some(right_table_name.into()),
                        ..right_projected.into()
                    }),
                    alias: Some(state.fresh_alias()),
                });
            }

            QueryOperation::ProjectLiteral => {
                let alias = state.fresh_alias();
                query.fields.push(FieldDefinitionExpr::Expr {
                    expr: Expr::Literal(Literal::Integer(1)),
                    alias: Some(alias),
                });
            }

            QueryOperation::SingleParameter => {
                let col = parameter_column_in_query(state, query);
                and_where(
                    query,
                    Expr::BinaryOp {
                        op: BinaryOperator::Equal,
                        lhs: Box::new(Expr::Column(col.clone())),
                        rhs: Box::new(Expr::Literal(Literal::Placeholder(
                            state.next_placeholder(),
                        ))),
                    },
                );
                state.add_parameter(col.table.unwrap().name.into(), col.name.into());
            }

            QueryOperation::MultipleParameters => {
                QueryOperation::SingleParameter.add_to_query(state, query);
                QueryOperation::SingleParameter.add_to_query(state, query);
            }

            QueryOperation::RangeParameter => {
                let col = parameter_column_in_query_filtered(state, query, |_, _, col| {
                    col.sql_type == SqlType::Int(None)
                });
                and_where(
                    query,
                    Expr::BinaryOp {
                        lhs: Box::new(Expr::Column(col.clone())),
                        op: BinaryOperator::Greater,
                        rhs: Box::new(Expr::Literal(Literal::Placeholder(
                            state.next_placeholder(),
                        ))),
                    },
                );
                state
                    .gen
                    .table_mut(&col.table.as_ref().unwrap().name)
                    .unwrap()
                    .set_column_generator_spec(
                        col.name.clone().into(),
                        ColumnGenerationSpec::Uniform(1i32.into(), 20i32.into()),
                    );
                state.add_parameter_with_value(
                    col.table.unwrap().name.into(),
                    col.name.into(),
                    10i32,
                );
            }

            QueryOperation::MultipleRangeParameters => {
                QueryOperation::RangeParameter.add_to_query(state, query);
                QueryOperation::RangeParameter.add_to_query(state, query);
            }

            QueryOperation::InParameter { num_values } => {
                let col = parameter_column_in_query(state, query);
                and_where(
                    query,
                    Expr::In {
                        lhs: Box::new(Expr::Column(col.clone())),
                        rhs: InValue::List(
                            (0..*num_values)
                                .map(|idx| {
                                    let p = Expr::Literal(Literal::Placeholder(
                                        state.next_placeholder(),
                                    ));

                                    state.add_parameter_with_index(
                                        col.table.clone().unwrap().name.into(),
                                        col.name.clone().into(),
                                        idx as _,
                                    );

                                    p
                                })
                                .collect(),
                        ),
                        negated: false,
                    },
                );
            }
            QueryOperation::ProjectBuiltinFunction(bif) => {
                macro_rules! add_builtin {
                    ($fname:ident($($arg:tt)*)) => {{
                        let table = state.some_table_in_query_mut(query);

                        if query.tables.is_empty() {
                            query.tables.push(TableExpr::from(Relation::from(table.name.clone())));
                        }

                        let mut arguments = Vec::new();
                        add_builtin!(@args_to_expr, table, arguments, $($arg)*);
                        let expr = Expr::Call(FunctionExpr::Call {
                            name: stringify!($fname).into(),
                            arguments: Some(arguments),
                        });
                        let alias = state.fresh_alias();
                        query.fields.push(FieldDefinitionExpr::Expr {
                            alias: Some(alias.clone()),
                            expr,
                        });
                    }};

                    (@args_to_expr, $table: ident, $out: ident, $(,)?) => {};

                    (@args_to_expr, $table: ident, $out:ident, $arg:literal, $($args: tt)*) => {{
                        $out.push(Expr::Literal($arg.into()));
                        add_builtin!(@args_to_expr, $table, $out, $($args)*);
                    }};
                    (@args_to_expr, $table: ident, $out:ident, $arg:literal) => {
                        add_builtin!(@args_to_expr, $table, $out, $arg,);
                    };

                    (@args_to_expr, $table: ident, $out:ident, $arg:expr, $($args: tt)*) => {{
                        $out.push(Expr::Column(
                            Column {
                                table: Some($table.name.clone().into()),
                                ..$table.some_column_with_type($arg).into()
                            }
                        ));
                        add_builtin!(@args_to_expr, $table, $out, $($args)*);
                    }};
                    (@args_to_expr, $table: ident, $out:ident, $arg:expr) => {{
                        add_builtin!(@args_to_expr, $table, $out, $arg,);
                    }};
                }

                match bif {
                    BuiltinFunction::ConvertTZ => {
                        add_builtin!(convert_tz(SqlType::Timestamp, "America/New_York", "UTC"))
                    }
                    BuiltinFunction::DayOfWeek => add_builtin!(dayofweek(SqlType::Date)),
                    BuiltinFunction::IfNull => add_builtin!(ifnull(SqlType::Text, SqlType::Text)),
                    BuiltinFunction::Month => add_builtin!(month(SqlType::Date)),
                    BuiltinFunction::Timediff => {
                        add_builtin!(timediff(SqlType::Time, SqlType::Time))
                    }
                    BuiltinFunction::Addtime => add_builtin!(addtime(SqlType::Time, SqlType::Time)),
                    BuiltinFunction::Round => add_builtin!(round(SqlType::Real)),
                }
            }
            QueryOperation::TopK { order_type, limit } => {
                let table = state.some_table_in_query_mut(query);

                if query.tables.is_empty() {
                    query
                        .tables
                        .push(TableExpr::from(Relation::from(table.name.clone())));
                }

                let column_name = table.some_column_name();
                let column = Column {
                    table: Some(table.name.clone().into()),
                    ..column_name.into()
                };
                query.order = Some(OrderClause {
                    order_by: vec![OrderBy {
                        field: FieldReference::Expr(Expr::Column(column.clone())),
                        order_type: Some(*order_type),
                        null_order: None,
                    }],
                });

                query.limit_clause = LimitClause::LimitOffset {
                    limit: Some(LimitValue::Literal(Literal::Integer(*limit as _))),
                    offset: None,
                };

                if query.distinct {
                    query.fields.push(FieldDefinitionExpr::Expr {
                        expr: Expr::Column(column),
                        alias: Some(state.fresh_alias()),
                    })
                }
            }
            QueryOperation::Paginate {
                order_type,
                limit,
                page_number,
            } => {
                let table = state.some_table_in_query_mut(query);

                if query.tables.is_empty() {
                    query
                        .tables
                        .push(TableExpr::from(Relation::from(table.name.clone())));
                }

                let column_name = table.some_column_name();
                let column = Column {
                    table: Some(table.name.clone().into()),
                    ..column_name.into()
                };
                query.order = Some(OrderClause {
                    order_by: vec![OrderBy {
                        field: FieldReference::Expr(Expr::Column(column.clone())),
                        order_type: Some(*order_type),
                        null_order: None,
                    }],
                });

                // Since we are setting both fields, check first to see what kind of syntax
                // we were using.
                if matches!(query.limit_clause, LimitClause::OffsetCommaLimit { .. }) {
                    query.limit_clause = LimitClause::OffsetCommaLimit {
                        limit: LimitValue::Literal(Literal::Integer(*limit as _)),
                        offset: Literal::Integer((*limit * *page_number) as _),
                    }
                } else {
                    query.limit_clause = LimitClause::LimitOffset {
                        limit: Some(LimitValue::Literal(Literal::Integer(*limit as _))),
                        offset: Some(Literal::Integer((*limit * *page_number) as _)),
                    };
                }

                if query.distinct {
                    query.fields.push(FieldDefinitionExpr::Expr {
                        expr: Expr::Column(column),
                        alias: Some(state.fresh_alias()),
                    })
                }
            }
            // Subqueries are turned into QuerySeed::subqueries as part of
            // GeneratorOps::into_query_seeds
            QueryOperation::Subquery(_) => {}
        }
    }

    /// Returns an iterator over all permutations of length 1..`max_depth` [`QueryOperation`]s.
    pub fn permute(max_depth: usize) -> impl Iterator<Item = Vec<&'static QueryOperation>> {
        (1..=max_depth).flat_map(|depth| ALL_OPERATIONS.iter().combinations(depth))
    }
}

/// Representation of a subset of query operations
///
/// Operations can be converted from a user-supplied string using [`FromStr::from_str`], which
/// supports the following speccifications:
///
/// | Specification                           | Meaning                                 |
/// |-----------------------------------------|-----------------------------------------|
/// | aggregates                              | All [`AggregateType`]s                  |
/// | count                                   | COUNT aggregates                        |
/// | count_distinct                          | COUNT(DISTINCT) aggregates              |
/// | sum                                     | SUM aggregates                          |
/// | sum_distinct                            | SUM(DISTINCT) aggregates                |
/// | avg                                     | AVG aggregates                          |
/// | avg_distinct                            | AVG(DISTINCT) aggregates                |
/// | group_concat                            | GROUP_CONCAT aggregates                 |
/// | max                                     | MAX aggregates                          |
/// | min                                     | MIN aggregates                          |
/// | filters                                 | All constant-valued [`Filter`]s         |
/// | equal_filters                           | Constant-valued `=` filters             |
/// | not_equal_filters                       | Constant-valued `!=` filters            |
/// | greater_filters                         | Constant-valued `>` filters             |
/// | greater_or_equal_filters                | Constant-valued `>=` filters            |
/// | less_filters                            | Constant-valued `<` filters             |
/// | less_or_equal_filters                   | Constant-valued `<=` filters            |
/// | between_filters                         | Constant-valued `BETWEEN` filters       |
/// | is_null_filters                         | IS NULL and IS NOT NULL filters         |
/// | distinct                                | `SELECT DISTINCT`                       |
/// | joins                                   | Joins, with all [`JoinOperator`]s       |
/// | inner_join                              | `INNER JOIN`s                           |
/// | left_join                               | `LEFT JOIN`s                            |
/// | single_parameter / single_param / param | A single query parameter                |
/// | range_param                             | A range query parameter                 |
/// | multiple_parameters / params            | Multiple query parameters               |
/// | multiple_range_params                   | Multiple range query parameters         |
/// | in_parameter                            | IN with multiple query parameters       |
/// | project_literal                         | A projected literal value               |
/// | project_builtin                         | Project a built-in function             |
/// | subqueries                              | All subqueries                          |
/// | cte                                     | CTEs (WITH statements)                  |
/// | join_subquery                           | JOIN to a subquery directly             |
/// | topk                                    | ORDER BY combined with LIMIT            |
/// | paginate                                | ORDER BY combined with LIMIT and OFFSET |
/// | exists                                  | EXISTS with a subquery                  |
#[repr(transparent)]
#[derive(Debug, PartialEq, Eq, Clone, From, Into)]
pub struct Operations(pub Vec<QueryOperation>);

impl FromStr for Operations {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use QueryOperation::*;

        match s {
            "aggregates" => Ok(ALL_AGGREGATE_TYPES
                .iter()
                .cloned()
                .map(ColumnAggregate)
                .collect()),
            "count" => Ok(vec![ColumnAggregate(AggregateType::Count {
                column_type: SqlType::Int(None),
                distinct: false,
            })]
            .into()),
            "count_distinct" => Ok(vec![ColumnAggregate(AggregateType::Count {
                column_type: SqlType::Int(None),
                distinct: true,
            })]
            .into()),
            "sum" => Ok(vec![ColumnAggregate(AggregateType::Sum {
                column_type: SqlType::Int(None),
                distinct: false,
            })]
            .into()),
            "sum_distinct" => Ok(vec![ColumnAggregate(AggregateType::Sum {
                column_type: SqlType::Int(None),
                distinct: true,
            })]
            .into()),
            "avg" => Ok(vec![ColumnAggregate(AggregateType::Avg {
                column_type: SqlType::Int(None),
                distinct: false,
            })]
            .into()),
            "avg_distinct" => Ok(vec![ColumnAggregate(AggregateType::Avg {
                column_type: SqlType::Int(None),
                distinct: true,
            })]
            .into()),
            "group_concat" => Ok(vec![ColumnAggregate(AggregateType::GroupConcat)].into()),
            "max" => Ok(vec![ColumnAggregate(AggregateType::Max {
                column_type: SqlType::Int(None),
            })]
            .into()),
            "min" => Ok(vec![ColumnAggregate(AggregateType::Min {
                column_type: SqlType::Int(None),
            })]
            .into()),
            "filters" => Ok(ALL_FILTERS.iter().cloned().map(Filter).collect()),
            "equal_filters" => Ok(crate::Filter::all_with_operator(BinaryOperator::Equal)
                .map(Filter)
                .collect()),
            "not_equal_filters" => Ok(crate::Filter::all_with_operator(BinaryOperator::NotEqual)
                .map(Filter)
                .collect()),
            "greater_filters" => Ok(crate::Filter::all_with_operator(BinaryOperator::Greater)
                .map(Filter)
                .collect()),
            "greater_or_equal_filters" => Ok(crate::Filter::all_with_operator(
                BinaryOperator::GreaterOrEqual,
            )
            .map(Filter)
            .collect()),
            "less_filters" => Ok(crate::Filter::all_with_operator(BinaryOperator::Less)
                .map(Filter)
                .collect()),
            "less_or_equal_filters" => Ok(crate::Filter::all_with_operator(
                BinaryOperator::LessOrEqual,
            )
            .map(Filter)
            .collect()),
            "between_filters" => Ok(LogicalOp::iter()
                .cartesian_product(ALL_BETWEEN_OPS.clone())
                .map(|(extend_where_with, operation)| crate::Filter {
                    extend_where_with,
                    operation,

                    column_type: SqlType::Int(None),
                })
                .map(Filter)
                .collect()),
            "is_null_filters" => Ok(LogicalOp::iter()
                .cartesian_product(
                    iter::once(FilterOp::IsNull { negated: true })
                        .chain(iter::once(FilterOp::IsNull { negated: false })),
                )
                .map(|(extend_where_with, operation)| crate::Filter {
                    extend_where_with,
                    operation,
                    column_type: SqlType::Int(None),
                })
                .map(Filter)
                .collect()),
            "distinct" => Ok(vec![Distinct].into()),
            "joins" => Ok(JOIN_OPERATORS.iter().cloned().map(Join).collect()),
            "inner_join" => Ok(vec![Join(JoinOperator::InnerJoin)].into()),
            "left_join" => Ok(vec![Join(JoinOperator::LeftJoin)].into()),
            "single_parameter" | "single_param" | "param" => Ok(vec![SingleParameter].into()),
            "multiple_parameters" | "params" => Ok(vec![MultipleParameters].into()),
            "range_param" => Ok(vec![RangeParameter].into()),
            "multiple_range_params" => Ok(vec![MultipleRangeParameters].into()),
            "in_parameter" => Ok(vec![InParameter { num_values: 3 }].into()),
            "project_literal" => Ok(vec![ProjectLiteral].into()),
            "project_builtin" => Ok(BuiltinFunction::iter()
                .map(ProjectBuiltinFunction)
                .collect()),
            "subqueries" => Ok(ALL_SUBQUERY_POSITIONS
                .iter()
                .cloned()
                .map(Subquery)
                .collect()),
            "cte" => Ok(vec![Subquery(SubqueryPosition::Cte(JoinOperator::InnerJoin))].into()),
            "join_subquery" => {
                Ok(vec![Subquery(SubqueryPosition::Join(JoinOperator::InnerJoin))].into())
            }
            "exists" => Ok(vec![
                Subquery(SubqueryPosition::Exists { correlated: None }),
                Subquery(SubqueryPosition::Exists {
                    correlated: Some(SqlType::Int(None)),
                }),
            ]
            .into()),
            "topk" => Ok(ALL_TOPK.to_vec().into()),
            "paginate" => Ok(ALL_PAGINATE.to_vec().into()),
            s => Err(anyhow!("unknown query operation: {}", s)),
        }
    }
}

impl FromIterator<QueryOperation> for Operations {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = QueryOperation>,
    {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for Operations {
    type Item = QueryOperation;

    type IntoIter = <Vec<QueryOperation> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Operations {
    type Item = &'a QueryOperation;

    type IntoIter = <&'a Vec<QueryOperation> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Arbitrary for Operations {
    type Parameters = <Vec<QueryOperation> as Arbitrary>::Parameters;

    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(args: Self::Parameters) -> Self::Strategy {
        any_with::<Vec<QueryOperation>>(args)
            .prop_map(|mut ops| {
                // Don't generate an aggregate with distinct keyword or a plain distinct
                // in the same query as a WHERE IN clause,
                // since we don't support those queries (ENG-2942)
                let mut distinct_found = false;
                let mut in_parameter_found = false;

                // Don't generate an OR filter in the same query as a parameter of any kind, since
                // we don't support those queries (ENG-2976)
                let mut parameter_found = false;
                let mut or_filter_found = false;

                ops.retain(|op| match op {
                    QueryOperation::ColumnAggregate(agg) if agg.is_distinct() => {
                        if in_parameter_found {
                            false
                        } else {
                            distinct_found = true;
                            true
                        }
                    }
                    QueryOperation::Distinct => {
                        if in_parameter_found {
                            false
                        } else {
                            distinct_found = true;
                            true
                        }
                    }
                    QueryOperation::InParameter { .. } => {
                        if distinct_found | or_filter_found {
                            false
                        } else {
                            in_parameter_found = true;
                            parameter_found = true;
                            true
                        }
                    }
                    QueryOperation::SingleParameter
                    | QueryOperation::MultipleParameters
                    | QueryOperation::RangeParameter
                    | QueryOperation::MultipleRangeParameters => {
                        if or_filter_found {
                            false
                        } else {
                            parameter_found = true;
                            true
                        }
                    }
                    QueryOperation::Filter(Filter {
                        extend_where_with: LogicalOp::Or,
                        ..
                    }) => {
                        if parameter_found {
                            false
                        } else {
                            or_filter_found = true;
                            true
                        }
                    }
                    _ => true,
                });
                Operations(ops)
            })
            .boxed()
    }
}

/// Representation of a list of subsets of query operations, as specified by the user on the command
/// line.
///
/// `OperationList` can be converted from a (user-supplied) string using [`FromStr::from_str`],
/// using a comma-separated list of [`Operations`]
#[repr(transparent)]
#[derive(Clone)]
pub struct OperationList(pub Vec<Operations>);

impl FromStr for OperationList {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(
            s.split(',')
                .map(Operations::from_str)
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }
}

impl OperationList {
    /// Generate a set of permutations of all the sets of [`QueryOperation`]s represented by the
    /// [`Operations`] in this `OperationList`.
    pub fn permute(&self) -> impl Iterator<Item = Vec<QueryOperation>> + '_ {
        self.0
            .iter()
            .multi_cartesian_product()
            .map(|ops| ops.into_iter().cloned().collect())
    }
}

impl From<Vec<Vec<QueryOperation>>> for OperationList {
    fn from(ops: Vec<Vec<QueryOperation>>) -> Self {
        Self(ops.into_iter().map(|ops| ops.into()).collect())
    }
}

/// A specification for a subquery included in a query
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subquery {
    /// Where does the subquery appear in the query?
    position: SubqueryPosition,

    /// The specification for the query itself
    seed: QuerySeed,
}

impl Subquery {
    fn add_to_query(self, state: &mut QueryState<'_>, query: &mut SelectStatement) {
        // perturb the generator to make a new table, so that we don't get the same table in the
        // subquery that we got in the outer query
        state.fresh_table_mut();
        let mut subquery = self.seed.generate(state);
        let right_table = state.some_table_in_query_mut(&mut subquery);
        let right_table_name = right_table.name.clone();
        let right_join_col = right_table.some_column_with_type(SqlType::Int(None));
        let right_join_key = subquery
            .fields
            .iter()
            // First, see if we're already projecting the join column we picked in the subquery
            .find_map(|f| match f {
                FieldDefinitionExpr::Expr {
                    expr:
                        Expr::Column(Column {
                            name,
                            table:
                                Some(Relation {
                                    name: table_name, ..
                                }),
                        }),
                    alias,
                } if *name == right_join_col.0 && *table_name == right_table_name.0 => {
                    Some(alias.clone().unwrap_or_else(|| name.clone()))
                }
                _ => None,
            })
            // If we don't find it, add it to the fields with a fresh alias, and use that alias as
            // our join column
            .unwrap_or_else(|| {
                let alias = state.fresh_alias();
                let col = Column {
                    name: right_join_col.clone().into(),
                    table: Some(right_table_name.into()),
                };
                subquery.fields.push(FieldDefinitionExpr::Expr {
                    expr: Expr::Column(col.clone()),
                    alias: Some(alias.clone()),
                });

                if let Some(gb) = &mut subquery.group_by {
                    gb.fields.push(FieldReference::Expr(Expr::Column(col)))
                }

                alias
            });

        let left_table = state.some_table_in_query_mut(query);
        let left_table_name = left_table.name.clone();
        let left_join_key = left_table.some_column_with_type(SqlType::Int(None));

        let subquery_name = state.fresh_alias();
        let (join_rhs, operator) = match self.position {
            SubqueryPosition::Cte(operator) => {
                query.ctes.push(CommonTableExpr {
                    name: subquery_name.clone(),
                    statement: subquery,
                });
                (
                    JoinRightSide::Table(TableExpr::from(Relation {
                        name: subquery_name.clone(),
                        schema: None,
                    })),
                    operator,
                )
            }
            SubqueryPosition::Join(operator) => (
                JoinRightSide::Table(TableExpr {
                    inner: TableExprInner::Subquery(Box::new(subquery)),
                    alias: Some(subquery_name.clone()),
                }),
                operator,
            ),

            SubqueryPosition::Exists { correlated } => {
                if let Some(col_type) = correlated {
                    let outer_table = state.some_table_in_query_mut(query);
                    let outer_col = outer_table.some_column_with_type(col_type.clone());
                    let outer_col = Column {
                        table: Some(outer_table.name.clone().into()),
                        name: outer_col.into(),
                    };

                    let subquery_table = if let Some(table) = query
                        .tables
                        .iter()
                        .chain(query.join.iter().filter_map(|jc| match &jc.right {
                            JoinRightSide::Table(tbl) => Some(tbl),
                            _ => None,
                        }))
                        .filter_map(|te| te.inner.as_table())
                        .next()
                    {
                        table.name.clone().into()
                    } else {
                        let subquery_table = state.some_table_not_in_query_mut(query);
                        subquery
                            .tables
                            .push(TableExpr::from(Relation::from(subquery_table.name.clone())));
                        subquery_table.name.clone()
                    };
                    let subquery_col = state
                        .gen
                        .table_mut(&subquery_table)
                        .unwrap()
                        .some_column_with_type(col_type);

                    and_where(
                        &mut subquery,
                        Expr::BinaryOp {
                            lhs: Box::new(Expr::Column(Column {
                                table: Some(subquery_table.into()),
                                name: subquery_col.into(),
                            })),
                            op: BinaryOperator::Equal,
                            rhs: Box::new(Expr::Column(outer_col)),
                        },
                    );
                }

                and_where(query, Expr::Exists(Box::new(subquery)));
                return;
            }
        };

        query.join.push(JoinClause {
            operator,
            right: join_rhs,
            constraint: JoinConstraint::On(Expr::BinaryOp {
                lhs: Box::new(Expr::Column(Column {
                    name: left_join_key.into(),
                    table: Some(left_table_name.into()),
                })),
                op: BinaryOperator::Equal,
                rhs: Box::new(Expr::Column(Column {
                    name: right_join_key,
                    table: Some(subquery_name.into()),
                })),
            }),
        })
    }
}

/// A specification for generating an individual query
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySeed {
    /// The set of operations to include in the query
    operations: Vec<QueryOperation>,

    /// A set of subqueries to include in the query
    subqueries: Vec<Subquery>,
}

impl Arbitrary for QuerySeed {
    type Parameters = QueryOperationArgs;

    type Strategy = BoxedStrategy<QuerySeed>;

    fn arbitrary_with(op_args: Self::Parameters) -> Self::Strategy {
        any_with::<Operations>((Default::default(), op_args.clone()))
            .prop_map(|Operations(operations)| Self {
                operations,
                subqueries: vec![],
            })
            .prop_recursive(3, 5, 3, move |inner| {
                (
                    proptest::collection::vec((any::<SubqueryPosition>(), inner), 0..3).prop_map(
                        |sqs| {
                            sqs.into_iter()
                                .map(|(position, mut seed)| {
                                    seed.operations.retain(|op| op.supported_in_subqueries());
                                    Subquery { position, seed }
                                })
                                .collect()
                        },
                    ),
                    any_with::<Operations>((Default::default(), op_args.clone())),
                )
                    .prop_map(|(subqueries, Operations(operations))| Self {
                        subqueries,
                        operations,
                    })
            })
            .boxed()
    }
}

impl QuerySeed {
    /// Construct a new QuerySeed with the given operations and subqueries
    pub fn new(operations: Vec<QueryOperation>, subqueries: Vec<Subquery>) -> Self {
        Self {
            operations,
            subqueries,
        }
    }

    fn generate(self, state: &mut QueryState) -> SelectStatement {
        let mut query = SelectStatement::default();

        for op in self.operations {
            op.add_to_query(state, &mut query);
        }

        for subquery in self.subqueries {
            subquery.add_to_query(state, &mut query);
        }

        if query.tables.is_empty() {
            state.some_table_in_query_mut(&mut query);
        }

        if query.fields.is_empty() {
            let col = column_in_query(state, &mut query);
            query.fields.push(FieldDefinitionExpr::Expr {
                expr: Expr::Column(col.clone()),
                alias: Some(state.fresh_alias()),
            });

            if query.tables.is_empty() {
                query.tables.push(col.table.unwrap().into());
            }
        }

        if query_has_aggregate(&query) {
            let mut group_by = query.group_by.take().unwrap_or_default();
            // Fill the GROUP BY with all columns not mentioned in an aggregate
            let existing_group_by_exprs: HashSet<_> = group_by
                .fields
                .iter()
                .map(|gbc| match gbc {
                    FieldReference::Numeric(_) => {
                        unreachable!("We don't currently ever generate numeric field references")
                    }
                    FieldReference::Expr(expr) => expr.clone(),
                })
                .collect();
            for field in &query.fields {
                if let FieldDefinitionExpr::Expr { expr, .. } = field {
                    if !contains_aggregate(expr) {
                        for col in expr.referred_columns() {
                            if !existing_group_by_exprs
                                .iter()
                                .any(|e| matches!(e, Expr::Column(c) if c == col))
                            {
                                group_by
                                    .fields
                                    .push(FieldReference::Expr(Expr::Column(col.clone())));
                            }
                        }
                    }
                }
            }

            if let Some(order) = &query.order {
                for OrderBy { field, .. } in &order.order_by {
                    let expr = match field {
                        FieldReference::Expr(expr) => expr,
                        FieldReference::Numeric(_) => unreachable!(
                            "We don't currently ever generate numeric field references"
                        ),
                    };
                    if !existing_group_by_exprs.contains(expr) {
                        group_by.fields.push(FieldReference::Expr(expr.clone()));
                    }
                }
            }

            // TODO: once we support HAVING we'll need to check that here too
            if !group_by.fields.is_empty() {
                query.group_by = Some(group_by);
            }
        }

        query
    }
}

fn parse_num_operations<T>(s: &str) -> anyhow::Result<BoundPair<T>>
where
    T: FromStr + Clone,
    <T as FromStr>::Err: Send + Sync + Error + 'static,
{
    use Bound::*;

    let (lower_s, upper_s) = match s.split_once("..") {
        Some(lu) => lu,
        None => {
            let n = T::from_str(s)?;
            return Ok((Included(n.clone()), Included(n)));
        }
    };

    let lower = T::from_str(lower_s)?;

    if let Some(without_equals) = upper_s.strip_prefix('=') {
        Ok((Included(lower), Included(T::from_str(without_equals)?)))
    } else {
        Ok((Included(lower), Excluded(T::from_str(upper_s)?)))
    }
}

#[derive(Parser, Clone)]
pub struct GenerateOpts {
    /// Comma-separated list of query operations to generate top-level queries with
    ///
    /// If not specified, will permute the set of all possible query operations.
    #[arg(long)]
    pub operations: Option<OperationList>,

    /// Maximum recursion depth to use when generating subqueries
    #[arg(long, default_value = "2")]
    pub subquery_depth: usize,

    /// Range of operations to be used in a single query, represented as either a single number or
    /// a Rust-compatible range
    ///
    /// If not specified, queries will all contain a number of operations equal to the length of
    /// `operations`.
    #[arg(long, value_parser = parse_num_operations::<usize>)]
    pub num_operations: Option<BoundPair<usize>>,
}

impl GenerateOpts {
    /// Construct an iterator of [`QuerySeed`]s from the options in self.
    ///
    /// This involves permuting [`Self::operations`] up to [`Self::num_operations`] times, and
    /// recursively generating subqueries up to a depth of [`Self::subquery_depth`]
    pub fn into_query_seeds(self) -> impl Iterator<Item = QuerySeed> {
        let operations: Vec<_> = match self.operations {
            Some(OperationList(ops)) => ops.into_iter().flat_map(|ops| ops.into_iter()).collect(),
            None => ALL_OPERATIONS.clone(),
        };

        let (subqueries, operations): (Vec<SubqueryPosition>, Vec<QueryOperation>) =
            operations.into_iter().partition_map(|op| {
                if let QueryOperation::Subquery(position) = op {
                    Either::Left(position)
                } else {
                    Either::Right(op)
                }
            });

        let num_operations = match self.num_operations {
            None => Either::Left(1..=operations.len()),
            Some(num_ops) => Either::Right(num_ops.into_iter().unwrap()),
        };

        let available_ops: Vec<_> = num_operations
            .flat_map(|depth| operations.clone().into_iter().combinations(depth))
            .collect();

        fn make_seeds(
            subquery_depth: usize,
            operations: Vec<QueryOperation>,
            subqueries: Vec<SubqueryPosition>,
            available_ops: Vec<Vec<QueryOperation>>,
        ) -> Box<dyn Iterator<Item = QuerySeed>> {
            if subquery_depth == 0 || subqueries.is_empty() {
                Box::new(iter::once(QuerySeed {
                    operations,
                    subqueries: vec![],
                }))
            } else {
                Box::new(
                    subqueries
                        .iter()
                        .cloned()
                        .map(|position| {
                            if available_ops.is_empty() {
                                Box::new(Either::Left(make_seeds(
                                    subquery_depth - 1,
                                    vec![],
                                    subqueries.clone(),
                                    vec![],
                                )))
                            } else {
                                Box::new(Either::Right(
                                    available_ops
                                        .clone()
                                        .into_iter()
                                        .map(|mut ops| {
                                            ops.retain(|op| op.supported_in_subqueries());
                                            ops
                                        })
                                        .flat_map(|operations| {
                                            make_seeds(
                                                subquery_depth - 1,
                                                operations,
                                                subqueries.clone(),
                                                available_ops.clone(),
                                            )
                                        }),
                                ))
                            }
                            .map(|seed| Subquery {
                                position: position.clone(),
                                seed,
                            })
                            .collect::<Vec<_>>()
                        })
                        .multi_cartesian_product()
                        .map(move |subqueries| QuerySeed {
                            operations: operations.clone(),
                            subqueries,
                        }),
                )
            }
        }

        let subquery_depth = self.subquery_depth;

        if operations.is_empty() {
            Either::Left(make_seeds(
                subquery_depth,
                operations,
                subqueries,
                available_ops,
            ))
        } else {
            Either::Right(available_ops.clone().into_iter().flat_map(move |ops| {
                make_seeds(
                    subquery_depth,
                    ops,
                    subqueries.clone(),
                    available_ops.clone(),
                )
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use readyset_sql::ast::BinaryOperator;
    use readyset_sql::DialectDisplay;

    use super::*;

    fn generate_query(operations: Vec<QueryOperation>) -> SelectStatement {
        let mut gen = GeneratorState::default();
        let seed = QuerySeed {
            operations,
            subqueries: vec![],
        };
        gen.generate_query(seed).statement
    }

    #[test]
    fn parse_operation_list() {
        let src = "aggregates,joins";
        let OperationList(res) = OperationList::from_str(src).unwrap();
        assert_eq!(
            res,
            vec![
                Operations(vec![
                    QueryOperation::ColumnAggregate(AggregateType::Count {
                        column_type: SqlType::Int(None),
                        distinct: true,
                    }),
                    QueryOperation::ColumnAggregate(AggregateType::Count {
                        column_type: SqlType::Int(None),
                        distinct: false,
                    }),
                    QueryOperation::ColumnAggregate(AggregateType::Sum {
                        column_type: SqlType::Int(None),
                        distinct: true,
                    }),
                    QueryOperation::ColumnAggregate(AggregateType::Sum {
                        column_type: SqlType::Int(None),
                        distinct: false,
                    }),
                    QueryOperation::ColumnAggregate(AggregateType::Avg {
                        column_type: SqlType::Int(None),
                        distinct: true,
                    }),
                    QueryOperation::ColumnAggregate(AggregateType::Avg {
                        column_type: SqlType::Int(None),
                        distinct: false,
                    }),
                    QueryOperation::ColumnAggregate(AggregateType::GroupConcat),
                    QueryOperation::ColumnAggregate(AggregateType::Max {
                        column_type: SqlType::Int(None)
                    }),
                    QueryOperation::ColumnAggregate(AggregateType::Min {
                        column_type: SqlType::Int(None)
                    }),
                ]),
                Operations(vec![
                    QueryOperation::Join(JoinOperator::LeftJoin),
                    QueryOperation::Join(JoinOperator::LeftOuterJoin),
                    QueryOperation::Join(JoinOperator::InnerJoin),
                ])
            ]
        );
    }

    #[test]
    fn single_join() {
        let query = generate_query(vec![QueryOperation::Join(JoinOperator::LeftJoin)]);
        eprintln!("query: {}", query.display(ParseDialect::MySQL));
        assert_eq!(query.tables.len(), 1);
        assert_eq!(query.join.len(), 1);
        let join = query.join.first().unwrap();
        match &join.constraint {
            JoinConstraint::On(Expr::BinaryOp { op, lhs, rhs }) => {
                assert_eq!(op, &BinaryOperator::Equal);
                match (lhs.as_ref(), rhs.as_ref()) {
                    (Expr::Column(left_field), Expr::Column(right_field)) => {
                        assert_eq!(
                            left_field
                                .table
                                .as_ref()
                                .map(|t| TableExpr::from(t.clone()))
                                .as_ref(),
                            Some(query.tables.first().unwrap())
                        );
                        assert_eq!(
                            right_field
                                .table
                                .as_ref()
                                .map(|t| TableExpr::from(t.clone()))
                                .as_ref(),
                            Some(match &join.right {
                                JoinRightSide::Table(table) => table,
                                _ => unreachable!(),
                            })
                        );
                    }
                    _ => unreachable!(),
                }
            }
            constraint => unreachable!("Unexpected constraint: {:?}", constraint),
        }
    }

    mod parse_num_operations {
        use super::*;

        #[test]
        fn number() {
            assert_eq!(
                parse_num_operations::<usize>("13").unwrap(),
                (Bound::Included(13), Bound::Included(13))
            );
        }

        #[test]
        fn exclusive() {
            assert_eq!(
                parse_num_operations::<usize>("0..9").unwrap(),
                (Bound::Included(0), Bound::Excluded(9))
            )
        }

        #[test]
        fn inclusive() {
            assert_eq!(
                parse_num_operations::<usize>("0..=123").unwrap(),
                (Bound::Included(0), Bound::Included(123))
            )
        }
    }

    #[test]
    fn in_params() {
        let mut gen = GeneratorState::default();
        let seed = QuerySeed {
            operations: vec![QueryOperation::InParameter { num_values: 3 }],
            subqueries: vec![],
        };
        let query = gen.generate_query(seed);
        eprintln!(
            "query: {}",
            query.statement.display(readyset_sql::Dialect::MySQL)
        );
        match query.statement.where_clause {
            Some(Expr::In {
                lhs: _,
                rhs: InValue::List(exprs),
                negated: false,
            }) => {
                assert_eq!(exprs.len(), 3);
                assert!(exprs.iter().all(|expr| *expr
                    == Expr::Literal(Literal::Placeholder(ItemPlaceholder::QuestionMark))));
            }
            _ => unreachable!(),
        }

        let key = query.state.key();
        assert_eq!(key.len(), 3);
    }

    #[test]
    fn into_query_seeds_just_subquery() {
        let opts = GenerateOpts {
            operations: Some(
                vec![vec![QueryOperation::Subquery(SubqueryPosition::Cte(
                    JoinOperator::InnerJoin,
                ))]]
                .into(),
            ),
            subquery_depth: 1,
            num_operations: None,
        };

        let seeds = opts.into_query_seeds().collect::<Vec<_>>();
        assert_eq!(seeds.len(), 1);
        assert_eq!(
            seeds.first().unwrap(),
            &QuerySeed {
                operations: vec![],
                subqueries: vec![Subquery {
                    position: SubqueryPosition::Cte(JoinOperator::InnerJoin),
                    seed: QuerySeed {
                        operations: vec![],
                        subqueries: vec![]
                    }
                }]
            }
        )
    }

    #[test]
    fn double_param_uses_different_col() {
        let query = generate_query(vec![
            QueryOperation::SingleParameter,
            QueryOperation::RangeParameter,
        ]);
        eprintln!("query: {}", query.display(ParseDialect::MySQL));
        match &query.where_clause {
            Some(
                expr @ Expr::BinaryOp {
                    lhs,
                    op: BinaryOperator::And,
                    rhs,
                },
            ) => match (lhs.as_ref(), rhs.as_ref()) {
                (Expr::BinaryOp { lhs: lhs1, .. }, Expr::BinaryOp { lhs: lhs2, .. }) => {
                    assert_ne!(lhs1, lhs2);
                }
                _ => panic!(
                    "Unexpected where clause for query: {}",
                    expr.display(ParseDialect::MySQL)
                ),
            },
            Some(expr) => panic!(
                "Unexpected where clause for query: {}",
                expr.display(ParseDialect::MySQL)
            ),
            None => panic!("Expected query to have a where clause!"),
        }
    }
}
