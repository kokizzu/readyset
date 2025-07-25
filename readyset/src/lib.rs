#![deny(macro_use_extern_crate)]

pub mod mysql;
pub mod psql;
pub mod query_logger;
use std::collections::{HashMap, HashSet};
use std::fs::remove_dir_all;
use std::future::Future;
use std::io;
use std::marker::Send;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail};
use clap::builder::NonEmptyStringValueParser;
use clap::{ArgAction, ArgGroup, Parser};
use crossbeam_skiplist::SkipSet;
use database_utils::{DatabaseType, DatabaseURL, TlsMode, UpstreamConfig};
use failpoint_macros::set_failpoint;
use futures_util::future::FutureExt;
use futures_util::stream::{SelectAll, StreamExt};
use health_reporter::{HealthReporter as AdapterHealthReporter, State as AdapterState};
use readyset_adapter::backend::noria_connector::{NoriaConnector, ReadBehavior};
use readyset_adapter::backend::{MigrationMode, UnsupportedSetMode};
use readyset_adapter::http_router::NoriaAdapterHttpRouter;
use readyset_adapter::metrics_handle::MetricsHandle;
use readyset_adapter::migration_handler::MigrationHandler;
use readyset_adapter::proxied_queries_reporter::ProxiedQueriesReporter;
use readyset_adapter::query_status_cache::{MigrationStyle, QueryStatusCache};
use readyset_adapter::views_synchronizer::ViewsSynchronizer;
use readyset_adapter::{
    Backend, BackendBuilder, DeploymentMode, QueryHandler, ReadySetStatusReporter, UpstreamDatabase,
};
use readyset_alloc::{StdThreadBuildWrapper, ThreadBuildWrapper};
use readyset_alloc_metrics::report_allocator_metrics;
use readyset_client::consensus::AuthorityType;
use readyset_client::metrics::recorded;
use readyset_client::ReadySetHandle;
use readyset_client_metrics::QueryLogMode;
use readyset_common::ulimit::maybe_increase_nofile_limit;
use readyset_data::upstream_system_props::{init_system_props, UpstreamSystemProperties};
use readyset_dataflow::Readers;
use readyset_errors::{internal_err, ReadySetError};
use readyset_server::metrics::{CompositeMetricsRecorder, MetricsRecorder};
use readyset_server::worker::readers::{retry_misses, Ack, BlockingRead, ReadRequestHandler};
use readyset_server::PrometheusBuilder;
use readyset_sql::ast::Relation;
use readyset_sql_passes::adapter_rewrites::AdapterRewriteParams;
use readyset_telemetry_reporter::{TelemetryBuilder, TelemetryEvent, TelemetryInitializer};
#[cfg(feature = "failure_injection")]
use readyset_util::failpoints;
use readyset_util::futures::abort_on_panic;
use readyset_util::redacted::RedactedString;
use readyset_util::shared_cache::SharedCache;
use readyset_util::shutdown;
use readyset_version::*;
use tokio::net;
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tokio_stream::wrappers::TcpListenerStream;
use tracing::{debug, debug_span, error, info, info_span, span, warn, Level};
use tracing_futures::Instrument;

use std::io::Read;
use tokio_native_tls::{native_tls, TlsAcceptor};

// readyset_alloc initializes the global allocator
extern crate readyset_alloc;

/// Timeout to use when connecting to the upstream database
const UPSTREAM_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Retry interval to use when attempting to connect to the upstream database
const UPSTREAM_CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub trait ConnectionHandler {
    type UpstreamDatabase: UpstreamDatabase;
    type Handler: QueryHandler;

    fn process_connection(
        &mut self,
        stream: net::TcpStream,
        backend: Backend<Self::UpstreamDatabase, Self::Handler>,
    ) -> impl Future<Output = ()> + Send;

    /// Return an immediate error to a newly-established connection, then immediately disconnect
    fn immediate_error(
        self,
        stream: net::TcpStream,
        error_message: String,
    ) -> impl Future<Output = ()> + Send;
}

/// Parse and normalize the given string as an [`IpAddr`]
pub fn resolve_addr(addr: &str) -> anyhow::Result<IpAddr> {
    Ok(format!("{addr}:0")
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("Could not resolve address: {}", addr))?
        .ip())
}

pub struct NoriaAdapter<H>
where
    H: ConnectionHandler,
{
    pub description: &'static str,
    pub default_address: SocketAddr,
    /// Address used to listen for incoming connections
    pub connection_handler: H,
    pub database_type: DatabaseType,
    /// SQL dialect to use when parsing queries
    pub parse_dialect: readyset_sql::Dialect,
    /// Expression evaluation dialect to pass to ReadySet for all migration requests
    pub expr_dialect: readyset_data::Dialect,
}

#[derive(Parser, Debug)]
#[command(group(
    ArgGroup::new("metrics")
        .multiple(true)
        .args(&["prometheus_metrics", "noria_metrics"]),
), version = VERSION_STR_PRETTY)]
#[group(skip)]
pub struct Options {
    /// IP:PORT to listen on
    #[arg(long, short = 'a', env = "LISTEN_ADDRESS")]
    address: Option<SocketAddr>,

    /// ReadySet deployment ID. All nodes in a deployment must have the same deployment ID.
    #[arg(long, env = "DEPLOYMENT", default_value = "readyset.db", value_parser = NonEmptyStringValueParser::new(), hide = true)]
    deployment: String,

    /// Database engine protocol to emulate. If omitted, will be inferred from the
    /// `upstream-db-url`
    #[arg(
        long,
        env = "DATABASE_TYPE",
        value_enum,
        required_unless_present("upstream_db_url"),
        hide = true
    )]
    pub database_type: Option<DatabaseType>,

    /// Run ReadySet in standalone mode (the default), embedded readers mode, or adapter mode.
    ///
    /// When running in standalone mode, this process will run an entire deployment of ReadySet
    /// (both adapter and server).
    ///
    /// When running in embedded_readers mode, this process will run a ReadySet adapter with reader
    /// replicas (and only reader replicas) embedded in the adapter.  This mode should be combined
    /// with `--no-readers` and `--reader-replicas` set to the number of adapter instances to each
    /// server process.
    ///
    /// When running in adapter mode, this process will run a ReadySet adapter with no locally
    /// cached data.
    #[arg(
        long,
        env = "DEPLOYMENT_MODE",
        default_value = "standalone",
        hide = false
    )]
    deployment_mode: DeploymentMode,

    /// The authority to use
    // NOTE: hidden because the value can be derived from `--deployment-mode standalone`
    #[arg(
        long,
        env = "AUTHORITY",
        default_value_if("deployment_mode", "standalone", Some("standalone")),
        default_value = "consul",
        hide = true
    )]
    authority: AuthorityType,

    /// Authority uri
    // NOTE: `authority_address` should come after `authority` for clap to set default values
    // properly
    #[arg(
        long,
        env = "AUTHORITY_ADDRESS",
        default_value_if("authority", "standalone", Some(".")),
        default_value_if("authority", "consul", Some("127.0.0.1:8500")),
        required = false,
        hide = true
    )]
    authority_address: String,

    /// Log slow queries (> 5ms)
    #[arg(long, hide = true)]
    log_slow: bool,

    /// Don't require authentication for any client connections
    #[arg(long, env = "ALLOW_UNAUTHENTICATED_CONNECTIONS", hide = true)]
    allow_unauthenticated_connections: bool,

    /// Specify the migration mode for ReadySet to use. The default "explicit" mode is the only
    /// non-experimental mode.
    #[arg(long, env = "QUERY_CACHING", default_value = "explicit", hide = true)]
    query_caching: MigrationStyle,

    /// Sets the maximum time in minutes that we will retry migrations for in the
    /// migration handler. If this time is reached, the query will be exclusively
    /// sent to the upstream database.
    ///
    /// Defaults to 15 minutes.
    #[arg(
        long,
        env = "MAX_PROCESSING_MINUTES",
        default_value = "15",
        hide = true
    )]
    max_processing_minutes: u64,

    /// Sets the migration handlers's loop interval in milliseconds.
    #[arg(
        long,
        env = "MIGRATION_TASK_INTERVAL",
        default_value = "20000",
        hide = true
    )]
    migration_task_interval: u64,

    /// IP:PORT to host endpoint for scraping metrics from the adapter.
    #[arg(long, env = "METRICS_ADDRESS", default_value = "0.0.0.0:6034")]
    metrics_address: SocketAddr,

    /// Comma list of allowed usernames:passwords to authenticate database connections with.
    /// If not set, the username and password in --upstream-db-url will be used.
    /// If --allow-unauthenticated-connections is passed, this will be ignored.
    #[arg(long, env = "ALLOWED_USERS")]
    allowed_users: Option<RedactedString>,

    /// Enable recording and exposing Prometheus metrics
    #[arg(long, env = "PROMETHEUS_METRICS", default_value = "true", hide = true)]
    prometheus_metrics: bool,

    #[arg(long, hide = true)]
    noria_metrics: bool,

    /// Enable logging queries and execution metrics. This creates a histogram per unique query.
    /// Enabled by default if prometheus-metrics is enabled.
    #[arg(
        long,
        env = "QUERY_LOG_MODE",
        requires = "metrics",
        default_value = "enabled",
        default_value_if("prometheus_metrics", "true", Some("enabled")),
        hide = true
    )]
    query_log_mode: QueryLogMode,

    /// IP address to advertise to other ReadySet instances running in the same deployment.
    ///
    /// If not specified, defaults to the value of `address`
    #[arg(long, env = "EXTERNAL_ADDRESS", value_parser = resolve_addr, hide = true)]
    external_address: Option<IpAddr>,

    #[command(flatten)]
    pub tracing: readyset_tracing::Options,

    /// readyset-psql-specific options
    #[command(flatten)]
    pub psql_options: psql::Options,

    /// Configure how ReadySet behaves when receiving unsupported SET statements.
    ///
    /// The possible values are:
    ///
    /// * "error" - return an error to the client
    /// * "proxy" (default) - proxy all subsequent statements
    /// * "allow" - allow and ignore all unsupported set statements
    #[arg(
        long,
        env = "UNSUPPORTED_SET_MODE",
        default_value = "proxy",
        hide = true
    )]
    unsupported_set_mode: UnsupportedSetMode,

    // TODO(DAN): require explicit migrations
    /// Specifies the polling interval in seconds for requesting views from the Leader.
    #[arg(long, env = "VIEWS_POLLING_INTERVAL", default_value = "5", hide = true)]
    views_polling_interval: u64,

    /// The time to wait before canceling a migration request. Defaults to 0 (unlimited).
    #[arg(
        long,
        hide = true,
        env = "MIGRATION_REQUEST_TIMEOUT_MS",
        default_value = "0"
    )]
    migration_request_timeout_ms: u64,

    /// The time to wait before canceling a controller request. Defaults to 5 seconds (0 specifies
    /// unlimited).
    #[arg(
        long,
        hide = true,
        env = "CONTROLLER_REQUEST_TIMEOUT_MS",
        default_value = "5000"
    )]
    controller_request_timeout_ms: u64,

    /// Specifies the maximum continuous failure time for any given query, in seconds, before
    /// entering into a fallback recovery mode.
    #[arg(
        long,
        hide = true,
        env = "QUERY_MAX_FAILURE_SECONDS",
        default_value = "9223372036854775"
    )]
    query_max_failure_seconds: u64,

    /// Specifies the recovery period in seconds that we enter if a given query fails for the
    /// period of time designated by the query_max_failure_seconds flag.
    #[arg(
        long,
        hide = true,
        env = "FALLBACK_RECOVERY_SECONDS",
        default_value = "0"
    )]
    fallback_recovery_seconds: u64,

    /// Whether to use non-blocking or blocking reads against the cache.
    #[arg(long, env = "NON_BLOCKING_READS", hide = true)]
    non_blocking_reads: bool,

    #[command(flatten)]
    server_worker_options: readyset_server::WorkerOptions,

    /// Whether to disable telemetry reporting. Defaults to false.
    #[arg(long, env = "DISABLE_TELEMETRY")]
    disable_telemetry: bool,

    /// Whether we should wait for a failpoint request to the adapters http router, which may
    /// impact startup.
    #[arg(long, hide = true)]
    wait_for_failpoint: bool,

    /// Whether to allow ReadySet to automatically create inlined caches when we receive a CREATE
    /// CACHE command for a query with unsupported placeholders.
    ///
    /// If set, we will create a cache with literals inlined in the unsupported placeholder
    /// positions every time the statement is executed with a new set of parameters.
    // XXX JCD keep features synchronized with readyset-features.json
    #[arg(
        long,
        env = "FEATURE_PLACEHOLDER_INLINING",
        default_value = "false",
        default_missing_value = "true",
        num_args = 0..=1,
        action = ArgAction::Set,
        hide = true
    )]
    feature_placeholder_inlining: bool,

    /// Don't make connections to the upstream database for new client connections.
    ///
    /// If this flag is set queries will never be proxied upstream - even if they are unsupported,
    /// fail to execute, or are run in a transaction.
    #[arg(long, env = "NO_UPSTREAM_CONNECTIONS", hide = true)]
    no_upstream_connections: bool,

    /// If supplied we will clean up assets for the supplied deployment. If an upstream url is
    /// supplied, we will also clean up various assets related to upstream (replication slot, etc.)
    #[arg(long)]
    cleanup: bool,

    /// In [`DeploymentMode::Standalone`] or  [`DeploymentMode::EmbeddedReaders`] mode,
    /// the IP address on which the ReadySet will listen.
    #[arg(long, env = "CONTROLLER_ADDRESS", hide = true)]
    controller_address: Option<IpAddr>,

    /// The number of queries that will be retained and eligible to be returned by `show caches`
    /// and `show proxied queries`.
    #[arg(
        long,
        env = "QUERY_STATUS_CAPACITY",
        default_value = "100000",
        hide = true
    )]
    query_status_capacity: usize,

    /// Which address to listen on and also allow cache DDL statements to be executed from.
    /// If not set, cache ddl statements can be executed from any address
    /// If set, `address` will reject cache ddl statements.
    #[arg(long, env = "CACHE_DDL_ADDRESS", hide = true)]
    cache_ddl_address: Option<SocketAddr>,

    /// The pkcs12 identity file (certificate and key) used by ReadySet for establishing TLS
    /// connections as the server.
    ///
    /// ReadySet will not accept TLS connections if there is no identity file specified.
    #[arg(long, env = "READYSET_IDENTITY_FILE")]
    readyset_identity_file: Option<String>,

    /// Password for the pkcs12 identity file used by ReadySet for establishing TLS connections as
    /// the server.
    ///
    /// If password is not provided, ReadySet will try using an empty string to unlock the identity
    /// file.
    #[arg(
        long,
        requires = "readyset_identity_file",
        env = "READYSET_IDENTITY_FILE_PASSWORD"
    )]
    readyset_identity_file_password: Option<String>,

    /// Specifies the types of client connections permitted to connect to Readyset.
    ///
    /// The available options are:
    ///
    /// * "optional" (default) - Clients can connect using either plain or TLS connections.
    /// * "disabled" - TLS connections are not allowed; only plain connections are permitted.
    /// * "required" - Only TLS connections are allowed; plain connections are rejected.
    #[arg(long, env = "TLS_MODE", default_value = "optional")]
    pub tls_mode: TlsMode,
}

impl Options {
    /// Extract database type from a URL string
    ///
    /// # Input
    ///
    /// - `url` - A string representing a database URL
    ///
    /// # Output
    ///
    /// - A `DatabaseType` representing the database type
    fn infer_database_type_from_url(&self, url: &str) -> anyhow::Result<DatabaseType> {
        Ok(url.parse::<DatabaseURL>()?.database_type())
    }

    /// Check that the user has provided the same database type for both the upstream and cdc URLs
    ///
    /// # Output
    ///
    /// - An `anyhow::Result` indicating whether the database types match
    fn check_replication_and_cdc_urls(&self) -> anyhow::Result<()> {
        if let Some(url) = &self.server_worker_options.replicator_config.upstream_db_url {
            let inferred = self.infer_database_type_from_url(url)?;
            if let Some(cdc_url) = &self.server_worker_options.replicator_config.cdc_db_url {
                let cdc_inferred = self.infer_database_type_from_url(cdc_url)?;
                if inferred != cdc_inferred {
                    bail!(
                        "Database type for --upstream-db-url ({}) does not match \
                         database type for --cdc-db-url ({})",
                        inferred,
                        cdc_inferred
                    );
                }
            }
        }

        Ok(())
    }

    /// Check that the user has provided a database type or an upstream URL
    /// If the user has provided both, we will check that the database types match
    ///
    /// # Output
    ///
    /// - An `anyhow::Result` indicating whether the database types match and the database type
    pub fn database_type(&self) -> anyhow::Result<DatabaseType> {
        self.check_replication_and_cdc_urls()?;
        match (
            self.database_type,
            &self.server_worker_options.replicator_config.upstream_db_url,
        ) {
            (None, None) => bail!("One of either --database-type or --upstream-db-url is required"),
            (None, Some(url)) => self.infer_database_type_from_url(url),
            (Some(dt), None) => Ok(dt),
            (Some(dt), Some(url)) => {
                let inferred = self.infer_database_type_from_url(url)?;
                if dt != inferred {
                    bail!(
                        "Provided --database-type {dt} does not match database type {inferred} for \
                         --upstream-db-url"
                    );
                }
                Ok(dt)
            }
        }
    }

    pub fn tls_acceptor(&self) -> anyhow::Result<Option<Arc<TlsAcceptor>>> {
        let Some(ref path) = self.readyset_identity_file else {
            return Ok(None);
        };

        let mut identity_file = std::fs::File::open(path)?;
        let mut identity = vec![];
        identity_file.read_to_end(&mut identity)?;

        let password = self
            .readyset_identity_file_password
            .clone()
            .unwrap_or_default();

        let tls_identity = native_tls::Identity::from_pkcs12(&identity, &password)?;

        Ok(Some(Arc::new(TlsAcceptor::from(
            native_tls::TlsAcceptor::new(tls_identity)?,
        ))))
    }

    fn process_pair(
        &self,
        pair: &str,
        seen_users: &mut HashSet<String>,
    ) -> Result<(String, String), anyhow::Error> {
        let mut parts = pair.trim().splitn(2, ':');
        match (parts.next(), parts.next()) {
            (Some(user), Some(pass)) => {
                let user = user.trim();
                let pass = pass.trim();
                if user.is_empty() || pass.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Invalid user:password pair format. Expected format: user:password"
                    ));
                }
                if !seen_users.insert(user.to_string()) {
                    return Err(anyhow::anyhow!("Duplicate user found: {}", user));
                }
                Ok((user.to_string(), pass.to_string()))
            }
            _ => Err(anyhow::anyhow!(
                "Invalid user:password pair format. Expected format: user:password"
            )),
        }
    }

    // Build list of allowed user to connect to Readyset
    fn build_allowed_users(&self) -> Result<HashMap<String, String>, anyhow::Error> {
        let upstream_config = self.server_worker_options.replicator_config.clone();
        let upstream_url = upstream_config
            .upstream_db_url
            .as_ref()
            .and_then(|s| s.parse::<DatabaseURL>().ok());
        let mut seen_users = std::collections::HashSet::new();
        // Parse allowed users from comma-separated "user:pass" pairs
        let mut allowed_users = self
            .allowed_users
            .as_ref()
            .map(|s| {
                let mut users = HashMap::new();
                let mut current = String::new();
                let mut in_quotes = false;
                let mut quote_char = None;

                // Parse character by character
                for (i, c) in s.chars().enumerate() {
                    match c {
                        '\'' | '"' if !in_quotes => {
                            in_quotes = true;
                            quote_char = Some(c);
                        }
                        c if Some(c) == quote_char => {
                            if let Some(next_c) = s.chars().nth(i + 1) {
                                if next_c == c {
                                    // Handle escaped quote
                                    current.push(c);
                                    continue; // Skip next quote
                                }
                            }
                            in_quotes = false;
                            quote_char = None;
                        }
                        ',' if !in_quotes => {
                            if !current.is_empty() {
                                let (user, pass) = self.process_pair(&current, &mut seen_users)?;
                                users.insert(user, pass);
                                current.clear();
                            }
                        }
                        _ => current.push(c),
                    }
                }

                // Process the last pair if any
                if !current.is_empty() {
                    let (user, pass) = self.process_pair(&current, &mut seen_users)?;
                    users.insert(user, pass);
                }

                if in_quotes {
                    return Err(anyhow::anyhow!("Unclosed quote in input"));
                }

                Ok(users)
            })
            .transpose()?
            .unwrap_or_default();

        match (
            upstream_url.as_ref().and_then(|url| url.user()),
            upstream_url.as_ref().and_then(|url| url.password()),
        ) {
            (Some(user), Some(pass)) => {
                if seen_users.insert(user.to_owned()) {
                    allowed_users.insert(user.to_owned(), pass.to_owned())
                } else {
                    return Err(anyhow::anyhow!("Duplicate user found: {}", user));
                }
            }
            _ => None,
        };

        Ok(allowed_users)
    }
}

async fn connect_upstream<U>(
    upstream_config: UpstreamConfig,
    no_upstream_connections: bool,
) -> Result<Option<U>, U::Error>
where
    U: UpstreamDatabase,
{
    if upstream_config.upstream_db_url.is_some() && !no_upstream_connections {
        set_failpoint!(failpoints::UPSTREAM);
        timeout(
            UPSTREAM_CONNECTION_TIMEOUT,
            U::connect(upstream_config, None, None),
        )
        .instrument(debug_span!("Connecting to upstream database"))
        .await
        .map_err(|_| internal_err!("Connection timed out").into())
        .and_then(|r| r)
        .map(Some)
    } else {
        Ok(None)
    }
}

/// Spawn a task to query the upstream for its currently-configured schema search path and
/// timezone name in a loop until it succeeds, returning a lock that will contain the result
/// when it finishes
///
/// NOTE: when we start tracking all configuration parameters, this should be folded into
/// whatever loads those initially
async fn load_system_props<U>(
    upstream_config: UpstreamConfig,
    no_upstream_conns: bool,
) -> Arc<RwLock<Result<UpstreamSystemProperties, U::Error>>>
where
    U: UpstreamDatabase,
{
    if no_upstream_conns {
        return Arc::new(RwLock::new(Ok(UpstreamSystemProperties {
            search_path: upstream_config.default_schema_search_path(),
            timezone_name: upstream_config.default_timezone_name(),
            ..Default::default()
        })));
    }

    let try_load = move |upstream_config: UpstreamConfig| async move {
        let upstream = connect_upstream::<U>(upstream_config.clone(), no_upstream_conns).await?;

        let Some(mut upstream) = upstream else {
            return Ok(Default::default());
        };

        let search_path = upstream.schema_search_path().await?;
        let timezone_name = upstream.timezone_name().await?;
        let lower_case_database_names = upstream.lower_case_database_names().await?;
        let lower_case_table_names = upstream.lower_case_table_names().await?;

        Ok(UpstreamSystemProperties {
            search_path,
            timezone_name,
            lower_case_database_names,
            lower_case_table_names,
        })
    };

    // First, try to load once outside the loop
    let e = match try_load(upstream_config.clone()).await {
        Ok(res) => return Arc::new(RwLock::new(Ok(res))),
        Err(error) => {
            warn!(%error, "Loading initial upstream system properties failed, spawning retry loop");
            error
        }
    };

    // If that fails, spawn a task to keep retrying
    let out = Arc::new(RwLock::new(Err(e)));
    tokio::spawn({
        let out = Arc::clone(&out);
        async move {
            let mut first_loop = true;
            loop {
                if !first_loop {
                    sleep(UPSTREAM_CONNECTION_RETRY_INTERVAL).await;
                }
                first_loop = false;

                let res = try_load(upstream_config.clone()).await;

                if let Ok(ssp) = &res {
                    debug!(?ssp, "Successfully loaded schema search path from upstream");
                }

                let was_ok = res.is_ok();
                *out.write().await = res;
                if was_ok {
                    break;
                }
            }
        }
    });

    out
}

impl<H> NoriaAdapter<H>
where
    H: ConnectionHandler + Clone + Send + Sync + 'static,
{
    pub fn run(&mut self, options: Options) -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .with_sys_hooks()
            .enable_all()
            .thread_name("Adapter Runtime")
            .build()?;

        let _guard =
            rt.block_on(async { options.tracing.init("adapter", options.deployment.as_ref()) })?;
        info!(?options, "Starting ReadySet adapter");

        if options.deployment_mode.is_standalone() {
            maybe_increase_nofile_limit(
                options
                    .server_worker_options
                    .replicator_config
                    .ignore_ulimit_check,
            )?;
        }

        let deployment_dir = options
            .server_worker_options
            .storage_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(&options.deployment);

        let upstream_config = options.server_worker_options.replicator_config.clone();

        if options.cleanup {
            info!(?options, "Cleaning up deployment");

            return rt.block_on(async { self.cleanup(upstream_config, deployment_dir).await });
        }
        let users = options.build_allowed_users()?;
        let users: &'static HashMap<String, String> = if !options.allow_unauthenticated_connections
        {
            if users.is_empty() {
                bail!(
                    "Failed to build authentication map from \
                upstream DB URL or --allowed-users. Please ensure they are present and \
                correctly formatted as follows: --upstream-db-url <protocol>://<username>:<password>@<address>[:<port>][/<database>] \
                or --allowed-users <username:password>[,<username:password>...]"
                )
            }
            Box::leak(Box::new(users))
        } else {
            Box::leak(Box::new(HashMap::new()))
        };

        info!(version = %VERSION_STR_ONELINE);

        if matches!(options.unsupported_set_mode, UnsupportedSetMode::Allow) {
            warn!(
                "Running with --unsupported-set-mode allow can cause certain queries to return \
                 incorrect results"
            )
        }

        let listen_address = options.address.unwrap_or(self.default_address);
        let listener = rt.block_on(tokio::net::TcpListener::bind(&listen_address))?;
        let mut all_listeners = SelectAll::new();
        all_listeners.push(TcpListenerStream::new(listener));

        if let Some(ref ddl_addr) = options.cache_ddl_address {
            info!(%ddl_addr, "Listening for cache ddl connections");
            let cache_ddl_listener = rt.block_on(tokio::net::TcpListener::bind(ddl_addr))?;
            all_listeners.push(TcpListenerStream::new(cache_ddl_listener));
        }

        info!(%listen_address, "Listening for new connections");

        let auto_increments: Arc<RwLock<HashMap<Relation, AtomicUsize>>> = Arc::default();
        let view_name_cache = SharedCache::new();
        let view_cache = SharedCache::new();
        let connections: Arc<SkipSet<SocketAddr>> = Arc::default();
        let mut health_reporter = AdapterHealthReporter::new();

        let rs_connect = span!(Level::INFO, "Connecting to RS server");
        rs_connect.in_scope(|| info!(%options.authority_address, %options.deployment));

        let authority_type = options.authority.clone();
        let authority_address = match authority_type {
            AuthorityType::Standalone => deployment_dir
                .clone()
                .into_os_string()
                .into_string()
                .unwrap_or_else(|_| options.authority_address.clone()),
            _ => options.authority_address.clone(),
        };
        let deployment = options.deployment.clone();
        let adapter_authority =
            Arc::new(authority_type.to_authority(&authority_address, &deployment));

        let adapter_rewrite_params = AdapterRewriteParams {
            dialect: self.database_type.into(),
            server_supports_topk: options.server_worker_options.feature_topk,
            server_supports_pagination: options.server_worker_options.feature_topk
                && options.server_worker_options.feature_pagination,
            server_supports_mixed_comparisons: options
                .server_worker_options
                .feature_mixed_comparisons,
        };
        let no_upstream_connections = options.no_upstream_connections;

        let migration_request_timeout = if options.migration_request_timeout_ms > 0 {
            Some(Duration::from_millis(options.migration_request_timeout_ms))
        } else {
            None
        };
        let controller_request_timeout = if options.controller_request_timeout_ms > 0 {
            Some(Duration::from_millis(options.controller_request_timeout_ms))
        } else {
            None
        };
        let rh = rt.block_on(async {
            Ok::<ReadySetHandle, ReadySetError>(
                ReadySetHandle::with_timeouts(
                    adapter_authority.clone(),
                    controller_request_timeout,
                    migration_request_timeout,
                )
                .instrument(rs_connect.clone())
                .await,
            )
        })?;

        rs_connect.in_scope(|| info!("ReadySetHandle created"));

        let status_reporter = ReadySetStatusReporter::new(
            upstream_config.clone(),
            Some(rh.clone()),
            connections.clone(),
            adapter_authority.clone(),
        );
        let ctrlc = tokio::signal::ctrl_c();
        let mut sigterm = {
            let _guard = rt.enter();
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap()
        };
        let mut listener = Box::pin(futures_util::stream::select(
            all_listeners,
            futures_util::stream::select(
                ctrlc
                    .map(|r| {
                        r?;
                        Err(io::Error::new(io::ErrorKind::Interrupted, "got ctrl-c"))
                    })
                    .into_stream(),
                sigterm
                    .recv()
                    .map(futures_util::stream::iter)
                    .into_stream()
                    .flatten()
                    .map(|_| Err(io::Error::new(io::ErrorKind::Interrupted, "got SIGTERM"))),
            ),
        ));
        rs_connect.in_scope(|| info!("Now capturing ctrl-c and SIGTERM events"));

        let mut recorders = Vec::new();
        let prometheus_handle = if options.prometheus_metrics {
            let _guard = rt.enter();
            let database_label = match self.database_type {
                DatabaseType::MySQL => "mysql",
                DatabaseType::PostgreSQL => "psql",
            };

            let recorder = PrometheusBuilder::new()
                .add_global_label("upstream_db_type", database_label)
                .add_global_label("deployment", &options.deployment)
                .build_recorder();

            let handle = recorder.handle();
            recorders.push(MetricsRecorder::Prometheus(recorder));
            Some(handle)
        } else {
            None
        };

        if options.noria_metrics {
            recorders.push(MetricsRecorder::Noria(
                readyset_server::NoriaMetricsRecorder::new(),
            ));
        }

        if !recorders.is_empty() {
            readyset_server::metrics::install_global_recorder(
                CompositeMetricsRecorder::with_recorders(recorders),
            );
        }

        rs_connect.in_scope(|| info!("PrometheusHandle created"));

        metrics::gauge!(
            recorded::READYSET_ADAPTER_VERSION,
            &[
                ("release_version", READYSET_VERSION.release_version),
                ("commit_id", READYSET_VERSION.commit_id),
                ("platform", READYSET_VERSION.platform),
                ("rustc_version", READYSET_VERSION.rustc_version),
                ("profile", READYSET_VERSION.profile),
                ("profile", READYSET_VERSION.profile),
                ("opt_level", READYSET_VERSION.opt_level),
            ]
        )
        .set(1.0);
        metrics::counter!(recorded::READYSET_ADAPTER_STARTUPS).increment(1);
        let adapter_start_time = SystemTime::now();

        let (shutdown_tx, shutdown_rx) = shutdown::channel();

        // if we're running in standalone mode, server will already
        // spawn it's own allocator metrics reporter.
        if prometheus_handle.is_some() && !options.deployment_mode.is_standalone() {
            let alloc_shutdown = shutdown_rx.clone();
            rt.handle().spawn(report_allocator_metrics(alloc_shutdown));
        }

        // Gate query log code path on the log flag existing.
        let qlog_sender = if options.query_log_mode.is_enabled() {
            rs_connect.in_scope(|| info!("Query logs are enabled. Spawning query logger"));
            let (qlog_sender, qlog_receiver) = tokio::sync::mpsc::unbounded_channel();

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap();

            let shutdown_rx = shutdown_rx.clone();
            // Spawn the actual thread to run the logger
            let query_log_mode = options.query_log_mode;
            let rewrite_params = adapter_rewrite_params;
            let dialect = self.parse_dialect;
            std::thread::Builder::new()
                .name("Query logger".to_string())
                .stack_size(2 * 1024 * 1024) // Use the same value tokio is using
                .spawn_wrapper(move || {
                    let mut logger =
                        query_logger::QueryLogger::new(query_log_mode, dialect, rewrite_params);
                    runtime.block_on(logger.run(qlog_receiver, shutdown_rx));
                    runtime.shutdown_background();
                })?;

            Some(qlog_sender)
        } else {
            rs_connect.in_scope(|| info!("Query logs are disabled"));
            None
        };

        let noria_read_behavior = if options.non_blocking_reads {
            rs_connect.in_scope(|| info!("Will perform NonBlocking Reads"));
            ReadBehavior::NonBlocking
        } else {
            rs_connect.in_scope(|| info!("Will perform Blocking Reads"));
            ReadBehavior::Blocking
        };

        let migration_style = options.query_caching;

        rs_connect.in_scope(|| info!(?migration_style));

        let query_status_cache: &'static _ = Box::leak(Box::new(
            QueryStatusCache::with_capacity(options.query_status_capacity)
                .style(migration_style)
                .set_placeholder_inlining(options.feature_placeholder_inlining),
        ));

        let telemetry_sender = rt.block_on(async {
            let proxied_queries_reporter =
                Arc::new(ProxiedQueriesReporter::new(query_status_cache));
            TelemetryInitializer::init(
                options.disable_telemetry,
                std::env::var("RS_API_KEY").ok(),
                vec![proxied_queries_reporter],
                options.deployment.clone(),
            )
            .await
        });

        let _ = telemetry_sender
            .send_event_with_payload(
                TelemetryEvent::AdapterStart,
                TelemetryBuilder::new()
                    .adapter_version(option_env!("CARGO_PKG_VERSION").unwrap_or_default())
                    .db_backend(format!("{:?}", &self.database_type).to_lowercase())
                    .build(),
            )
            .map_err(|error| warn!(%error, "Failed to initialize telemetry sender"));

        let migration_mode = match migration_style {
            MigrationStyle::Async | MigrationStyle::Explicit => MigrationMode::OutOfBand,
            MigrationStyle::InRequestPath => MigrationMode::InRequestPath,
        };

        rs_connect.in_scope(|| info!(?migration_mode));

        // Spawn a task for handling this adapter's HTTP request server.
        // This step is done as the last thing before accepting connections because it is used as
        // the health check for the service.
        rs_connect.in_scope(|| info!("Spawning HTTP request server task"));
        let (tx, rx) = if options.wait_for_failpoint {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            (Some(Arc::new(tx)), Some(rx))
        } else {
            (None, None)
        };
        let http_server = NoriaAdapterHttpRouter {
            listen_addr: options.metrics_address,
            prometheus_handle: prometheus_handle.clone(),
            health_reporter: health_reporter.clone(),
            failpoint_channel: tx,
            metrics: Default::default(),
            status_reporter: status_reporter.clone(),
        };

        let router_shutdown_rx = shutdown_rx.clone();
        let fut = async move {
            let http_listener = http_server.create_listener().await.unwrap();
            NoriaAdapterHttpRouter::route_requests(http_server, http_listener, router_shutdown_rx)
                .await
        };

        rt.handle().spawn(fut);

        // If we previously setup a failpoint channel because wait_for_failpoint was enabled,
        // then we should wait to hear from the http router that a failpoint request was
        // handled.
        if let Some(mut rx) = rx {
            let fut = async move {
                let _ = rx.recv().await;
            };
            rt.block_on(fut);
        }

        let sys_props = rt.block_on(load_system_props::<H::UpstreamDatabase>(
            upstream_config.clone(),
            no_upstream_connections,
        ));

        if let MigrationMode::OutOfBand = migration_mode {
            set_failpoint!(failpoints::ADAPTER_OUT_OF_BAND);
            let rh = rh.clone();
            let auto_increments = auto_increments.clone();
            let view_name_cache = view_name_cache.clone();
            let view_cache = view_cache.clone();
            let mut shutdown_rx = shutdown_rx.clone();
            let loop_interval = options.migration_task_interval;
            let max_retry = options.max_processing_minutes;
            let dry_run = matches!(migration_style, MigrationStyle::Explicit);
            let expr_dialect = self.expr_dialect;
            let parse_dialect = self.parse_dialect;
            let sys_props = Arc::clone(&sys_props);

            rs_connect.in_scope(|| info!("Spawning migration handler task"));
            let fut = async move {
                let connection = span!(Level::INFO, "migration task upstream database connection");

                let sys_props = {
                    let retry_loop = async {
                        loop {
                            if let Ok(v) = &*sys_props.read().await {
                                break v.clone();
                            }
                            sleep(UPSTREAM_CONNECTION_RETRY_INTERVAL).await
                        }
                    };

                    tokio::select! {
                            v = retry_loop => v,
                            _ = shutdown_rx.recv() => return Ok(()),
                    }
                };

                if let Err(e) = init_system_props(&sys_props) {
                    info!("{e}");
                }

                let noria =
                    NoriaConnector::new(
                        rh.clone(),
                        auto_increments,
                        view_name_cache.new_local(),
                        view_cache.new_local(),
                        noria_read_behavior,
                        expr_dialect,
                        parse_dialect,
                        sys_props.search_path,
                        adapter_rewrite_params,
                    )
                    .instrument(connection.in_scope(|| {
                        span!(Level::DEBUG, "Building migration task noria connector")
                    }))
                    .await;

                let controller_handle = dry_run.then(|| rh.clone());
                let mut migration_handler = MigrationHandler::new(
                    noria,
                    controller_handle,
                    query_status_cache,
                    expr_dialect,
                    std::time::Duration::from_millis(loop_interval),
                    std::time::Duration::from_secs(max_retry * 60),
                    shutdown_rx.clone(),
                );

                migration_handler.run().await.map_err(move |e| {
                    error!(error = %e, "Migration Handler failed, aborting the process due to service entering a degraded state");
                    std::process::abort()
                })
            };

            rt.handle().spawn(abort_on_panic(fut));
        }

        if matches!(migration_style, MigrationStyle::Explicit) {
            rs_connect.in_scope(|| info!("Spawning explicit migrations task"));
            let rh = rh.clone();
            let loop_interval = options.views_polling_interval;
            let expr_dialect = self.expr_dialect;
            let shutdown_rx = shutdown_rx.clone();
            let view_name_cache = view_name_cache.clone();
            let fut = async move {
                let mut views_synchronizer = ViewsSynchronizer::new(
                    rh,
                    query_status_cache,
                    std::time::Duration::from_secs(loop_interval),
                    expr_dialect,
                    view_name_cache.new_local(),
                );
                views_synchronizer.run(shutdown_rx).await
            };
            rt.handle().spawn(abort_on_panic(fut));
        }

        // Create a set of readers on this adapter. This will allow servicing queries directly
        // from readers on the adapter rather than across a network hop.
        let readers: Readers = Arc::new(Mutex::new(Default::default()));

        let parsing_preset = options.server_worker_options.parsing_preset;

        // Run a readyset-server instance within this adapter.
        let internal_server_handle = if options.deployment_mode.has_reader_nodes() {
            let authority = options.authority.clone();
            let deployment = options.deployment.clone();

            let mut builder = readyset_server::Builder::from_worker_options(
                options.server_worker_options,
                &options.deployment,
                deployment_dir,
            );
            let persistence = builder.get_persistence();
            dataflow_state::clean_working_dir(persistence)?;

            let r = readers.clone();

            if options.deployment_mode.is_embedded_readers() {
                builder.as_reader_only();
                builder.cannot_become_leader();
            }

            builder.set_replicator_statement_logging(options.tracing.statement_logging);

            builder.set_telemetry_sender(telemetry_sender.clone());

            if let Some(addr) = options.controller_address {
                builder.set_listen_addr(addr);
            }

            if let Some(external_addr) = options.external_address.or(options.controller_address) {
                builder.set_external_addr(SocketAddr::new(external_addr, 0));
            }

            let server_handle = rt.block_on(async move {
                let authority = Arc::new(authority.to_authority(&authority_address, &deployment));

                builder
                    .start_with_readers(
                        authority,
                        r,
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 4000),
                    )
                    .await
            })?;

            Some(server_handle)
        } else {
            None
        };

        health_reporter.set_state(AdapterState::Healthy);

        rs_connect
            .in_scope(|| info!(supported = %adapter_rewrite_params.server_supports_pagination));

        let expr_dialect = self.expr_dialect;
        let parse_dialect = self.parse_dialect;
        while let Some(Ok(s)) = rt.block_on(listener.next()) {
            let client_addr = s.peer_addr()?;
            let connection = info_span!("connection", addr = %client_addr);
            connection.in_scope(|| debug!("Accepted new connection"));
            s.set_nodelay(true)?;

            // bunch of stuff to move into the async block below
            let rh = rh.clone();
            let adapter_authority = adapter_authority.clone();
            let auto_increments = auto_increments.clone();
            let view_name_cache = view_name_cache.clone();
            let view_cache = view_cache.clone();
            let mut connection_handler = self.connection_handler.clone();
            // If cache_ddl_address is not set, allow cache ddl from all addresses.
            let local_addr = s.local_addr()?;
            let allow_cache_ddl = options
                .cache_ddl_address
                .as_ref()
                .map(|cache_ddl_addr| local_addr == *cache_ddl_addr)
                .unwrap_or(true);
            let backend_builder = BackendBuilder::new()
                .client_addr(client_addr)
                .slowlog(options.log_slow)
                .users(users.clone())
                .allow_cache_ddl(allow_cache_ddl)
                .require_authentication(!options.allow_unauthenticated_connections)
                .dialect(self.parse_dialect)
                .parsing_preset(parsing_preset)
                .query_log_sender(qlog_sender.clone())
                .query_log_mode(Some(options.query_log_mode))
                .unsupported_set_mode(options.unsupported_set_mode)
                .migration_mode(migration_mode)
                .query_max_failure_seconds(options.query_max_failure_seconds)
                .telemetry_sender(telemetry_sender.clone())
                .fallback_recovery_seconds(options.fallback_recovery_seconds)
                .set_placeholder_inlining(options.feature_placeholder_inlining)
                .connections(connections.clone())
                .metrics_handle(prometheus_handle.clone().map(MetricsHandle::new));
            let telemetry_sender = telemetry_sender.clone();

            // Initialize the reader layer for the adapter.
            let r = options.deployment_mode.has_reader_nodes().then(|| {
                // Create a task that repeatedly polls BlockingRead's every `RETRY_TIMEOUT`.
                // When the `BlockingRead` completes, tell the future to resolve with ack.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(BlockingRead, Ack)>();
                rt.handle().spawn(retry_misses(rx));
                ReadRequestHandler::new(readers.clone(), tx, Duration::from_secs(5))
            });

            let upstream_config = upstream_config.clone();
            let sys_props = Arc::clone(&sys_props);
            let status_reporter_clone = status_reporter.clone();
            let fut = async move {
                let upstream_res = connect_upstream::<H::UpstreamDatabase>(
                    upstream_config,
                    no_upstream_connections,
                )
                .await
                .map_err(|e| format!("Error connecting to upstream database: {e}"));

                match upstream_res {
                    Ok(upstream) => {
                        if let Err(e) =
                            telemetry_sender.send_event(TelemetryEvent::UpstreamConnected)
                        {
                            warn!(error = %e, "Failed to send upstream connected metric");
                        }

                        match &*sys_props.read().await {
                            Ok(sys_props) => {
                                if let Err(e) = init_system_props(sys_props) {
                                    info!("{e}");
                                }

                                let noria = NoriaConnector::new_with_local_reads(
                                    rh.clone(),
                                    auto_increments,
                                    view_name_cache.new_local(),
                                    view_cache.new_local(),
                                    noria_read_behavior,
                                    r,
                                    expr_dialect,
                                    parse_dialect,
                                    sys_props.search_path.clone(),
                                    adapter_rewrite_params,
                                )
                                .instrument(debug_span!("Building noria connector"))
                                .await;

                                let backend = backend_builder.clone().build(
                                    noria,
                                    upstream,
                                    query_status_cache,
                                    adapter_authority.clone(),
                                    status_reporter_clone,
                                    adapter_start_time,
                                );
                                connection_handler.process_connection(s, backend).await;
                            }
                            Err(error) => {
                                error!(
                                    %error,
                                    "Error loading initial upstream system properties from ~"
                                );
                                connection_handler
                                    .immediate_error(
                                        s,
                                        format!(
                                            "Error loading initial upstream system properties from \
                                             upstream: {error}"
                                        ),
                                    )
                                    .await;
                            }
                        }
                    }
                    Err(error) => {
                        error!(%error, "Error during initial connection establishment");
                        connection_handler.immediate_error(s, error).await;
                    }
                }

                debug!("disconnected");
            }
            .instrument(connection);

            rt.handle().spawn(fut);
        }

        let rs_shutdown = span!(Level::INFO, "RS server Shutting down");
        health_reporter.set_state(AdapterState::ShuttingDown);

        // We need to drop the last remaining `ShutdownReceiver` before sending the shutdown
        // signal. If we didn't, `ShutdownSender::shutdown` would hang forever, since it
        // specifically waits for every associated `ShutdownReceiver` to be dropped.
        drop(shutdown_rx);

        // Shut down all of our background tasks
        rs_shutdown.in_scope(|| {
            info!("Waiting up to 20 seconds for all background tasks to shut down");
        });
        rt.block_on(shutdown_tx.shutdown_timeout(Duration::from_secs(20)));

        if let Some((_, server_shutdown_tx)) = internal_server_handle {
            rs_shutdown.in_scope(|| info!("Shutting down embedded server task"));
            rt.block_on(server_shutdown_tx.shutdown_timeout(Duration::from_secs(20)));

            // Send server shutdown telemetry event
            let _ = telemetry_sender.send_event(TelemetryEvent::ServerStop);
        }

        // Send adapter shutdown telemetry event
        let _ = telemetry_sender.send_event(TelemetryEvent::AdapterStop);
        rs_shutdown.in_scope(|| {
            info!("Waiting up to 5s for telemetry reporter to drain in-flight metrics")
        });
        rt.block_on(async move {
            match telemetry_sender
                .shutdown(std::time::Duration::from_secs(5))
                .await
            {
                Ok(_) => info!("TelemetrySender shutdown gracefully"),
                Err(e) => info!(error=%e, "TelemetrySender did not shut down gracefully"),
            }
        });

        // We use `shutdown_timeout` instead of `shutdown_background` in case any
        // blocking IO is ongoing.
        rs_shutdown.in_scope(|| info!("Waiting up to 20s for tasks to complete shutdown"));
        rt.shutdown_timeout(std::time::Duration::from_secs(20));
        rs_shutdown.in_scope(|| info!("Shutdown completed successfully"));

        Ok(())
    }

    // TODO: Figure out a way to not require as many flags when --cleanup flag is supplied.
    /// Cleans up the provided deployment, and if an upstream url was provided, will clean up
    /// replication slot and other deployment related assets in the upstream.
    async fn cleanup(
        &mut self,
        upstream_config: UpstreamConfig,
        deployment_dir: PathBuf,
    ) -> anyhow::Result<()> {
        replicators::cleanup(upstream_config).await?;

        if deployment_dir.exists() {
            remove_dir_all(deployment_dir)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Certain clap things, like `requires`, only ever throw an error at runtime, not at
    // compile-time - this tests that none of those happen
    #[test]
    fn arg_parsing_noria_standalone() {
        let opts = Options::parse_from(vec![
            "readyset",
            "--database-type",
            "mysql",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--authority-address",
            "consul:8500",
            "--allow-unauthenticated-connections",
        ]);

        assert_eq!(opts.deployment, "test");
    }

    #[test]
    fn arg_parsing_with_upstream() {
        let opts = Options::parse_from(vec![
            "readyset",
            "--database-type",
            "mysql",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--authority-address",
            "consul:8500",
            "--allow-unauthenticated-connections",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
        ]);

        assert_eq!(opts.deployment, "test");
    }

    #[test]
    fn async_migrations_param_defaults() {
        let opts = Options::parse_from(vec![
            "readyset",
            "--database-type",
            "mysql",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--authority-address",
            "consul:8500",
            "--allow-unauthenticated-connections",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
            "--query-caching=async",
        ]);

        assert_eq!(opts.max_processing_minutes, 15);
        assert_eq!(opts.migration_task_interval, 20000);
    }

    #[test]
    fn infer_database_type() {
        let opts = Options::parse_from(vec![
            "readyset",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
        ]);
        assert_eq!(opts.database_type().unwrap(), DatabaseType::MySQL);

        let opts = Options::parse_from(vec![
            "readyset",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--upstream-db-url",
            "postgresql://root:password@db/readyset",
        ]);
        assert_eq!(opts.database_type().unwrap(), DatabaseType::PostgreSQL);

        let opts = Options::parse_from(vec![
            "readyset",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--upstream-db-url",
            "postgresql://root:password@db/readyset",
            "--database-type",
            "postgresql",
        ]);
        assert_eq!(opts.database_type().unwrap(), DatabaseType::PostgreSQL);

        let opts = Options::parse_from(vec![
            "readyset",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--upstream-db-url",
            "postgresql://root:password@db/readyset",
            "--database-type",
            "mysql",
        ]);
        opts.database_type().unwrap_err();
    }

    #[test]
    fn upstream_and_cdc_urls() {
        let opts = Options::parse_from(vec![
            "readyset",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
        ]);
        assert_eq!(
            opts.server_worker_options
                .replicator_config
                .upstream_db_url
                .as_ref()
                .unwrap()
                .parse::<DatabaseURL>()
                .unwrap(),
            opts.server_worker_options
                .replicator_config
                .get_cdc_db_url()
                .unwrap()
        );

        let opts = Options::parse_from(vec![
            "readyset",
            "--upstream-db-url",
            "mysql://app_user:app_password@mysql:3306/my_app",
            "--cdc-db-url",
            "mysql://replication:rpl_pwd@mysql:3306/my_app",
        ]);

        let upstream = opts
            .server_worker_options
            .replicator_config
            .upstream_db_url
            .as_ref()
            .unwrap();
        let cdc = opts
            .server_worker_options
            .replicator_config
            .get_cdc_db_url()
            .unwrap();

        let r_upstream: RedactedString = "mysql://app_user:app_password@mysql:3306/my_app"
            .parse()
            .unwrap();
        let r_cdc: DatabaseURL = "mysql://replication:rpl_pwd@mysql:3306/my_app"
            .parse()
            .unwrap();
        assert_eq!(*upstream, r_upstream);
        assert_eq!(cdc, r_cdc);

        let opts = Options::parse_from(vec![
            "readyset",
            "--upstream-db-url",
            "mysql://app_user:app_password@mysql:3306/my_app",
            "--cdc-db-url",
            "postgresql://replication:rpl_pwd@mysql:3306/my_app",
        ]);
        opts.database_type().unwrap_err();
    }

    #[test]
    fn infer_deployment_mode() {
        // test --deployment-mode flag
        let opts = Options::parse_from(vec![
            "readyset",
            "--deployment-mode",
            "adapter",
            "--upstream-db-url",
            "postgresql://root:password@db/readyset",
        ]);
        assert_eq!(DeploymentMode::Adapter, opts.deployment_mode);

        // test default
        let opts = Options::parse_from(vec![
            "readyset",
            "--upstream-db-url",
            "postgresql://root:password@db/readyset",
        ]);
        assert_eq!(DeploymentMode::Standalone, opts.deployment_mode);
    }

    #[test]
    fn allowed_users() {
        // test allowed-users with comma and colon in password
        let opts = Options::parse_from(vec![
            "readyset",
            "--allowed-users",
            "user1:pass1,u:\'pwd,\',u2:\'pwd,:,\'",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
        ]);
        let user_list = opts.build_allowed_users().unwrap();
        assert_eq!(user_list.len(), 4);
        assert_eq!(user_list["user1"], "pass1");
        assert_eq!(user_list["u"], "pwd,");
        assert_eq!(user_list["u2"], "pwd,:,");
        assert_eq!(user_list["root"], "password");

        // duplicate user
        let opts = Options::parse_from(vec![
            "readyset",
            "--allowed-users",
            "user1:pass1,user1:pass2",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
        ]);
        opts.build_allowed_users().unwrap_err();

        // duplicate user between allowed-users and upstream-db-url
        let opts = Options::parse_from(vec![
            "readyset",
            "--allowed-users",
            "user1:pass1,user2:pass2",
            "--upstream-db-url",
            "mysql://user1:pass1@mysql:3306/readyset",
        ]);
        opts.build_allowed_users().unwrap_err();
    }
}
