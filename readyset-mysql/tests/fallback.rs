use itertools::Itertools;
use mysql_async::prelude::*;
use mysql_async::{ChangeUserOpts, Conn};
use readyset_adapter::backend::UnsupportedSetMode;
use readyset_adapter::BackendBuilder;
use readyset_client::query::QueryId;
use readyset_client_metrics::QueryDestination;
use readyset_client_test_helpers::mysql_helpers::{last_query_info, MySQLAdapter};
use readyset_client_test_helpers::{self, sleep, TestBuilder};
use readyset_server::Handle;
use readyset_server::NodeIndex;
use readyset_sql::ast::Relation;
use readyset_util::eventually;
use readyset_util::shutdown::ShutdownSender;
use test_utils::skip_flaky_finder;
use test_utils::tags;

async fn setup_with(
    backend_builder: BackendBuilder,
) -> (mysql_async::Opts, Handle, ShutdownSender) {
    TestBuilder::new(backend_builder)
        .fallback(true)
        .build::<MySQLAdapter>()
        .await
}

async fn setup() -> (mysql_async::Opts, Handle, ShutdownSender) {
    setup_with(BackendBuilder::new().require_authentication(false)).await
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn create_table() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int, PRIMARY KEY(id))")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id) VALUES (1)")
        .await
        .unwrap();
    sleep().await;

    let row: Option<(i32,)> = conn
        .query_first("SELECT Cats.id FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(row, Some((1,)));

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn add_column() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int, PRIMARY KEY(id))")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id) VALUES (1)")
        .await
        .unwrap();
    sleep().await;

    let row: Option<(i32,)> = conn
        .query_first("SELECT Cats.id FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(row, Some((1,)));

    conn.query_drop("ALTER TABLE Cats ADD COLUMN name TEXT;")
        .await
        .unwrap();
    conn.query_drop("UPDATE Cats SET name = 'Whiskers' WHERE id = 1;")
        .await
        .unwrap();
    sleep().await;

    let row: Option<(i32, String)> = conn
        .query_first("SELECT Cats.id, Cats.name FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(row, Some((1, "Whiskers".to_owned())));

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
#[ignore = "REA-4099"]
async fn json_column_insert_read() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int PRIMARY KEY, data JSON)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id, data) VALUES (1, '{\"name\": \"Mr. Mistoffelees\"}')")
        .await
        .unwrap();
    sleep().await;
    sleep().await;

    let rows: Vec<(i32, String)> = conn
        .query("SELECT * FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec![(1, "{\"name\":\"Mr. Mistoffelees\"}".to_string())]
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn json_column_partial_update() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE Cats (id int PRIMARY KEY, data JSON)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("INSERT INTO Cats (id, data) VALUES (1, '{\"name\": \"xyz\"}')")
        .await
        .unwrap();
    conn.query_drop("UPDATE Cats SET data = JSON_REMOVE(data, '$.name') WHERE id = 1")
        .await
        .unwrap();
    sleep().await;

    let rows: Vec<(i32, String)> = conn
        .query("SELECT * FROM Cats WHERE Cats.id = 1")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, "{}".to_string())]);

    shutdown_tx.shutdown().await;
}

// TODO: remove this once we support range queries again
#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn range_query() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE cats (id int PRIMARY KEY, cuteness int)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO cats (id, cuteness) values (1, 10)")
        .await
        .unwrap();
    sleep().await;

    let rows: Vec<(i32, i32)> = conn
        .exec("SELECT id, cuteness FROM cats WHERE cuteness > ?", vec![5])
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, 10)]);

    shutdown_tx.shutdown().await;
}

// TODO: remove this once we support aggregates on parameterized IN
#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn aggregate_in() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE cats (id int PRIMARY KEY, cuteness int)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO cats (id, cuteness) values (1, 10), (2, 8)")
        .await
        .unwrap();
    sleep().await;

    let rows: Vec<(i32,)> = conn
        .exec(
            "SELECT sum(cuteness) FROM cats WHERE id IN (?, ?)",
            vec![1, 2],
        )
        .await
        .unwrap();
    assert_eq!(rows, vec![(18,)]);

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn set_autocommit() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();
    // We do not support SET autocommit = 0;
    conn.query_drop("SET @@SESSION.autocommit = 1;")
        .await
        .unwrap();
    conn.query_drop("SET @@SESSION.autocommit = 0;")
        .await
        .unwrap_err();
    conn.query_drop("SET @@LOCAL.autocommit = 1;")
        .await
        .unwrap();
    conn.query_drop("SET @@LOCAL.autocommit = 0;")
        .await
        .unwrap_err();

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn proxy_unsupported_sets() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();

    sleep().await;

    conn.query_drop("SET @@SESSION.SQL_MODE = 'ANSI_QUOTES';")
        .await
        .unwrap();

    // We should proxy the SET statement upstream, then all subsequent statements should go upstream
    // (evidenced by the fact that `"x"` is interpreted as a column reference, per the ANSI_QUOTES
    // SQL mode)
    assert_eq!(
        conn.query_first::<(i32,), _>("SELECT \"x\" FROM \"t\"")
            .await
            .unwrap()
            .unwrap()
            .0,
        1,
    );

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn proxy_unsupported_sets_prep_exec() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();

    sleep().await;

    conn.query_drop("SET @@SESSION.SQL_MODE = 'ANSI_QUOTES';")
        .await
        .unwrap();

    conn.exec_drop("SELECT * FROM t", ()).await.unwrap();

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn prepare_in_tx_select_out() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();
    let prepared = tx.prep("SELECT * FROM t").await.unwrap();
    tx.commit().await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn prep_and_select_in_tx() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    let prepared = tx.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = tx.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Upstream
    );
    tx.rollback().await.unwrap();

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn prep_then_select_in_tx() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    let _: Option<i64> = tx.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Upstream
    );
    tx.rollback().await.unwrap();

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn prep_then_always_select_in_tx() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;
    let prepared = conn.prep("SELECT x FROM t").await.unwrap();
    conn.query_drop("CREATE CACHE ALWAYS test_always FROM SELECT x FROM t;")
        .await
        .unwrap();
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    let _: Option<i64> = tx.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Readyset
    );
    tx.rollback().await.unwrap();

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn always_should_bypass_tx() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE ALWAYS test_always FROM SELECT x FROM t;")
        .await
        .unwrap();
    let mut tx = conn
        .start_transaction(mysql_async::TxOpts::new())
        .await
        .unwrap();

    tx.query_drop("SELECT x FROM t").await.unwrap();

    assert_eq!(
        last_query_info(&mut tx).await.destination,
        QueryDestination::Readyset
    );
    tx.rollback().await.unwrap();

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn prep_select() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();

    sleep().await;

    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn set_then_prep_and_select() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("set @foo = 5").await.unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn always_should_never_proxy() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    conn.query_drop("CREATE CACHE ALWAYS FROM SELECT * FROM t")
        .await
        .unwrap();
    conn.query_drop("set @foo = 5").await.unwrap();
    conn.query_drop("SELECT * FROM t").await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn always_should_never_proxy_exec() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE ALWAYS FROM SELECT * FROM t")
        .await
        .unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );
    conn.query_drop("set @foo = 5").await.unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn prep_then_set_then_select_proxy() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("set @foo = 5").await.unwrap();
    let prepared = conn.prep("SELECT * FROM t").await.unwrap();
    let _: Option<i64> = conn.exec_first(prepared, ()).await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn proxy_mode_should_allow_commands() {
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .unsupported_set_mode(UnsupportedSetMode::Proxy),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("INSERT INTO t (x) values (1)")
        .await
        .unwrap();
    sleep().await;

    conn.query_drop("SET @@SESSION.SQL_MODE = 'ANSI_QUOTES';")
        .await
        .unwrap();

    // We should proxy the SET statement upstream, then all subsequent statements should go upstream
    // (evidenced by the fact that `"x"` is interpreted as a column reference, per the ANSI_QUOTES
    // SQL mode)
    assert_eq!(
        conn.query_first::<(i32,), _>("SELECT \"x\" FROM \"t\"")
            .await
            .unwrap()
            .unwrap()
            .0,
        1,
    );

    // We should still handle custom ReadySet commands directly, otherwise we will end up passing
    // back errors from the upstream database for queries it doesn't recognize.
    // This validates what we already just validated (that the query went to upstream) and also
    // that EXPLAIN LAST STATEMENT, a ReadySet command, was handled directly by ReadySet and not
    // proxied upstream.
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn drop_then_recreate_table_with_query() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE q FROM SELECT x FROM t")
        .await
        .unwrap();
    conn.query_drop("SELECT x FROM t").await.unwrap();
    conn.query_drop("DROP TABLE t").await.unwrap();

    sleep().await;

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    conn.query_drop("CREATE CACHE q FROM SELECT x FROM t")
        .await
        .unwrap();
    // Query twice to clear the cache
    conn.query_drop("SELECT x FROM t").await.unwrap();
    conn.query_drop("SELECT x FROM t").await.unwrap();

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
#[skip_flaky_finder]
async fn transaction_proxies() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (x int)").await.unwrap();
    sleep().await;

    conn.query_drop("CREATE CACHE FROM SELECT * FROM t")
        .await
        .unwrap();

    conn.query_drop("BEGIN;").await.unwrap();
    conn.query_drop("SELECT * FROM t;").await.unwrap();

    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Upstream
    );

    conn.query_drop("COMMIT;").await.unwrap();

    conn.query_drop("SELECT * FROM t;").await.unwrap();
    assert_eq!(
        last_query_info(&mut conn).await.destination,
        QueryDestination::Readyset
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn show_caches_index_hints() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    let _ = conn.query_drop("CREATE TABLE t_idx_hnt (a INT PRIMARY KEY, b INT, c INT, KEY `idx_1`(b), KEY `idx_2`(c))")
        .await;

    // Run a base query
    let _ = conn
        .query_drop("SELECT a, b, c FROM t_idx_hnt WHERE a = 1;")
        .await;

    // Check we have cached this query
    // in-request-path migrations is enabled, we should have a cached query
    let cached_queries = conn
        .query::<(String, String, String, String), _>("SHOW CACHES;")
        .await
        .unwrap();
    assert!(cached_queries.len() == 1);

    // Run all possible index hints
    let index_hint_type_list: Vec<&str> = vec!["USE", "IGNORE", "FORCE"];
    let index_or_key_list: Vec<&str> = vec!["INDEX", "KEY"];
    let index_for_list: Vec<&str> = vec!["", "FOR JOIN", "FOR ORDER BY", "FOR GROUP BY"];
    let index_name_list: Vec<&str> = vec!["`idx_1`", "`idx_2`", "`primary`"];
    for hint_type in index_hint_type_list {
        for index_or_key in index_or_key_list.iter() {
            for index_for in index_for_list.iter() {
                let mut formatted_index_list_str = String::new();
                for n in index_name_list.iter() {
                    if formatted_index_list_str.is_empty() {
                        formatted_index_list_str = n.to_string();
                    } else {
                        formatted_index_list_str = format!("{formatted_index_list_str}, {n}");
                    }
                    let qstring = format!(
                        "SELECT a, b, c FROM `t_idx_hnt` {hint_type} {index_or_key} {index_for}({formatted_index_list_str}) WHERE a = 1"
                    );
                    conn.query_drop(&qstring).await.unwrap();
                }
            }
        }
    }

    // All variants of index hints should resolve to the same base query
    let cached_queries = conn
        .query::<(String, String, String, String), _>("SHOW CACHES;")
        .await
        .unwrap();
    assert!(cached_queries.len() == 1);

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
#[ignore = "Add failpoint to readyset-sql-parsing"]
async fn valid_sql_parsing_failed_shows_proxied() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();
    // This query needs to be valid SQL but fail to parse. The current query is one known to not be
    // supported by the parser, but if this test is failing, that may no longer be the case and
    // this the query should be replaced with another one that isn't supported, or a more
    // complicated way of testing this needs to be devised
    let q = "CREATE TABLE t1 (id polygon);".to_string();
    let _ = conn.query_drop(q.clone()).await;

    let proxied_queries = conn
        .query::<(String, String, String), _>("SHOW PROXIED QUERIES;")
        .await
        .unwrap();
    let id = QueryId::from_unparsed_select(&q);

    let proxied_query = proxied_queries
        .into_iter()
        .exactly_one()
        .expect("only one proxied query expected");

    assert_eq!(proxied_query.0, id.to_string());
    assert_eq!(proxied_query.1, q);
    assert!(proxied_query.2.starts_with("unsupported:"));

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn invalid_sql_parsing_failed_doesnt_show_proxied() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    let q = "this isn't valid SQL".to_string();
    let _ = conn.query_drop(q.clone()).await;
    let proxied_queries = conn
        .query::<(String, String, String), _>("SHOW PROXIED QUERIES;")
        .await
        .unwrap();

    assert!(proxied_queries.is_empty());

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
#[skip_flaky_finder]
async fn switch_database_with_use() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("DROP DATABASE IF EXISTS s1;")
        .await
        .unwrap();
    conn.query_drop("DROP DATABASE IF EXISTS s2;")
        .await
        .unwrap();
    conn.query_drop("CREATE DATABASE s1;").await.unwrap();
    conn.query_drop("CREATE DATABASE s2;").await.unwrap();
    conn.query_drop("CREATE TABLE s1.t (a int)").await.unwrap();
    conn.query_drop("CREATE TABLE s2.t (b int)").await.unwrap();
    conn.query_drop("CREATE TABLE s2.t2 (c int)").await.unwrap();

    conn.query_drop("USE s1;").await.unwrap();
    conn.query_drop("SELECT a FROM t").await.unwrap();

    conn.query_drop("USE s2;").await.unwrap();
    conn.query_drop("SELECT b FROM t").await.unwrap();
    conn.query_drop("SELECT c FROM t2").await.unwrap();

    shutdown_tx.shutdown().await;
}

#[cfg(feature = "failure_injection")]
#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
async fn replication_failure_ignores_table() {
    readyset_tracing::init_test_logging();
    use mysql_common::serde_json;
    use readyset_adapter::backend::MigrationMode;
    use readyset_errors::ReadySetError;
    use readyset_sql::ast::Relation;

    let (config, mut handle, shutdown_tx) = TestBuilder::default()
        .recreate_database(false)
        .fallback(true)
        .migration_mode(MigrationMode::OutOfBand)
        .build::<MySQLAdapter>()
        .await;

    // Tests that if a table fails replication due to a TableError, it is dropped and we stop
    // replicating it going forward
    let mut client = Conn::new(config).await.unwrap();

    client
        .query_drop("DROP TABLE IF EXISTS cats CASCADE")
        .await
        .unwrap();
    client
        .query_drop("DROP VIEW IF EXISTS cats_view")
        .await
        .unwrap();
    client
        .query_drop("CREATE TABLE cats (id int, PRIMARY KEY(id))")
        .await
        .unwrap();
    client
        .query_drop("CREATE VIEW cats_view AS SELECT id FROM cats ORDER BY id ASC")
        .await
        .unwrap();
    sleep().await;

    client
        .query_drop("INSERT INTO cats (id) VALUES (1)")
        .await
        .unwrap();

    sleep().await;
    sleep().await;

    assert_last_statement_matches("cats", "upstream", "ok", &mut client).await;
    client
        .query_drop("CREATE CACHE FROM SELECT * FROM cats")
        .await
        .unwrap();
    client
        .query_drop("CREATE CACHE FROM SELECT * FROM cats_view")
        .await
        .unwrap();
    sleep().await;

    let result: i32 = client
        .query_first("SELECT * FROM cats")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, 1);

    let result: i32 = client
        .query_first("SELECT * FROM cats_view")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, 1);

    let err_to_inject = ReadySetError::TableError {
        table: Relation {
            schema: Some("noria".into()),
            name: "cats".into(),
        },
        source: Box::new(ReadySetError::Internal("failpoint injected".to_string())),
    };

    handle
        .set_failpoint(
            readyset_util::failpoints::REPLICATION_HANDLE_ACTION,
            &format!(
                "1*return({})",
                serde_json::ser::to_string(&err_to_inject).expect("failed to serialize error")
            ),
        )
        .await;

    client
        .query_drop("INSERT INTO cats (id) VALUES (2)")
        .await
        .unwrap();

    sleep().await;

    client
        .query_drop("CREATE CACHE FROM SELECT * FROM cats")
        .await
        .expect_err("should fail to create cache now that table is ignored");
    client
        .query_drop("CREATE CACHE FROM SELECT * FROM cats_view")
        .await
        .expect_err("should fail to create cache now that table is ignored");

    for source in ["cats", "cats_view"] {
        let mut results: Vec<i32> = client
            .query(format!("SELECT * FROM {source}"))
            .await
            .unwrap();
        results.sort();
        assert_eq!(results, vec![1, 2]);
        assert_last_statement_matches(
            source,
            "readyset_then_upstream",
            "view destroyed",
            &mut client,
        )
        .await;
    }

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, mysql_upstream)]
async fn reset_user() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TEMPORARY TABLE t (id INT)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO t (id) VALUES (1)")
        .await
        .unwrap();
    let row_temp_table: Vec<i64> = conn.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(row_temp_table.len(), 1);
    assert_eq!(row_temp_table[0], 1);
    conn.reset().await.unwrap();
    let row = conn.query_drop("SELECT COUNT(*) FROM t").await;

    assert_eq!(
        row.map_err(|e| e.to_string()),
        Err("Server error: `ERROR 42S02 (1146): Table 'noria.t' doesn't exist'".to_string())
    );

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, slow, mysql_upstream)]
#[ignore = "REA-3933 (see comments on ticket)"]
async fn show_proxied_queries_show_caches_query_text_matches() {
    readyset_tracing::init_test_logging();
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();

    conn.query_drop("CREATE TABLE t (id INT)").await.unwrap();
    conn.query_drop("SELECT id FROM t").await.unwrap();
    sleep().await;

    let cached_queries: Vec<(String, String, String, String)> =
        conn.query("SHOW CACHES").await.unwrap();
    let (_, cache_name, cached_query_text, _) = cached_queries.first().unwrap();

    conn.query_drop(format!("DROP CACHE {cache_name}"))
        .await
        .unwrap();

    let proxied_queries: Vec<(String, String, String)> =
        conn.query("SHOW PROXIED QUERIES").await.unwrap();
    let (_, proxied_query_text, _) = proxied_queries.first().unwrap();

    assert_eq!(proxied_query_text, cached_query_text);

    shutdown_tx.shutdown().await;
}

#[allow(dead_code)]
async fn last_statement_matches(dest: &str, status: &str, client: &mut Conn) -> (bool, String) {
    let rows: Vec<(String, String)> = client
        .query("EXPLAIN LAST STATEMENT")
        .await
        .expect("explain query failed");
    let dest_col = rows[0].0.clone();
    let status_col = rows[0].1.clone();
    if !dest_col.contains(dest) {
        return (
            false,
            format!(r#"dest column was expected to contain "{dest}", but was: "{dest_col}""#),
        );
    }
    if !status_col.contains(status) {
        return (
            false,
            format!(r#"status column was expected to contain "{status}", but was: "{status_col}""#),
        );
    }
    (true, "".to_string())
}

#[allow(dead_code)]
async fn assert_last_statement_matches(table: &str, dest: &str, status: &str, client: &mut Conn) {
    let (matches, err) = last_statement_matches(dest, status, client).await;
    assert!(
        matches,
        "EXPLAIN LAST STATEMENT mismatch for query involving table {table}: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, mysql_upstream)]
async fn it_change_user() {
    let mut users = std::collections::HashMap::new();
    users.insert("root".to_string(), "noria".to_string());
    let (opts, _handle, shutdown_tx) = setup_with(
        BackendBuilder::new()
            .require_authentication(false)
            .users(users),
    )
    .await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TEMPORARY TABLE t (id INT)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO t (id) VALUES (1)")
        .await
        .unwrap();
    let row_temp_table: Vec<i64> = conn.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(row_temp_table.len(), 1);
    assert_eq!(row_temp_table[0], 1);
    let _ = conn
        .change_user(
            ChangeUserOpts::default()
                .with_user(Some("root".to_string()))
                .with_db_name(Some("noria".to_string()))
                .with_pass(Some("noria".to_string())),
        )
        .await;
    // Temporary table t should be gone after changing user
    let row = conn.query_drop("SELECT COUNT(*) FROM t").await;

    assert_eq!(
        row.map_err(|e| e.to_string()),
        Err("Server error: `ERROR 42S02 (1146): Table 'noria.t' doesn't exist'".to_string())
    );

    // Run change user again to make sure it can query the database
    let _ = conn
        .change_user(
            ChangeUserOpts::default()
                .with_user(Some("root".to_string()))
                .with_db_name(Some("noria".to_string()))
                .with_pass(Some("noria".to_string())),
        )
        .await;

    let row: Vec<String> = conn.query("SELECT @@version").await.unwrap();
    assert_eq!(row.len(), 1);

    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, mysql_upstream)]
async fn select_version_comment() {
    let (opts, _handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();
    let row: String = conn
        .query_first("SELECT @@version_comment")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(row, "Readyset");
    shutdown_tx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[tags(serial, mysql_upstream)]
async fn resnapshot_table_command() {
    async fn get_table_index<T: AsRef<str>>(
        handle: &mut Handle,
        schema: T,
        name: T,
    ) -> Option<NodeIndex> {
        let tables = handle.tables().await.unwrap();
        tables
            .get(&Relation {
                schema: Some(schema.as_ref().into()),
                name: name.as_ref().into(),
            })
            .cloned()
    }

    let (opts, mut handle, shutdown_tx) = setup().await;
    let mut conn = Conn::new(opts).await.unwrap();
    conn.query_drop("CREATE TABLE t (id INT)").await.unwrap();

    let old_table = eventually!(
        run_test: { get_table_index(&mut handle, "noria", "t").await },
        then_assert: |result| {
            assert!(result.is_some());
            result
        }
    );

    conn.query_drop("ALTER READYSET RESNAPSHOT TABLE t")
        .await
        .unwrap();

    eventually! {
        let new_table = get_table_index(&mut handle, "noria", "t").await;
        new_table.is_some() && new_table != old_table
    }

    shutdown_tx.shutdown().await;
}
