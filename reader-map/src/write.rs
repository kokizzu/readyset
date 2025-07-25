use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::ops::Deref;
use std::time::Instant;

use left_right::Absorb;
use partial_map::InsertionOrder;
use readyset_client::internal::IndexType;
use readyset_util::ranges::{Bound, RangeBounds};

use crate::eviction::EvictionMeta;
use crate::inner::Inner;
use crate::read::ReadHandle;
use crate::values::Values;

/// A representation of how many keys to evict from the map.
#[derive(Debug)]
pub enum EvictionQuantity {
    /// The ratio of keys to evict on [0, 1]
    Ratio(f64),
    /// Evict a single key.
    ///
    /// NOTE: This should not be used outside of tests, because it wouldn't be efficient to evict
    /// one key at a time.
    SingleKey,
}

/// A handle that may be used to modify the eventually consistent map.
///
/// Note that any changes made to the map will not be made visible to readers until
/// [`publish`](Self::publish) is called.
///
/// When the `WriteHandle` is dropped, the map is immediately (but safely) taken away from all
/// readers, causing all future lookups to return `None`.
///
/// # Examples
/// ```
/// let x = ('x', 42);
///
/// let (mut w, r) = reader_map::new();
///
/// // the map is uninitialized, so all lookups should return Err(NotPublished)
/// assert_eq!(r.get(&x.0).err().unwrap(), reader_map::Error::NotPublished);
///
/// w.publish();
///
/// // after the first publish, it is empty, but ready
/// assert!(r.get(&x.0).unwrap().is_none());
///
/// w.insert(x.0, x);
///
/// // it is empty even after an add (we haven't publish yet)
/// assert!(r.get(&x.0).unwrap().is_none());
///
/// w.publish();
///
/// // but after the swap, the record is there!
/// assert_eq!(r.get(&x.0).unwrap().map(|rs| rs.len()), Some(1));
/// assert_eq!(
///     r.get(&x.0)
///         .unwrap()
///         .map(|rs| rs.iter().any(|v| v.0 == x.0 && v.1 == x.1)),
///     Some(true)
/// );
/// ```
pub struct WriteHandle<K, V, I, M = (), S = RandomState>
where
    K: Ord + Hash + Clone,
    S: BuildHasher + Clone,
    V: Ord + Clone,
    M: 'static + Clone,
    I: InsertionOrder<V>,
{
    handle: left_right::WriteHandle<Inner<K, V, M, S, I>, Operation<K, V, M>>,
    r_handle: ReadHandle<K, V, I, M, S>,
}

impl<K, V, I, M, S> fmt::Debug for WriteHandle<K, V, I, M, S>
where
    K: Ord + Hash + Clone + fmt::Debug,
    S: BuildHasher + Clone + fmt::Debug,
    V: Ord + Clone + fmt::Debug,
    M: 'static + Clone + fmt::Debug,
    I: InsertionOrder<V>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteHandle")
            .field("handle", &self.handle)
            .finish()
    }
}

impl<K, V, I, M, S> WriteHandle<K, V, I, M, S>
where
    K: Ord + Hash + Clone,
    S: BuildHasher + Clone,
    V: Ord + Clone,
    M: 'static + Clone,
    I: InsertionOrder<V>,
{
    pub(crate) fn new(
        handle: left_right::WriteHandle<Inner<K, V, M, S, I>, Operation<K, V, M>>,
    ) -> Self {
        let r_handle = ReadHandle::new(left_right::ReadHandle::clone(&*handle));
        Self { handle, r_handle }
    }

    /// Returns the base size of inner data associated with this write handle.
    pub fn base_value_size(&self) -> usize {
        std::mem::size_of::<Values<V, I>>()
    }

    /// Publish all changes since the last call to `publish` to make them visible to readers.
    ///
    /// This can take some time, especially if readers are executing slow operations, or if there
    /// are many of them.
    pub fn publish(&mut self) -> &mut Self {
        self.handle.publish();
        self
    }

    /// Returns true if there are changes to the map that have not yet been exposed to readers.
    pub fn has_pending(&self) -> bool {
        self.handle.has_pending_operations()
    }

    /// Set the metadata.
    ///
    /// Will only be visible to readers after the next call to [`publish`](Self::publish).
    pub fn set_meta(&mut self, meta: M) {
        self.add_op(Operation::SetMeta(meta));
    }

    fn add_ops<IT>(&mut self, ops: IT) -> &mut Self
    where
        IT: IntoIterator<Item = Operation<K, V, M>>,
    {
        self.handle.extend(ops);
        self
    }

    fn add_op(&mut self, op: Operation<K, V, M>) -> &mut Self {
        self.add_ops(vec![op])
    }

    /// Add the given value to the value-bag of the given key.
    ///
    /// The updated value-bag will only be visible to readers after the next call to
    /// [`publish`](Self::publish).
    pub fn insert(&mut self, k: K, v: V) -> &mut Self {
        self.add_op(Operation::Add {
            key: k,
            value: v,
            eviction_meta: None,
            index: None,
            timestamp: Instant::now(),
        })
    }

    /// Add the list of `records` to the value-set, which are assumed to have a key part of the
    /// `range`. The `range` is then also inserted to the underlying interval tree, to keep
    /// track of which intervals are covered by the evmap.
    ///
    /// The update value-set will only be visible to readers after the next call to
    /// [`publish`](Self::publish).
    pub fn insert_range<R>(&mut self, range: R) -> &mut Self
    where
        R: RangeBounds<K>,
    {
        self.add_op(Operation::AddRange((
            range.start_bound().cloned(),
            range.end_bound().cloned(),
        )))
    }

    /// Inserts the full range of keys into the map.
    pub fn insert_full_range(&mut self) -> &mut Self {
        self.add_op(Operation::AddFullRange)
    }

    /// Clear the value-bag of the given key, without removing it.
    ///
    /// If a value-bag already exists, this will clear it but leave the
    /// allocated memory intact for reuse, or if no associated value-bag exists
    /// an empty value-bag will be created for the given key.
    ///
    /// The new value will only be visible to readers after the next call to
    /// [`publish`](Self::publish).
    pub fn clear(&mut self, k: K) -> &mut Self {
        self.add_op(Operation::Clear(k, None))
    }

    /// Remove the given value from the value-bag of the given key.
    ///
    /// The updated value-bag will only be visible to readers after the next call to
    /// [`publish`](Self::publish).
    pub fn remove_value(&mut self, k: K, v: V) -> &mut Self {
        self.add_op(Operation::RemoveValue {
            key: k,
            value: v,
            index: None,
            timestamp: Instant::now(),
        })
    }

    /// Remove the value-bag for the given key.
    ///
    /// The value-bag will only disappear from readers after the next call to
    /// [`publish`](Self::publish).
    pub fn remove_entry(&mut self, k: K) -> &mut Self {
        self.add_op(Operation::RemoveEntry(k))
    }

    /// Remove all the entries for keys in the given range.
    ///
    /// The entries will only disappear from readers after the next call to
    /// [`publish`](Self::publish).
    pub fn remove_range(&mut self, range: (Bound<K>, Bound<K>)) -> &mut Self {
        self.add_op(Operation::RemoveRange(range))
    }

    /// Purge all value-bags from the map.
    ///
    /// The map will only appear empty to readers after the next call to
    /// [`publish`](Self::publish).
    ///
    /// Note that this will iterate once over all the keys internally.
    pub fn purge(&mut self) -> &mut Self {
        self.add_op(Operation::Purge)
    }

    /// Remove the value-bag for randomly chosen keys given an `EvictionQuantity` to evict.
    ///
    /// This method immediately calls [`publish`](Self::publish) to ensure that the keys and values
    /// it returns match the elements that will be emptied on the next call to
    /// [`publish`](Self::publish). The values will be submitted for eviction, but the result will
    /// only be visible to all readers after a following call to publish is made. The method returns
    /// the amount of memory freed, computed using the provided closure on each (K,V) pair.
    ///
    /// Returns the number of bytes evicted and the keys/ranges evicted.
    pub fn evict_keys<'a, F>(
        &'a mut self,
        request: EvictionQuantity,
        mut mem_cnt: F,
    ) -> (usize, Box<dyn Iterator<Item = (K, Option<K>)>>)
    where
        F: FnMut(&K, &Values<V, I>) -> usize,
        K: 'static,
    {
        self.publish();

        let inner = self
            .r_handle
            .handle
            .raw_handle()
            .expect("WriteHandle has not been dropped");
        // safety: the writer cannot publish until 'a ends, so we know that reading from the read
        // map is safe for the duration of 'a.
        let inner: &'a Inner<K, V, M, S, I> =
            unsafe { std::mem::transmute::<&Inner<K, V, M, S, I>, _>(inner.as_ref()) };

        let keys_to_evict = match request {
            EvictionQuantity::Ratio(ratio) => (inner.data.len() as f64 * ratio) as usize,
            EvictionQuantity::SingleKey => 1,
        }
        .min(inner.data.len());

        let mut mem = 0;
        let mut keys = Vec::new();

        match inner.data.index_type() {
            IndexType::BTreeMap => {
                let mut range_iterator = inner
                    .eviction_strategy
                    .pick_ranges_to_evict(&inner.data, keys_to_evict);

                // If we received an `EvictionQuantity::SingleKey` request, return the evicted key.
                if matches!(request, EvictionQuantity::SingleKey) {
                    // It's possible that we had nothing to evict
                    let Some(mut subrange_iter) = range_iterator.next_range() else {
                        return (0, Box::new(std::iter::empty()));
                    };
                    let (k, v) = subrange_iter.next().expect("Subrange can't be empty");

                    return (mem_cnt(k, v), Box::new(vec![(k.clone(), None)].into_iter()));
                }

                while let Some(subrange_iter) = range_iterator.next_range() {
                    let mut subrange_iter = subrange_iter.inspect(|(k, v)| {
                        mem += mem_cnt(k, v);
                    });

                    let (start, _) = subrange_iter.next().expect("Subrange can't be empty");
                    let end = match subrange_iter.last() {
                        None => start,
                        Some((end, _)) => end,
                    };

                    self.add_op(Operation::RemoveRange((
                        Bound::Included(start.clone()),
                        Bound::Included(end.clone()),
                    )));
                    keys.push((start.clone(), Some(end.clone())));
                }
            }
            IndexType::HashMap => {
                let kvs = inner
                    .eviction_strategy
                    .pick_keys_to_evict(&inner.data, keys_to_evict);
                for (k, v) in kvs {
                    self.add_op(Operation::RemoveEntry(k.clone()));
                    keys.push((k.clone(), None));
                    mem += mem_cnt(k, v);
                    if matches!(request, EvictionQuantity::SingleKey) {
                        return (mem, Box::new(keys.into_iter()));
                    }
                }
            }
        };

        (mem, Box::new(keys.into_iter()))
    }
}

impl<K, V, M, S, I> Absorb<Operation<K, V, M>> for Inner<K, V, M, S, I>
where
    K: Ord + Hash + Clone,
    S: BuildHasher + Clone,
    V: Ord + Clone,
    M: 'static + Clone,
    I: InsertionOrder<V>,
{
    /// Apply ops in such a way that no values are dropped, only forgotten
    fn absorb_first(&mut self, op: &mut Operation<K, V, M>, _other: &Self) {
        match op {
            Operation::Add {
                key,
                value,
                eviction_meta,
                index,
                timestamp,
            } => {
                let metrics = self.metrics.clone();
                let values = self.data_entry(key.clone(), eviction_meta);
                values.insert(value.clone(), index, *timestamp);
                metrics.record_updated(values.metrics());
            }
            Operation::RemoveValue {
                key,
                value,
                index,
                timestamp,
            } => {
                if let Some(e) = self.data.get_mut(key) {
                    e.remove(value, index, *timestamp);

                    // removing a value from a key is just "updating" that key
                    self.metrics.record_updated(e.metrics());
                }
            }
            Operation::AddRange(range) => self.data.add_range(range.clone()),
            Operation::AddFullRange => self.data.add_full_range(),
            Operation::Clear(key, eviction_meta) => {
                self.data_entry(key.clone(), eviction_meta).clear()
                // `clear()` is invoked on replay when filling a hole, will be followed
                // by a call to `add()`. hence don't capture metrics on clear().
            }
            Operation::RemoveEntry(key) => {
                let v = self.data.remove(key);

                // upstream deletes flow through `RemoveValue`, thus these removes are either
                // upstream evictions or a call to "mark a hole" (mostly likely an eviction).
                if let Some(values) = v {
                    self.metrics.record_evicted(values.metrics());
                }
            }
            Operation::Purge => self.data.clear(),
            Operation::RemoveRange(range) => {
                // RemoveRange is only called on evictions (via marking a hole).
                self.data.remove_range(range.clone(), |metrics| {
                    self.metrics.record_evicted(metrics);
                });
            }
            Operation::MarkReady => {
                self.ready = true;
            }
            Operation::SetMeta(m) => {
                self.meta = m.clone();
            }
        }
    }

    /// Apply operations while allowing dropping of values
    fn absorb_second(&mut self, op: Operation<K, V, M>, _other: &Self) {
        match op {
            Operation::Add {
                key,
                value,
                mut eviction_meta,
                mut index,
                timestamp,
            } => {
                let values = self.data_entry(key, &mut eviction_meta);
                values.insert(value, &mut index, timestamp);
            }
            Operation::RemoveValue {
                key,
                value,
                mut index,
                timestamp,
            } => {
                if let Some(e) = self.data.get_mut(&key) {
                    e.remove(&value, &mut index, timestamp);
                }
            }
            Operation::AddRange(range) => self.data.add_range(range),
            Operation::AddFullRange => self.data.add_full_range(),
            Operation::Clear(key, mut eviction_meta) => {
                self.data_entry(key, &mut eviction_meta).clear()
            }
            Operation::RemoveEntry(key) => {
                self.data.remove(&key);
            }
            Operation::RemoveRange(range) => self.data.remove_range(range, |_| {}),
            Operation::Purge => self.data.clear(),
            Operation::MarkReady => {
                self.ready = true;
            }
            Operation::SetMeta(m) => {
                self.meta = m;
            }
        }
    }

    fn sync_with(&mut self, first: &Self) {
        self.data = first.data.clone();
        self.ready = first.ready;
    }
}

// allow using write handle for reads
impl<K, V, I, M, S> Deref for WriteHandle<K, V, I, M, S>
where
    K: Ord + Clone + Hash,
    S: BuildHasher + Clone,
    V: Ord + Clone,
    M: 'static + Clone,
    I: InsertionOrder<V>,
{
    type Target = ReadHandle<K, V, I, M, S>;

    fn deref(&self) -> &Self::Target {
        &self.r_handle
    }
}

/// A pending map operation.
#[non_exhaustive]
pub(super) enum Operation<K, V, M> {
    /// Add this value to the set of entries for this key.
    Add {
        key: K,
        value: V,
        eviction_meta: Option<EvictionMeta>,
        // insertion index for [`absorb_second`] and computed in [`absorb_first`]
        index: Option<usize>,
        // timestamp of when the operation occurred
        timestamp: Instant,
    },
    /// Add an interval to the list of filled intervals
    AddRange((Bound<K>, Bound<K>)),
    /// Add the full range of keys
    AddFullRange,
    /// Remove this value from the set of entries for this key.
    RemoveValue {
        key: K,
        value: V,
        // removal index for [`absorb_second`] and computed in [`absorb_first`]
        index: Option<usize>,
        // timestamp of when the operation occurred
        timestamp: Instant,
    },
    /// Remove the value set for this key.
    RemoveEntry(K),
    /// Remove all entries in the given range
    RemoveRange((Bound<K>, Bound<K>)),
    /// Remove all values in the value set for this key.
    Clear(K, Option<EvictionMeta>),
    /// Remove all values for all keys.
    ///
    /// Note that this will iterate once over all the keys internally.
    Purge,
    /// Mark the map as ready to be consumed for readers.
    MarkReady,
    /// Set the value of the map meta.
    SetMeta(M),
}

impl<K, V, M> fmt::Debug for Operation<K, V, M>
where
    K: fmt::Debug,
    V: fmt::Debug,
    M: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operation::Add {
                key,
                value,
                eviction_meta,
                index: _,
                timestamp: _,
            } => f
                .debug_tuple("Add")
                .field(key)
                .field(value)
                .field(eviction_meta)
                .finish(),
            Operation::AddRange(a) => f.debug_tuple("AddRange").field(a).finish(),
            Operation::AddFullRange => f.debug_tuple("AddFullRange").finish(),
            Operation::RemoveValue {
                key,
                value,
                index: _,
                timestamp: _,
            } => f
                .debug_tuple("RemoveValue")
                .field(key)
                .field(value)
                .finish(),
            Operation::RemoveRange(range) => f.debug_tuple("RemoveRange").field(range).finish(),
            Operation::RemoveEntry(a) => f.debug_tuple("RemoveEntry").field(a).finish(),
            Operation::Clear(a, _) => f.debug_tuple("Clear").field(a).finish(),
            Operation::Purge => f.debug_tuple("Purge").finish(),
            Operation::MarkReady => f.debug_tuple("MarkReady").finish(),
            Operation::SetMeta(a) => f.debug_tuple("SetMeta").field(a).finish(),
        }
    }
}
