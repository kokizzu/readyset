use std::error::Error;
use std::fmt::Debug;

use async_trait::async_trait;
pub use database_utils::UpstreamConfig;
use readyset_adapter_types::{DeallocateId, PreparedStatementType};
use readyset_client_metrics::QueryDestination;
use readyset_data::DfValue;
use readyset_errors::ReadySetError;
use readyset_sql::ast::{SqlIdentifier, StartTransactionStatement};
use readyset_util::redacted::RedactedString;
use tracing::debug;

pub type UpstreamStatementId = u32;

/// Information about a statement that has been prepared in an [`UpstreamDatabase`]
pub struct UpstreamPrepare<DB: UpstreamDatabase> {
    pub statement_id: UpstreamStatementId,
    pub meta: DB::StatementMeta,
}

impl<DB: UpstreamDatabase> Debug for UpstreamPrepare<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamPrepare")
            .field("statement_id", &self.statement_id)
            .field("meta", &self.meta)
            .finish()
    }
}

impl<DB: UpstreamDatabase> Clone for UpstreamPrepare<DB> {
    fn clone(&self) -> Self {
        UpstreamPrepare {
            statement_id: self.statement_id,
            meta: self.meta.clone(),
        }
    }
}

pub trait IsFatalError {
    fn is_fatal(&self) -> bool;
}

pub trait UpstreamDestination {
    fn destination(&self) -> QueryDestination {
        QueryDestination::Upstream
    }
}

/// A connector to some kind of upstream database which can be used for passthrough write queries
/// and fallback read queries.
///
/// An implementation of this trait can optionally be used to back a [`Reader`][] for fallback in
/// addition to ReadySet, or a [`Writer`][] for passthrough writes instead of ReadySet.
///
/// [`Reader`]: crate::backend::Reader
/// [`Writer`]: crate::backend::Writer
#[async_trait]
pub trait UpstreamDatabase: Sized + Send {
    /// The result returned by queries. Likely to be implemented as an enum containing a read or a
    /// write result.
    ///
    /// This type is used as the value inside of [`QueryResult::Upstream`][]
    ///
    /// [`QueryResult::Upstream`]: crate::backend::QueryResult::Upstream
    type QueryResult<'a>: Debug + UpstreamDestination
    where
        Self: 'a;

    /// A type representing metadata about a prepared statement.
    ///
    /// This type is used as a field of [`UpstreamPrepare`], returned from
    /// [`prepare`](UpstreamDatabase::prepaare)
    type StatementMeta: Debug + Send + Clone + 'static;

    /// Extra data passed to [`prepare`] by the protocol shim
    ///
    /// [`prepare`](UpstreamDatabase::prepaare)
    type PrepareData<'a>: Default + Send;

    /// Metadata passed to [`execute`] by the protocol shim
    ///
    /// [`execute`](UpstreamDatabase::execute)
    type ExecMeta<'a>: Send;

    /// Errors that can be returned from operations on this database
    ///
    /// This type, which must have at least one enum variant that includes a
    /// [`readyset_client::ReadySetError`], is used as the error type for all return values in the
    /// noria_client backend.
    type Error: From<ReadySetError> + IsFatalError + Error + Send + Sync + 'static;

    /// When there's no upstream DB to fetch the version from, default to this value. This features
    /// is only used for tests
    const DEFAULT_DB_VERSION: &'static str;

    /// Returns the SQL dialect to use for formatting queries
    const SQL_DIALECT: readyset_sql::Dialect;

    /// Create a new connection to this upstream database
    ///
    /// Connect will return an error if the upstream database is running an unsupported version.
    async fn connect(
        upstream_config: UpstreamConfig,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Self, Self::Error>;

    /// Set the user for the upstream connection
    async fn set_user(&mut self, user: &str, password: RedactedString) -> Result<(), Self::Error>;

    /// Test the connection with the upstream database
    async fn is_connected(&mut self) -> Result<bool, Self::Error>;

    /// Reconnect using new user
    async fn change_user(
        &mut self,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<(), Self::Error>;

    /// Ping the upstream connection to see if it is still alive
    async fn ping(&mut self) -> Result<(), Self::Error>;

    /// Reset the connection to the upstream database
    async fn reset(&mut self) -> Result<(), Self::Error>;

    /// Returns a database name if it was included in the original connection string, or None if no
    /// database name was included in the original connection string.
    fn database(&self) -> Option<&str> {
        None
    }

    /// Returns the servers's version string, including modifications to indicate that the
    /// connection is running via ReadySet
    fn version(&self) -> String;

    /// Send a request to the upstream database to prepare the given query, returning a unique ID
    /// for that prepared statement
    ///
    /// Implementations of this trait can use any method they like to store prepared statements
    /// associated with statement IDs, as long as after calling `on_prepare` on one instance of an
    /// UpstreamDatabase a later call of [`on_execute`] on the same UpstreamDatabase with the same
    /// statement ID executes that statement.
    async fn prepare<'a, 'b, S>(
        &'a mut self,
        query: S,
        data: Self::PrepareData<'b>,
        statement_type: PreparedStatementType,
    ) -> Result<UpstreamPrepare<Self>, Self::Error>
    where
        S: AsRef<str> + Send + Sync + 'a;

    /// Execute a statement that was prepared earlier with ['on_prepare'], with the given params
    ///
    /// The `exec_meta` argument is database-specific, and is generally passed through by the
    /// caller of [`Backend::execute`] if that call ends up being passed to the upstream.
    ///
    /// If 'on_execute' is called with a 'statement_id' that was not previously passed to
    /// 'on_prepare', this method should return
    /// ['Err(Error::ReadySet(ReadySetError::PreparedStatementMissing))'
    /// ](readyset_client::ReadySetError:: PreparedStatementMissing)
    /// [`Backend::execute`](readyset_client::Backend::execute)
    async fn execute<'a>(
        &'a mut self,
        statement_id: &UpstreamStatementId,
        params: &[DfValue],
        exec_meta: Self::ExecMeta<'_>,
    ) -> Result<Self::QueryResult<'a>, Self::Error>;

    /// Remove a prepared statement from the cache, and tell the upstream database to remove it and
    /// free any resources associated with it.
    ///
    /// Returns an error if the statement doesn't exist
    async fn remove_statement(&mut self, statement_id: DeallocateId) -> Result<(), Self::Error>;

    /// Execute a raw, un-prepared query
    async fn query<'a>(&'a mut self, query: &'a str) -> Result<Self::QueryResult<'a>, Self::Error>;

    /// Execute a raw, un-prepared query (or multiple queries concatenated in the provided `query`
    /// string, separated by semicolons) using the 'simple query' protocol flow[0],
    ///
    ///
    /// Note that the implementation of simple_query buffers results in memory before returning, so
    /// it should not be used for cases where there are large result sets. Use
    /// [`query`](Self::query) for most cases.
    ///
    /// Note that this is only relevant for PostgreSQL upstreams.
    ///
    /// [0] https://www.postgresql.org/docs/current/protocol-flow.html#PROTOCOL-FLOW-SIMPLE-QUERY
    async fn simple_query<'a>(
        &'a mut self,
        query: &'a str,
    ) -> Result<Self::QueryResult<'a>, Self::Error>;

    /// Handle starting a transaction with the upstream database.
    async fn start_tx<'a>(
        &'a mut self,
        stmt: &StartTransactionStatement,
    ) -> Result<Self::QueryResult<'a>, Self::Error>;

    /// Handle committing a transaction to the upstream database.
    async fn commit<'a>(&'a mut self) -> Result<Self::QueryResult<'a>, Self::Error>;

    /// Handle rolling back the ongoing transaction for this connection to the upstream db.
    async fn rollback<'a>(&'a mut self) -> Result<Self::QueryResult<'a>, Self::Error>;

    /// Query the upstream database for the currently configured schema search path.
    ///
    /// Note that the terminology used here is maximally general - while only PostgreSQL truly
    /// supports a multi-element schema search path, the concept of "currently connected database"
    /// in MySQL can be thought of as a schema search path that only has one element
    async fn schema_search_path(&mut self) -> Result<Vec<SqlIdentifier>, Self::Error>;

    async fn timezone_name(&mut self) -> Result<SqlIdentifier, Self::Error>;

    async fn lower_case_database_names(&mut self) -> Result<bool, Self::Error>;
    async fn lower_case_table_names(&mut self) -> Result<bool, Self::Error>;
}

pub struct LazyUpstream<U> {
    upstream: Option<U>,
    upstream_config: UpstreamConfig,
    username: Option<String>,
    password: Option<String>,
}

impl<U> From<UpstreamConfig> for LazyUpstream<U> {
    fn from(upstream_config: UpstreamConfig) -> Self {
        Self {
            upstream: None,
            upstream_config,
            username: None,
            password: None,
        }
    }
}

impl<U> LazyUpstream<U>
where
    U: UpstreamDatabase,
{
    pub async fn connect(&mut self) -> Result<(), U::Error> {
        debug!("LazyUpstream connecting to upstream");
        self.upstream = Some(
            U::connect(
                self.upstream_config.clone(),
                self.username.clone(),
                self.password.clone(),
            )
            .await?,
        );
        Ok(())
    }

    async fn upstream(&mut self) -> Result<&mut U, U::Error> {
        if self.upstream.is_none() {
            self.connect().await?;
        }

        Ok(self.upstream.as_mut().unwrap())
    }
}

#[async_trait]
impl<U> UpstreamDatabase for LazyUpstream<U>
where
    U: UpstreamDatabase,
{
    type QueryResult<'a>
        = U::QueryResult<'a>
    where
        U: 'a;
    type StatementMeta = U::StatementMeta;
    type PrepareData<'a> = U::PrepareData<'a>;
    type ExecMeta<'a> = U::ExecMeta<'a>;
    type Error = U::Error;

    const DEFAULT_DB_VERSION: &'static str = U::DEFAULT_DB_VERSION;
    const SQL_DIALECT: readyset_sql::Dialect = U::SQL_DIALECT;

    async fn connect(
        upstream_config: UpstreamConfig,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            upstream: None,
            upstream_config,
            username,
            password,
        })
    }

    async fn set_user(&mut self, user: &str, password: RedactedString) -> Result<(), Self::Error> {
        self.username = Some(user.to_string());
        self.password = Some(password.to_string());
        Ok(())
    }

    async fn is_connected(&mut self) -> Result<bool, Self::Error> {
        Ok(self.upstream().await?.is_connected().await?)
    }

    async fn change_user(
        &mut self,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<(), Self::Error> {
        self.upstream()
            .await?
            .change_user(user, password, database)
            .await
    }

    async fn ping(&mut self) -> Result<(), Self::Error> {
        if let Some(u) = &mut self.upstream {
            u.ping().await
        } else {
            Ok(())
        }
    }

    async fn reset(&mut self) -> Result<(), Self::Error> {
        self.upstream().await?.reset().await
    }

    fn database(&self) -> Option<&str> {
        if let Some(u) = &self.upstream {
            u.database()
        } else {
            None
        }
    }

    fn version(&self) -> String {
        match &self.upstream {
            Some(u) => u.version(),
            None => U::DEFAULT_DB_VERSION.into(),
        }
    }

    async fn prepare<'a, 'b, S>(
        &'a mut self,
        query: S,
        data: Self::PrepareData<'b>,
        statement_type: PreparedStatementType,
    ) -> Result<UpstreamPrepare<Self>, Self::Error>
    where
        S: AsRef<str> + Send + Sync + 'a,
    {
        let UpstreamPrepare { statement_id, meta } = self
            .upstream()
            .await?
            .prepare(query, data, statement_type)
            .await?;
        Ok(UpstreamPrepare { statement_id, meta })
    }

    async fn execute<'a>(
        &'a mut self,
        statement_id: &UpstreamStatementId,
        params: &[DfValue],
        exec_meta: Self::ExecMeta<'_>,
    ) -> Result<Self::QueryResult<'a>, Self::Error> {
        self.upstream()
            .await?
            .execute(statement_id, params, exec_meta)
            .await
    }

    async fn remove_statement(&mut self, statement_id: DeallocateId) -> Result<(), Self::Error> {
        self.upstream().await?.remove_statement(statement_id).await
    }

    async fn query<'a>(&'a mut self, query: &'a str) -> Result<Self::QueryResult<'a>, Self::Error> {
        self.upstream().await?.query(query).await
    }

    async fn simple_query<'a>(
        &'a mut self,
        query: &'a str,
    ) -> Result<Self::QueryResult<'a>, Self::Error> {
        self.upstream().await?.simple_query(query).await
    }

    async fn start_tx<'a>(
        &'a mut self,
        stmt: &StartTransactionStatement,
    ) -> Result<Self::QueryResult<'a>, Self::Error> {
        self.upstream().await?.start_tx(stmt).await
    }

    async fn commit<'a>(&'a mut self) -> Result<Self::QueryResult<'a>, Self::Error> {
        self.upstream().await?.commit().await
    }

    async fn rollback<'a>(&'a mut self) -> Result<Self::QueryResult<'a>, Self::Error> {
        self.upstream().await?.rollback().await
    }

    async fn schema_search_path(&mut self) -> Result<Vec<SqlIdentifier>, Self::Error> {
        self.upstream().await?.schema_search_path().await
    }

    async fn timezone_name(&mut self) -> Result<SqlIdentifier, Self::Error> {
        self.upstream().await?.timezone_name().await
    }

    async fn lower_case_database_names(&mut self) -> Result<bool, Self::Error> {
        self.upstream().await?.lower_case_database_names().await
    }

    async fn lower_case_table_names(&mut self) -> Result<bool, Self::Error> {
        self.upstream().await?.lower_case_table_names().await
    }
}
