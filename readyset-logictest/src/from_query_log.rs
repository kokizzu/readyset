use std::convert::TryFrom;
use std::path::PathBuf;

use anyhow::anyhow;
use clap::Parser;
use database_utils::{DatabaseConnection, DatabaseURL, QueryableConnection};
use itertools::Itertools;
use readyset_sql::ast::{Expr, FieldDefinitionExpr, FunctionExpr, SqlQuery};
use readyset_sql::{Dialect, DialectDisplay};
use readyset_sql_parsing::parse_query;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufReader};
use tracing::error;

use crate::ast::{Record, Statement, StatementResult, Value};

mod querylog;
use querylog::{Command, Entry, Session, Stream};

/// Convert a MySQL query log to a set of test scripts.
#[derive(Parser)]
pub struct FromQueryLog {
    /// URL of a reference database to connect to, execute queries from the log, and record the
    /// results.  Currently supports `mysql://` URLs, but may be expanded in the future.
    #[arg(long)]
    pub database: DatabaseURL,

    /// Enable verbose output
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Generate a separate logictest file for each client session in the querylog
    #[arg(long)]
    pub split_sessions: bool,

    /// Exclude DDL statements from the resulting logictest
    #[arg(long)]
    pub skip_ddl: bool,

    /// Log unparsable queries to a file
    #[arg(long)]
    pub unparsable_query_log: Option<PathBuf>,

    #[arg(skip)]
    unparsable_query_log_file: Option<File>,

    /// Query log to convert
    pub input: PathBuf,

    /// Directory to output logic tests into
    pub output: PathBuf,
}

fn should_validate_results(query: &str, parsed_query: &Option<SqlQuery>) -> bool {
    if let Some(parsed_query) = parsed_query {
        if let SqlQuery::Select(ref select) = parsed_query {
            if select.tables.is_empty() {
                for field in &select.fields {
                    if let FieldDefinitionExpr::Expr { expr, .. } = field {
                        match expr {
                            Expr::Call(FunctionExpr::Call { name, .. }) => match name.as_str() {
                                "VERSION" => return false,
                                "DATABASE" => return false,
                                _ => (),
                            },
                            Expr::Column(column) if column.name.starts_with("@@") => return false,
                            _ => (),
                        };
                    }
                }
            }
        } else {
            return false;
        }
    } else {
        // WARNING:  Here be hacks.
        // These conditionals work around nom_sql not parsing SHOW and certain SELECT queries.
        // From what I've seen thus far, subselects and SELECT DATABASE() are the SELECT queries
        // that fail.  In a future with flawless parsing, we can move the SHOW TABLES check to be a
        // SqlQuery::Show case above, and unconditionally return false here.
        let query = query.trim().to_ascii_uppercase();
        if !query.starts_with("SELECT") {
            if query != "SHOW TABLES" {
                return false;
            }
        } else if query.split(' ').contains(&"DATABASE()") {
            return false;
        }
    }
    true
}

fn is_ddl(query: &SqlQuery) -> bool {
    match query {
        SqlQuery::Select(_)
        | SqlQuery::Insert(_)
        | SqlQuery::Delete(_)
        | SqlQuery::Update(_)
        | SqlQuery::Set(_)
        | SqlQuery::CompoundSelect(_)
        | SqlQuery::StartTransaction(_)
        | SqlQuery::Commit(_)
        | SqlQuery::Rollback(_)
        | SqlQuery::Show(_)
        | SqlQuery::Explain(_)
        | SqlQuery::Deallocate(_)
        | SqlQuery::Truncate(_)
        | SqlQuery::Comment(_) => false,
        SqlQuery::CreateDatabase(_)
        | SqlQuery::CreateTable(_)
        | SqlQuery::CreateView(_)
        | SqlQuery::DropTable(_)
        | SqlQuery::DropView(_)
        | SqlQuery::AlterTable(_)
        | SqlQuery::RenameTable(_)
        | SqlQuery::Use(_)
        | SqlQuery::AlterReadySet(_)
        | SqlQuery::CreateCache(_)
        | SqlQuery::DropCache(_)
        | SqlQuery::DropAllProxiedQueries(_)
        | SqlQuery::DropAllCaches(_)
        | SqlQuery::CreateRls(_)
        | SqlQuery::DropRls(_) => true,
    }
}

impl FromQueryLog {
    async fn process_query(
        &mut self,
        entry: &Entry,
        conn: &mut DatabaseConnection,
    ) -> anyhow::Result<Option<Record>> {
        let parsed = parse_query(Dialect::MySQL, &entry.arguments).ok();
        let result = conn.query(&entry.arguments).await;

        if self.skip_ddl && parsed.iter().any(is_ddl) {
            return Ok(None);
        }

        let record = match result {
            Ok(rows) => {
                if !should_validate_results(&entry.arguments, &parsed) {
                    Record::Statement(Statement {
                        result: StatementResult::Ok,
                        command: entry.arguments.clone(),
                        conditionals: vec![],
                    })
                } else {
                    Record::query(
                        entry.arguments.clone(),
                        parsed.as_ref(),
                        vec![],
                        rows.try_into()?,
                    )
                }
            }
            Err(err) => Record::Statement(Statement {
                result: StatementResult::Error {
                    pattern: Some(err.to_string()),
                },
                command: entry.arguments.clone(),
                conditionals: vec![],
            }),
        };
        Ok(Some(record))
    }

    async fn process_execute(
        &mut self,
        session: &Session,
        entry: &Entry,
        conn: &mut DatabaseConnection,
    ) -> anyhow::Result<Option<Record>> {
        let parsed = self
            .parse_query(Dialect::MySQL, &entry.arguments)
            .await
            .map_err(|e| anyhow!(e))?;
        let (stmt, values) = session
            .find_prepared_statement(&parsed)
            .ok_or_else(|| anyhow!("Prepared statement not found"))?;
        let params = values
            .into_iter()
            .map(Value::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        // FIXME(REA-2168): Use correct dialect.
        let stmt_string = stmt.display(readyset_sql::Dialect::MySQL).to_string();

        let rows = conn.execute(&stmt_string, &params).await?;

        if self.skip_ddl && is_ddl(&parsed) {
            return Ok(None);
        }

        if should_validate_results(&stmt_string, &Some(parsed)) {
            Ok(Some(Record::query(
                stmt_string,
                Some(stmt),
                params,
                rows.try_into()?,
            )))
        } else {
            Ok(Some(Record::Statement(Statement {
                result: StatementResult::Ok,
                command: stmt_string,
                conditionals: vec![],
            })))
        }
    }

    async fn parse_query(
        &mut self,
        dialect: Dialect,
        query: impl AsRef<str>,
    ) -> anyhow::Result<SqlQuery> {
        match parse_query(dialect, query.as_ref()) {
            Ok(parsed) => Ok(parsed),
            Err(e) => {
                if let Some(f) = self.unparsable_query_log_file.as_mut() {
                    f.write_all(format!("{}\n\n", query.as_ref()).as_bytes())
                        .await?;
                }
                Err(anyhow!(e))
            }
        }
    }

    #[tokio::main]
    pub async fn run(mut self) -> anyhow::Result<()> {
        if let Some(path) = self.unparsable_query_log.as_ref() {
            self.unparsable_query_log_file = Some(
                OpenOptions::new()
                    .read(false)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .append(false)
                    .open(path)
                    .await
                    .unwrap(),
            );
        }

        let input = File::open(&self.input).await.unwrap();
        let mut input = Stream::new(BufReader::new(input), self.split_sessions);

        while let Some((session_number, mut session)) = input.next().await {
            // It is intentional to spin up a new connection for each session, so that we match the
            // logged behavior as closely as possible.
            let mut conn = self.database.connect(None).await.unwrap();
            let mut output = OpenOptions::new()
                .read(false)
                .write(true)
                .create(true)
                .truncate(true)
                .append(false)
                .open(self.output.join(session_number.to_string() + ".test"))
                .await
                .unwrap();
            for entry in &session.entries {
                let record = match entry.command {
                    Command::Connect => None,
                    Command::Query => self.process_query(entry, &mut conn).await.ok().flatten(),
                    Command::Prepare => {
                        let parsed = match parse_query(Dialect::MySQL, &entry.arguments) {
                            Ok(v) => v,
                            Err(err) => {
                                error!(
                                    %err,
                                    entry = entry.id,
                                    arguments = &entry.arguments,
                                    "Failed to parse",
                                );
                                continue;
                            }
                        };
                        session.prepared_statements.insert(parsed);
                        None
                    }
                    Command::Execute => {
                        match self.process_execute(&session, entry, &mut conn).await {
                            Ok(v) => Some(v).flatten(),
                            Err(err) => {
                                error!(
                                    %err,
                                    entry = entry.id,
                                    arguments = &entry.arguments,
                                    "Failed to execute",
                                );
                                continue;
                            }
                        }
                    }
                    Command::CloseStmt => None,
                    Command::Quit => None,
                };
                if let Some(record) = record {
                    output
                        .write_all(format!("{record}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
            output.flush().await.unwrap();
        }
        Ok(())
    }
}
