//! The query status cache provides a thread-safe window into an adapter's
//! knowledge about queries, currently the migration status of a query in
//! ReadySet.
use std::collections::HashSet;
use std::hash::Hash;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use clap::ValueEnum;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use lru::LruCache;
use metrics::gauge;
use parking_lot::RwLock;
use readyset_client::metrics::recorded;
use readyset_client::query::*;
use readyset_client::ViewCreateRequest;
use readyset_data::DfValue;
use tracing::{error, warn};

pub const DEFAULT_QUERY_STATUS_CAPACITY: usize = 100_000;

/// A metadata cache for all queries that have been processed by this
/// adapter. Thread-safe.
#[derive(Debug)]
pub struct QueryStatusCache {
    /// A thread-safe hash map that holds the query status of each query that
    /// has been sent to this adapter, keyed by the query's [`QueryId`].
    ///
    /// This map is used on the hot path to determine whether to route queries to upstream or to
    /// readyset.
    id_to_status: DashMap<QueryId, QueryStatus, ahash::RandomState>,

    /// A handle to a more detailed, persistent cache of Query information, which holds the full
    /// query strings. This structure is not used on the hot path, but rather for other auxiliary
    /// commands that seek more information about the queries we have processed so far.
    persistent_handle: PersistentStatusCacheHandle,

    /// Holds the current style of migration, whether async or explicit, which may change the
    /// behavior of some internal methods.
    style: MigrationStyle,

    /// Whether to store a list of pending inlined migrations. Inlined migrations are those with
    /// literal values inlined into certain placeholder positions in the query.
    ///
    /// Currently unused.
    placeholder_inlining: bool,
}

#[derive(Debug)]
/// A handle to persistent metadata for all queries that have been processed by this adapter.
pub struct PersistentStatusCacheHandle {
    /// An [`LRUCache`] that holds the full [`Query`] as well as its associated
    /// [`QueryStatus`] for a fixed number of queries.
    statuses: RwLock<LruCache<QueryId, (Query, QueryStatus)>>,

    /// List of pending inlined migrations. Contains the query to be inlined, and the sets of
    /// parameters to use for inlining.
    pending_inlined_migrations: DashMap<ViewCreateRequest, HashSet<Vec<DfValue>>>,
}

pub struct ReportableMetrics {
    pub id_to_status_size: u64,
    pub statuses_size: u64,
    pub pending_inlined_migrations_size: u64,
}

impl Default for PersistentStatusCacheHandle {
    fn default() -> Self {
        Self {
            statuses: RwLock::new(LruCache::new(
                DEFAULT_QUERY_STATUS_CAPACITY
                    .try_into()
                    .expect("num persisted queries is not zero"),
            )),
            pending_inlined_migrations: Default::default(),
        }
    }
}

impl PersistentStatusCacheHandle {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            statuses: RwLock::new(LruCache::new(
                capacity.try_into().expect("capacity is not zero"),
            )),
            pending_inlined_migrations: Default::default(),
        }
    }

    fn insert_with_status(&self, q: Query, id: QueryId, status: QueryStatus) {
        // Deadlock avoidance: If `with_mut_status` is passed a Fn that tries to write the RwLock,
        // it will result in a deadlock.
        match self.statuses.try_write_for(Duration::from_millis(10)) {
            Some(mut status_guard) => {
                status_guard.put(id, (q, status));
                gauge!(recorded::QUERY_STATUS_CACHE_PERSISTENT_CACHE_SIZE)
                    .set(status_guard.len() as f64);
            }
            None => {
                warn!(query_id=%id, "Avoiding deadlock when trying to insert")
            }
        }
    }

    fn allow_list(&self) -> Vec<(QueryId, Arc<ViewCreateRequest>, QueryStatus)> {
        let statuses = self.statuses.read();
        statuses
            .iter()
            .filter_map(|(query_id, (query, status))| match query {
                Query::Parsed(view) => {
                    if status.is_successful() {
                        Some((*query_id, view.clone(), status.clone()))
                    } else {
                        None
                    }
                }
                Query::ParseFailed(..) => None,
            })
            .collect::<Vec<_>>()
    }

    fn deny_list(&self, style: MigrationStyle) -> Vec<DeniedQuery> {
        let statuses = self.statuses.read();
        match style {
            MigrationStyle::Async | MigrationStyle::InRequestPath => statuses
                .iter()
                .filter_map(|(query_id, (query, status))| {
                    if status.is_unsupported() {
                        Some(DeniedQuery {
                            id: *query_id,
                            query: query.clone(),
                            status: status.clone(),
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            MigrationStyle::Explicit => statuses
                .iter()
                .filter_map(|(query_id, (query, status))| {
                    if status.is_denied() {
                        Some(DeniedQuery {
                            id: *query_id,
                            query: query.clone(),
                            status: status.clone(),
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
        }
    }
}

/// Keys into the queries stored in `QueryStatusCache`
///
/// This trait exists to allow us to overload the notion of "query" to include both successfully
/// parsed queries and queries that have failed to parse.
// The methods in this trait use closures because the reference types returned by DashMap include
// the key type, so methods that *return* lifetime-bound references would not be able to be generic
pub trait QueryStatusKey: Into<Query> + Hash + Clone {
    fn with_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: FnOnce(Option<&QueryStatus>) -> R;

    fn with_mut_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: Fn(Option<&mut QueryStatus>) -> R;

    fn query_id(&self) -> QueryId;
}

impl QueryStatusKey for Query {
    fn with_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: FnOnce(Option<&QueryStatus>) -> R,
    {
        match self {
            Query::Parsed(k) => k.with_status(cache, f),
            Query::ParseFailed(k, _) => k.with_status(cache, f),
        }
    }

    fn with_mut_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: Fn(Option<&mut QueryStatus>) -> R,
    {
        match self {
            Query::Parsed(k) => k.with_mut_status(cache, f),
            Query::ParseFailed(k, _) => k.with_mut_status(cache, f),
        }
    }

    fn query_id(&self) -> QueryId {
        self.into()
    }
}

impl QueryStatusKey for ViewCreateRequest {
    fn with_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: FnOnce(Option<&QueryStatus>) -> R,
    {
        let id = QueryId::from(self);
        // Since this isn't mutating anything, we only need to access the in-memory map.
        f(cache.id_to_status.get(&id).as_deref())
    }

    fn with_mut_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: Fn(Option<&mut QueryStatus>) -> R,
    {
        let id = QueryId::from(self);
        // Since this is potentially mutating, we need to apply F to both the in-memory and the
        // persistent version of the status.
        f(cache.id_to_status.get_mut(&id).as_deref_mut());
        let mut statuses = cache.persistent_handle.statuses.write();
        let transformed_status = statuses.get_mut(&id).map(|(_, status)| status);
        f(transformed_status)
    }

    fn query_id(&self) -> QueryId {
        self.into()
    }
}

impl QueryStatusKey for String {
    fn with_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: FnOnce(Option<&QueryStatus>) -> R,
    {
        let id = QueryId::from_unparsed_select(self);
        // Since this isn't mutating anything, we only need to access the in-memory map.
        f(cache.id_to_status.get(&id).as_deref())
    }

    fn with_mut_status<F, R>(&self, cache: &QueryStatusCache, f: F) -> R
    where
        F: Fn(Option<&mut QueryStatus>) -> R,
    {
        let id = QueryId::from_unparsed_select(self);
        // Since this is potentially mutating, we need to apply F to both the in-memory and the
        // persistent version of the status.
        f(cache.id_to_status.get_mut(&id).as_deref_mut());
        let mut statuses = cache.persistent_handle.statuses.write();
        let transformed_status = statuses.get_mut(&id).map(|(_, status)| status);
        f(transformed_status)
    }

    fn query_id(&self) -> QueryId {
        QueryId::from_unparsed_select(self)
    }
}

impl Default for QueryStatusCache {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryStatusCache {
    /// Constructs a new QueryStatusCache with the migration style set to InRequestPath and a
    /// default capacity of [`DEFAULT_QUERY_STATUS_CAPACITY`]
    pub fn new() -> QueryStatusCache {
        QueryStatusCache {
            id_to_status: Default::default(),
            persistent_handle: Default::default(),
            style: MigrationStyle::InRequestPath,
            placeholder_inlining: false,
        }
    }

    /// Constructs a new QueryStatusCache with the migration style set to InRequestPath and
    /// provided capacity that must be non-zero.
    pub fn with_capacity(capacity: usize) -> QueryStatusCache {
        QueryStatusCache {
            id_to_status: Default::default(),
            persistent_handle: PersistentStatusCacheHandle::with_capacity(capacity),
            style: MigrationStyle::InRequestPath,
            placeholder_inlining: false,
        }
    }

    /// Sets [`Self::style`]
    pub fn style(mut self, style: MigrationStyle) -> Self {
        self.style = style;
        self
    }

    /// Sets [`Self::placeholder_inlining`]
    pub fn set_placeholder_inlining(mut self, placeholder_inlining: bool) -> Self {
        self.placeholder_inlining = placeholder_inlining;
        self
    }

    /// Insert a query into the status cache with an initial status determined by the type of query
    /// that is being inserted. Parsed queries have initial status MigrationState::Pending, while
    /// queries that failed to parse have status MigrationState::Unsupported. Inserts into the
    /// statuses and ids hash maps.
    /// Only queries that are valid SQL should be inserted.
    /// Returns the QueryId and the MigrationState of the inserted Query
    /// self.statuses.insert() should not be called directly
    pub fn insert<Q>(&self, q: Q) -> (QueryId, MigrationState)
    where
        Q: Into<Query>,
    {
        let q = q.into();
        let status = QueryStatus::default_for_query(&q);
        let migration_state = status.migration_state.clone();
        let id = self.insert_with_status(q, status);
        (id, migration_state)
    }

    /// Inserts a query into the status cache with the provided QueryStatus
    /// Only queries that are valid SQL should be inserted.
    fn insert_with_status<Q>(&self, q: Q, status: QueryStatus) -> QueryId
    where
        Q: Into<Query>,
    {
        let q: Query = q.into();
        let status = match q {
            Query::Parsed { .. } => status,
            Query::ParseFailed(_, ref reason) => {
                let mut status = status;
                if !matches!(status.migration_state, MigrationState::Unsupported(_)) {
                    error!("Cannot set migration state to anything other than Unsupported for a Query::ParseFailed");
                    status.migration_state = MigrationState::Unsupported(reason.clone());
                }
                status
            }
        };
        let id = QueryId::from(&q);
        self.id_to_status.insert(id, status.clone());
        self.persistent_handle.insert_with_status(q, id, status);
        gauge!(recorded::QUERY_STATUS_CACHE_SIZE).set(self.id_to_status.len() as f64);
        id
    }

    /// This function returns the id and query migration state of a query.
    ///
    /// Side Effects: If this is the first time we have seen this query, it also adds it to our
    /// mapping of queries.
    pub fn query_migration_state<Q>(&self, q: &Q) -> (QueryId, MigrationState)
    where
        Q: QueryStatusKey,
    {
        let id: QueryId = q.query_id();
        let query_state = self.id_to_status.get(&id);

        match query_state {
            Some(s) => (id, s.value().migration_state.clone()),
            None => self.insert(q.clone()),
        }
    }

    /// This function returns the id and query migration state of a query, if it exists. Unlike
    /// [`QueryStatusCache.query_migration_state`], it does not add the query to our mapping of
    /// queries if it is not present.
    pub fn try_query_migration_state<Q>(&self, q: &Q) -> (QueryId, Option<MigrationState>)
    where
        Q: QueryStatusKey,
    {
        let id = q.query_id();
        let query_state = self.id_to_status.get(&id);

        (id, query_state.map(|s| s.value().migration_state.clone()))
    }

    /// This function returns the query status of a query. If the query does not exist
    /// within the query status cache, an entry is created and the query is set to
    /// PendingMigration.
    pub fn query_status<Q>(&self, q: &Q) -> QueryStatus
    where
        Q: QueryStatusKey,
    {
        match q.with_status(self, |s| s.cloned()) {
            Some(s) => s,
            None => QueryStatus::with_migration_state(self.insert(q.clone()).1),
        }
    }

    /// Updates the transition time in the execution info for the given query.
    pub fn update_transition_time<Q>(&self, q: &Q, transition: &std::time::Instant)
    where
        Q: QueryStatusKey,
    {
        q.with_mut_status(self, |s| {
            if let Some(s) = s {
                if let Some(ref mut info) = s.execution_info {
                    info.last_transition_time = *transition;
                }
            }
        })
    }

    /// Resets the internal transition time to now. This should be used with extreme caution.
    pub fn reset_transition_time(&self, q: &Query) {
        q.with_mut_status(self, |s| {
            if let Some(s) = s {
                if let Some(ref mut info) = s.execution_info {
                    info.last_transition_time = Instant::now()
                }
            }
        })
    }

    /// Update ExecutionInfo to indicate that a recent execute failed due to a networking problem.
    pub fn execute_network_failure(&self, q: &Query) {
        q.with_mut_status(self, |s| {
            if let Some(s) = s {
                match s.execution_info {
                    Some(ref mut info) => info.execute_network_failure(),
                    None => {
                        s.execution_info = Some(ExecutionInfo {
                            state: ExecutionState::NetworkFailure,
                            last_transition_time: Instant::now(),
                        });
                    }
                }
            }
        })
    }

    /// Update ExecutionInfo to indicate that a recent execute succeeded.
    pub fn execute_succeeded(&self, q: &Query) {
        q.with_mut_status(self, |s| {
            if let Some(s) = s {
                match s.execution_info {
                    Some(ref mut info) => info.execute_succeeded(),
                    None => {
                        s.execution_info = Some(ExecutionInfo {
                            state: ExecutionState::Successful,
                            last_transition_time: Instant::now(),
                        });
                    }
                }
            }
        })
    }

    /// Update ExecutionInfo to indicate that a recent execute failed.
    pub fn execute_failed(&self, q: &Query) {
        q.with_mut_status(self, |s| {
            if let Some(s) = s {
                match s.execution_info {
                    Some(ref mut info) => info.execute_failed(),
                    None => {
                        s.execution_info = Some(ExecutionInfo {
                            state: ExecutionState::Failed,
                            last_transition_time: Instant::now(),
                        });
                    }
                }
            }
        })
    }

    /// If the current ExecutionState is ExecutionState::NetworkFailure, then this method will
    /// return true if that state has persisted for longer than the supplied duration, otherwise,
    /// it will return false.
    pub fn execute_network_failure_exceeded(&self, q: &Query, duration: Duration) -> bool {
        q.with_mut_status(self, |s| {
            if let Some(s) = s {
                if let Some(ref info) = s.execution_info {
                    return info.execute_network_failure_exceeded(duration);
                }
            }

            false
        })
    }

    /// The server does not have a view for this query, so set the query to pending.
    pub fn view_not_found_for_query<Q>(&self, q: &Q)
    where
        Q: QueryStatusKey,
    {
        let should_insert = q.with_mut_status(self, |s| {
            match s {
                Some(s) => {
                    // We do not support transitions from the `Unsupported` state, as we assume
                    // any `Unsupported` query will remain `Unsupported` for the duration of
                    // this process.
                    //
                    // `Inlined` queries may only be changed from `Inlined` to `Unsupported`.
                    if !matches!(
                        s.migration_state,
                        MigrationState::Unsupported(_) | MigrationState::Inlined(_)
                    ) {
                        s.migration_state = MigrationState::Pending
                    }
                    false
                }
                // If the query was not in the cache, make a new entry
                None => true,
            }
        });

        if should_insert {
            self.insert_with_status(
                q.clone(),
                QueryStatus {
                    migration_state: MigrationState::Pending,
                    execution_info: None,
                    always: false,
                },
            );
        }
    }

    /// Updates a query's migration state to `m` unless the query's migration state was
    /// `MigrationState::Unsupported` or `MigrationState::Inlined`. An unsupported query cannot
    /// currently become supported once again. An Inlined query can only transition to the
    /// Unsupported state.
    pub fn update_query_migration_state<Q>(&self, q: &Q, m: MigrationState)
    where
        Q: QueryStatusKey,
    {
        let should_insert = q.with_mut_status(self, |s| {
            match s {
                Some(s) => {
                    match s.migration_state {
                        // We do not support transitions from the `Unsupported` state, as we assume
                        // any `Unsupported` query will remain `Unsupported` for the duration of
                        // this process.
                        MigrationState::Unsupported(_) => {}
                        // A query with an Inlined state can only transition to Unsupported.
                        MigrationState::Inlined(_) => {
                            if matches!(m, MigrationState::Unsupported(_)) {
                                s.migration_state = m.clone()
                            }
                        }
                        // All other state transitions are allowed.
                        _ => s.migration_state = m.clone(),
                    }
                    false
                }
                None => true,
            }
        });
        if should_insert {
            self.insert_with_status(
                q.clone(),
                QueryStatus {
                    migration_state: m,
                    execution_info: None,
                    always: false,
                },
            );
        }
    }

    /// Yields to the given function `f` a mutable reference to the migration state of the query
    /// `q`. The primary purpose of this method is allow for atomic reads and writes of the
    /// migration state of a query.
    pub fn with_mut_migration_state<Q, F>(&self, q: &Q, f: F) -> bool
    where
        Q: QueryStatusKey,
        F: Fn(&mut MigrationState),
    {
        q.with_mut_status(self, |maybe_query_status| {
            if let Some(query_status) = maybe_query_status {
                f(&mut query_status.migration_state);
                true
            } else {
                false
            }
        })
    }

    /// This function is called if we attempted to create an inlined migration but received an
    /// unsupported error. Updates the query status and removes pending inlined migrations.
    pub fn unsupported_inlined_migration(&self, q: &ViewCreateRequest) {
        let should_insert = q.with_mut_status(self, |s| match s {
            Some(s) => {
                s.migration_state =
                    MigrationState::Unsupported("Inlined migration not supported".into());
                false
            }
            None => true,
        });
        if should_insert {
            self.insert_with_status(
                q.clone(),
                QueryStatus {
                    migration_state: MigrationState::Unsupported(
                        "Inlined migration not supported".to_string(),
                    ),
                    execution_info: None,
                    always: false,
                },
            );
        }
        self.persistent_handle.pending_inlined_migrations.remove(q);
    }

    /// Updates the query's always flag, indicating whether the query should be served from
    /// ReadySet regardless of autocommit state.
    /// Will not apply the always flag to unsupported queries, or try to insert a query if it has
    /// not already been registered.
    pub fn always_attempt_readyset<Q>(&self, q: &Q, always: bool)
    where
        Q: QueryStatusKey,
    {
        q.with_mut_status(self, |s| match s {
            Some(s) if !matches!(s.migration_state, MigrationState::Unsupported(_)) => {
                s.always = always;
            }
            _ => {}
        })
    }

    /// Updates a queries status to `status` unless the queries migration state was
    /// `MigrationState::Unsupported`. An unsupported query cannot currently become supported once
    /// again.
    pub fn update_query_status<Q>(&self, q: &Q, status: QueryStatus)
    where
        Q: QueryStatusKey,
    {
        let should_insert = q.with_mut_status(self, |s| match s {
            Some(s) if !matches!(s.migration_state, MigrationState::Unsupported(_)) => {
                s.migration_state.clone_from(&status.migration_state);
                s.execution_info.clone_from(&status.execution_info);
                false
            }
            Some(s) => {
                s.execution_info.clone_from(&status.execution_info);
                false
            }
            None => true,
        });
        if should_insert {
            self.insert_with_status(q.clone(), status);
        }
    }

    /// Clear all queries currently marked as successful from the cache.
    pub fn clear(&self) {
        self.id_to_status
            .iter_mut()
            .filter(|v| v.is_successful())
            .for_each(|mut v| {
                v.migration_state = MigrationState::Pending;
                v.always = false;
            });
        let mut statuses = self.persistent_handle.statuses.write();
        statuses
            .iter_mut()
            .filter(|(_query_id, (_query, status))| status.is_successful())
            .for_each(|(_query_id, (_query, ref mut status))| {
                status.migration_state = MigrationState::Pending;
                status.always = false;
            });
    }

    /// Clear all queries not marked as successful from the cache.
    pub fn clear_proxied_queries(&self) {
        self.id_to_status
            .retain(|_query_id, status| status.is_successful());

        let mut statuses = self.persistent_handle.statuses.write();
        let keys_to_remove: Vec<QueryId> = statuses
            .iter()
            .filter(|(_, (_, status))| !status.is_successful())
            .map(|(query_id, _)| *query_id)
            .collect();

        for key in keys_to_remove {
            statuses.pop(&key);
        }
    }

    /// This method is called when a query is executed with the given params, but no inlined cache
    /// exists for the params. Adding the query to `Self::pending_inlined_migrations` indicates that
    /// it should be migrated by the MigrationHandler.
    pub fn inlined_cache_miss(&self, query: &ViewCreateRequest, params: Vec<DfValue>) {
        if self.placeholder_inlining {
            self.persistent_handle
                .pending_inlined_migrations
                .entry(query.clone())
                .or_default()
                .insert(params);
        }
    }

    /// Indicates that a migration has been completed for some set of literals for a query in
    /// `Self::pending_inlined_migrations`
    pub fn created_inlined_query(
        &self,
        query: &ViewCreateRequest,
        migrated_literals: Vec<&Vec<DfValue>>,
    ) {
        if let Entry::Occupied(mut entry) = self
            .persistent_handle
            .pending_inlined_migrations
            .entry(query.clone())
        {
            let pending_literals = entry.get_mut();
            for literals in migrated_literals {
                pending_literals.remove(literals);
            }
            // If we removed all the pending literals from the entry, we should remove the entry.
            if pending_literals.is_empty() {
                entry.remove();
            }
        }

        // Then update the inlined state epoch for the query
        query.with_mut_status(self, |s| {
            if let Some(QueryStatus {
                migration_state: MigrationState::Inlined(ref mut state),
                ..
            }) = s
            {
                state.epoch += 1;
            }
        })
    }

    /// Returns a list of queries that are pending an inlined migration, and a set of all literals
    /// to be used for inlining.
    pub fn pending_inlined_migration(&self) -> Vec<QueryInliningInstructions> {
        self.persistent_handle
            .pending_inlined_migrations
            .iter()
            .filter_map(|q| {
                // Get the placeholders that require inlining
                let placeholders =
                    q.key()
                        .with_status(self, |s| match s.map(|s| &s.migration_state) {
                            Some(MigrationState::Inlined(InlinedState {
                                inlined_placeholders,
                                ..
                            })) => Some(inlined_placeholders.clone()),
                            _ => None,
                        });

                // Generate QueryInliningInstructions
                placeholders.map(|p| {
                    QueryInliningInstructions::new(
                        q.key().clone(),
                        p,
                        q.value().iter().cloned().collect::<Vec<_>>(),
                    )
                })
            })
            .collect::<Vec<_>>()
    }

    /// Returns a list of queries that currently need the be processed to determine
    /// if they should be allowed (are supported by ReadySet).
    ///
    /// Does not include any queries that require inlining.
    pub fn pending_migration(&self) -> QueryList {
        let statuses = self.persistent_handle.statuses.read();
        statuses
            .iter()
            .filter_map(|(_query_id, (query, status))| {
                if status.is_pending() {
                    Some((query.clone(), status.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<(Query, QueryStatus)>>()
            .into()
    }

    /// Returns a list of queries whose migration states match `states`.
    pub fn queries_with_statuses(&self, states: &[MigrationState]) -> QueryList {
        let statuses = self.persistent_handle.statuses.read();
        statuses
            .iter()
            .filter_map(|(_query_id, (query, status))| {
                if states.contains(&status.migration_state) {
                    Some((query.clone(), status.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<(Query, QueryStatus)>>()
            .into()
    }

    /// Returns a list of queries that have a state of [`QueryState::Successful`].
    pub fn allow_list(&self) -> Vec<(QueryId, Arc<ViewCreateRequest>, QueryStatus)> {
        self.persistent_handle.allow_list()
    }

    /// Returns a list of queries that are in the deny list.
    pub fn deny_list(&self) -> Vec<DeniedQuery> {
        self.persistent_handle.deny_list(self.style)
    }

    /// Returns a query given a query hash
    pub fn query(&self, id: &str) -> Option<Query> {
        let id = id.parse::<QueryId>().ok()?;
        let statuses = self.persistent_handle.statuses.read();
        statuses.peek(&id).map(|(query, _status)| query.clone())
    }

    pub fn reportable_metrics(&self) -> ReportableMetrics {
        ReportableMetrics {
            id_to_status_size: self.id_to_status.len() as u64,
            statuses_size: self.persistent_handle.statuses.read().len() as u64,
            pending_inlined_migrations_size: self.persistent_handle.pending_inlined_migrations.len()
                as u64,
        }
    }
}

/// MigrationStyle is used to communicate which style of managing migrations we have configured.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MigrationStyle {
    /// Async migrations are enabled in the adapter by setting the --query-caching argument to
    /// async
    Async,
    /// Explicit migrations are enabled in the adapter by setting the --query-caching argument to
    /// explicit
    Explicit,
    /// InRequestPath is the style of managing migrations when neither async nor explicit
    /// migrations have been enabled.
    InRequestPath,
}

impl FromStr for MigrationStyle {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "inrequestpath" => Ok(MigrationStyle::InRequestPath),
            "async" => Ok(MigrationStyle::Async),
            "explicit" => Ok(MigrationStyle::Explicit),
            other => Err(anyhow!("Invalid option specified: {}", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use readyset_client::ViewCreateRequest;
    use readyset_sql::ast::{SelectStatement, SqlQuery};
    use readyset_util::hash::hash;
    use vec1::Vec1;

    use super::*;

    fn select_statement(s: &str) -> anyhow::Result<SelectStatement> {
        match readyset_sql_parsing::parse_query(readyset_sql::Dialect::MySQL, s) {
            Ok(SqlQuery::Select(s)) => Ok(s),
            Ok(q) => Err(anyhow::anyhow!("Not a SELECT statement: {q:?}")),
            Err(e) => Err(anyhow::anyhow!("Parsing error: {e}")),
        }
    }

    #[test]
    fn query_hashes_eq_inner_hashes() {
        // This ensures that calling query_status on a &SelectStatement or &String will find the
        // corresponding Query in the DashMap
        let select = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let string = "SELECT * FROM t1".to_string();
        let q_select = Query::Parsed(Arc::new(select.clone()));
        let q_string = Query::ParseFailed(string.clone().into(), "Failed".to_string());
        assert_eq!(
            hash(&QueryId::from(&select)),
            hash(&QueryId::from(&q_select))
        );
        assert_eq!(
            hash(&QueryId::from_unparsed_select(string.as_str())),
            hash(&QueryId::from(&q_string))
        );
    }

    #[test]
    fn select_is_found_after_insert() {
        let cache = QueryStatusCache::new();
        let q1 = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let status = QueryStatus::default_for_query(&q1.clone().into());
        let id = QueryId::from(&q1);

        cache.insert(q1.clone());

        let mut statuses = cache.persistent_handle.statuses.write();
        assert!(statuses
            .iter()
            .map(|(_, (q, _))| q.clone())
            .any(|q| q == q1.clone().into()));

        assert!(statuses.put(id, (q1.into(), status.clone())).is_some());

        assert_eq!(statuses.get(&id).unwrap().1, status);
    }

    #[test]
    fn string_is_found_after_insert() {
        let cache = QueryStatusCache::new();
        let q1 = "SELECT * FROM t1".to_string();
        let status = QueryStatus::default_for_query(&Query::ParseFailed(
            Arc::new(q1.clone()),
            "Failed".to_string(),
        ));
        let id = QueryId::from_unparsed_select(&q1);

        cache.insert(q1.clone());

        let mut statuses = cache.persistent_handle.statuses.write();
        assert!(statuses
            .iter()
            .map(|(_, (q, _))| q.clone())
            .any(|q| q == q1.clone().into()));

        assert!(statuses.put(id, (q1.into(), status.clone())).is_some());

        assert_eq!(statuses.get(&id).unwrap().1, status);
    }

    #[test]
    fn query_is_referenced_by_query_id() {
        let cache = QueryStatusCache::new();
        let q1 = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let q2 = ViewCreateRequest::new(select_statement("SELECT * FROM t2").unwrap(), vec![]);

        cache.update_query_migration_state(&q1, MigrationState::Pending);
        cache.update_query_migration_state(&q2, MigrationState::Successful);

        let h1 = QueryId::from(&q1);
        let h2 = QueryId::from(&q2);

        let r1 = cache.query(&h1.to_string()).unwrap();
        let r2 = cache.query(&h2.to_string()).unwrap();

        assert_eq!(r1, q1.into());
        assert_eq!(r2, q2.into());
    }

    #[test]
    fn query_is_allowed() {
        let cache = QueryStatusCache::new();
        let query = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);

        assert_eq!(
            cache.query_migration_state(&query).0,
            QueryId::from(&Into::<Query>::into(query.clone()))
        );

        // If we haven't explicitly updated it, we default to pending
        assert_eq!(
            cache.query_migration_state(&query).1,
            MigrationState::Pending
        );

        // Explicitly updating it also lets it be returned from pending_migration(), allow_list(),
        // and deny_list()
        cache.update_query_migration_state(&query, MigrationState::Pending);
        assert_eq!(cache.pending_migration().len(), 1);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 0);

        cache.update_query_migration_state(&query, MigrationState::Successful);
        assert_eq!(cache.pending_migration().len(), 0);
        assert_eq!(cache.allow_list().len(), 1);
        assert_eq!(cache.deny_list().len(), 0);
    }

    #[test]
    fn query_is_denied() {
        let cache = QueryStatusCache::new();
        let query = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);

        assert_eq!(
            cache.query_migration_state(&query).1,
            MigrationState::Pending
        );
        cache.update_query_migration_state(&query, MigrationState::Pending);
        assert_eq!(cache.pending_migration().len(), 1);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 0);

        cache.update_query_migration_state(&query, MigrationState::Unsupported("".into()));
        assert_eq!(cache.pending_migration().len(), 0);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 1);
    }

    #[test]
    fn query_is_inferred_denied_explicit() {
        let cache = QueryStatusCache::new().style(MigrationStyle::Explicit);
        let query = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);

        assert_eq!(
            cache.query_migration_state(&query).1,
            MigrationState::Pending
        );
        cache.update_query_migration_state(&query, MigrationState::Pending);
        assert_eq!(cache.pending_migration().len(), 1);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 1);

        cache.update_query_migration_state(&query, MigrationState::Unsupported("".into()));
        assert_eq!(cache.pending_migration().len(), 0);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 1);
    }

    #[test]
    fn clear() {
        let cache = QueryStatusCache::new().style(MigrationStyle::Explicit);

        cache.update_query_migration_state(
            &ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]),
            MigrationState::Successful,
        );
        cache.update_query_migration_state(
            &ViewCreateRequest::new(
                select_statement("SELECT * FROM t1 WHERE id = ?").unwrap(),
                vec![],
            ),
            MigrationState::Successful,
        );
        assert_eq!(cache.allow_list().len(), 2);

        cache.clear();
        assert_eq!(cache.allow_list().len(), 0);
    }

    #[test]
    fn view_not_found_for_query() {
        let cache = QueryStatusCache::new().style(MigrationStyle::Explicit);
        let q1 = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let q2 = ViewCreateRequest::new(select_statement("SELECT * FROM t2").unwrap(), vec![]);

        cache.update_query_migration_state(&q1, MigrationState::Successful);
        cache.update_query_migration_state(
            &q2,
            MigrationState::Inlined(InlinedState {
                inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
                epoch: 0,
            }),
        );
        // q1: supported -> pending
        cache.view_not_found_for_query(&q1);
        assert_eq!(cache.pending_migration().len(), 1);
        // q1: pending -> unsupported
        cache.update_query_migration_state(&q1, MigrationState::Unsupported("".to_string()));
        assert_eq!(cache.pending_migration().len(), 0);
        // q2: inlined -> inlined
        cache.view_not_found_for_query(&q2);
        assert_eq!(cache.pending_migration().len(), 0);
    }

    #[test]
    fn transition_from_unsupported() {
        let cache = QueryStatusCache::new().style(MigrationStyle::Explicit);
        let q = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);

        cache.update_query_migration_state(&q, MigrationState::Pending);
        cache.update_query_migration_state(&q, MigrationState::Unsupported("Failed".into()));
        assert_eq!(
            cache.query_migration_state(&q).1,
            MigrationState::Unsupported("Failed".into())
        );
        cache.update_query_migration_state(&q, MigrationState::Pending);
        assert_eq!(
            cache.query_migration_state(&q).1,
            MigrationState::Unsupported("Failed".into())
        );
        cache.update_query_migration_state(
            &q,
            MigrationState::Inlined(InlinedState {
                inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
                epoch: 0,
            }),
        );
        assert_eq!(
            cache.query_migration_state(&q).1,
            MigrationState::Unsupported("Failed".into())
        );
        cache.update_query_migration_state(&q, MigrationState::Successful);
        assert_eq!(
            cache.query_migration_state(&q).1,
            MigrationState::Unsupported("Failed".into())
        );
    }

    #[test]
    fn transition_from_inlined() {
        let cache = QueryStatusCache::new()
            .style(MigrationStyle::Explicit)
            .set_placeholder_inlining(true);
        let q = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let inlined_state = MigrationState::Inlined(InlinedState {
            inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
            epoch: 0,
        });

        cache.update_query_migration_state(&q, inlined_state.clone());
        assert_eq!(cache.query_migration_state(&q).1, inlined_state);
        cache.update_query_migration_state(&q, MigrationState::Pending);
        assert_eq!(cache.query_migration_state(&q).1, inlined_state);
        cache.update_query_migration_state(&q, MigrationState::Successful);
        assert_eq!(cache.query_migration_state(&q).1, inlined_state);
        cache.update_query_migration_state(&q, MigrationState::Unsupported("Should fail".into()));
        assert_eq!(
            cache.query_migration_state(&q).1,
            MigrationState::Unsupported("Should fail".into())
        );
    }

    #[test]
    fn inlined_cache_miss() {
        let cache = QueryStatusCache::new()
            .style(MigrationStyle::Explicit)
            .set_placeholder_inlining(true);
        let q = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let inlined_state = MigrationState::Inlined(InlinedState {
            inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
            epoch: 0,
        });
        cache.update_query_migration_state(&q, inlined_state);

        cache.inlined_cache_miss(&q, vec![DfValue::None]);
        cache.inlined_cache_miss(&q, vec![DfValue::None]);
        cache.inlined_cache_miss(&q, vec![DfValue::Max]);

        assert_eq!(
            cache
                .persistent_handle
                .pending_inlined_migrations
                .get(&q)
                .unwrap()
                .value()
                .len(),
            2
        );
    }

    #[test]
    fn unsupported_inlined_migration() {
        let cache = QueryStatusCache::new()
            .style(MigrationStyle::Explicit)
            .set_placeholder_inlining(true);
        let q = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let inlined_state = MigrationState::Inlined(InlinedState {
            inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
            epoch: 0,
        });
        cache.update_query_migration_state(&q, inlined_state);

        cache.inlined_cache_miss(&q, vec![DfValue::None]);

        cache.unsupported_inlined_migration(&q);

        assert!(cache
            .persistent_handle
            .pending_inlined_migrations
            .is_empty());
        assert_eq!(
            cache.query_migration_state(&q).1,
            MigrationState::Unsupported("Inlined migration not supported".to_string())
        );
    }

    #[test]
    fn created_inlined_query() {
        let cache = QueryStatusCache::new()
            .style(MigrationStyle::Explicit)
            .set_placeholder_inlining(true);
        let q = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let inlined_state = MigrationState::Inlined(InlinedState {
            inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
            epoch: 0,
        });
        cache.update_query_migration_state(&q, inlined_state.clone());

        cache.inlined_cache_miss(&q, vec![DfValue::None]);
        cache.inlined_cache_miss(&q, vec![DfValue::Max]);
        cache.inlined_cache_miss(&q, vec![DfValue::Int(1)]);

        assert_eq!(cache.query_migration_state(&q).1, inlined_state);
        cache.created_inlined_query(&q, vec![&vec![DfValue::Int(1)], &vec![DfValue::None]]);
        let inlined_state = MigrationState::Inlined(InlinedState {
            inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
            epoch: 1,
        });
        cache.update_query_migration_state(&q, inlined_state.clone());
        let state = cache.query_status(&q).migration_state;
        assert_eq!(state, inlined_state);
        assert_eq!(
            cache
                .persistent_handle
                .pending_inlined_migrations
                .get(&q)
                .unwrap()
                .value()
                .len(),
            1
        );
        assert!(cache
            .persistent_handle
            .pending_inlined_migrations
            .get(&q)
            .unwrap()
            .value()
            .contains(&vec![DfValue::Max]))
    }

    #[test]
    fn pending_inlined_migration() {
        let cache = QueryStatusCache::new()
            .style(MigrationStyle::Explicit)
            .set_placeholder_inlining(true);
        let q = ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]);
        let inlined_state = MigrationState::Inlined(InlinedState {
            inlined_placeholders: Vec1::try_from(vec![1]).unwrap(),
            epoch: 0,
        });
        cache.update_query_migration_state(&q, inlined_state);

        cache.inlined_cache_miss(&q, vec![DfValue::None]);
        cache.inlined_cache_miss(&q, vec![DfValue::Max]);

        assert!(cache.pending_migration().is_empty());
        let pending = cache.pending_inlined_migration();
        assert_eq!(pending[0].query(), &q);
        assert_eq!(pending[0].placeholders(), &[1]);
        assert_eq!(pending[0].literals().len(), 2);
        assert!(pending[0].literals().contains(&vec![DfValue::Max]));
        assert!(pending[0].literals().contains(&vec![DfValue::None]));
    }

    #[test]
    fn avoid_insert_deadlock() {
        readyset_tracing::init_test_logging();
        let cache = QueryStatusCache::new().style(MigrationStyle::Explicit);
        let q = Query::ParseFailed(Arc::new("foobar".to_string()), "Should Fail".to_string());

        q.with_mut_status(&cache, |_| {
            // Simulate it being removed by lru cache then inserted
            let query =
                Query::ParseFailed(Arc::new("foobar".to_string()), "Should Fail".to_string());
            let query_id = QueryId::from_unparsed_select("foobar");
            let query_status = QueryStatus::default_for_query(&query);
            cache
                .persistent_handle
                .insert_with_status(query, query_id, query_status);
        });
    }

    #[test]
    fn clear_proxied_queries() {
        let cache = QueryStatusCache::new().style(MigrationStyle::Explicit);

        cache.update_query_migration_state(
            &ViewCreateRequest::new(select_statement("SELECT * FROM t1").unwrap(), vec![]),
            MigrationState::Successful,
        );
        cache.update_query_migration_state(
            &ViewCreateRequest::new(
                select_statement("SELECT * FROM t1 WHERE id = ?").unwrap(),
                vec![],
            ),
            MigrationState::Successful,
        );
        cache.update_query_migration_state(
            &ViewCreateRequest::new(
                select_statement("SELECT * FROM t1 WHERE id > ?").unwrap(),
                vec![],
            ),
            MigrationState::Pending,
        );
        cache.update_query_migration_state(
            &ViewCreateRequest::new(select_statement("SELECT y FROM t2").unwrap(), vec![]),
            MigrationState::Unsupported("Should fail".to_string()),
        );
        assert_eq!(cache.allow_list().len(), 2);
        assert_eq!(cache.deny_list().len(), 2);

        cache.clear_proxied_queries();
        assert_eq!(cache.allow_list().len(), 2);
        assert_eq!(cache.deny_list().len(), 0);
    }
}
