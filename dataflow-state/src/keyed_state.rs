use std::iter;

use common::{Index, IndexType};
use indexmap::IndexMap;
use partial_map::PartialMap;
use readyset_data::{Bound, DfValue};
use readyset_util::ranges::RangeBounds;
use tuple::TupleElements;
use vec1::Vec1;

use crate::mk_key::MakeKey;
use crate::{Misses, PointKey, RangeKey, Row, Rows};
use readyset_util::maybe_shrink;

/// A map containing a single index into the state of a node.
///
/// KeyedStates are associative (key-value) maps from lists of [`DfValue`]s  to [lists of
/// reference-counted pointers to rows](Rows), and can be backed by either a
/// [`BTreeMap`](std::collections::BTreeMap) or an [`IndexMap`], according to an
/// [`IndexType`](readyset_client::IndexType).
///
/// Any operations on a KeyedState that are unsupported by the index type, such as inserting or
/// looking up ranges in a HashMap, will panic.
#[allow(clippy::type_complexity)]
pub(super) enum KeyedState {
    AllRows(Rows),
    SingleBTree(PartialMap<DfValue, Rows>),
    DoubleBTree(PartialMap<(DfValue, DfValue), Rows>),
    TriBTree(PartialMap<(DfValue, DfValue, DfValue), Rows>),
    QuadBTree(PartialMap<(DfValue, DfValue, DfValue, DfValue), Rows>),
    QuinBTree(PartialMap<(DfValue, DfValue, DfValue, DfValue, DfValue), Rows>),
    SexBTree(PartialMap<(DfValue, DfValue, DfValue, DfValue, DfValue, DfValue), Rows>),
    // the `usize` parameter is the length of the Vec.
    MultiBTree(PartialMap<Vec<DfValue>, Rows>, usize),

    SingleHash(IndexMap<DfValue, Rows, ahash::RandomState>),
    DoubleHash(IndexMap<(DfValue, DfValue), Rows, ahash::RandomState>),
    TriHash(IndexMap<(DfValue, DfValue, DfValue), Rows, ahash::RandomState>),
    QuadHash(IndexMap<(DfValue, DfValue, DfValue, DfValue), Rows, ahash::RandomState>),
    QuinHash(IndexMap<(DfValue, DfValue, DfValue, DfValue, DfValue), Rows, ahash::RandomState>),
    SexHash(
        IndexMap<(DfValue, DfValue, DfValue, DfValue, DfValue, DfValue), Rows, ahash::RandomState>,
    ),
    // ♪ multi-hash ♪ https://www.youtube.com/watch?v=bEtDVy55shI
    // (`usize` parameter as in `MultiBTree`)
    MultiHash(IndexMap<Vec<DfValue>, Rows>, usize),
}

impl KeyedState {
    /// Returns the length of this keyed state's key
    pub(super) fn key_len(&self) -> usize {
        match self {
            KeyedState::AllRows(_) => 0,
            KeyedState::SingleBTree(_) | KeyedState::SingleHash(_) => 1,
            KeyedState::DoubleBTree(_) | KeyedState::DoubleHash(_) => 2,
            KeyedState::TriBTree(_) | KeyedState::TriHash(_) => 3,
            KeyedState::QuadBTree(_) | KeyedState::QuadHash(_) => 4,
            KeyedState::QuinBTree(_) | KeyedState::QuinHash(_) => 5,
            KeyedState::SexBTree(_) | KeyedState::SexHash(_) => 6,
            KeyedState::MultiHash(_, l) | KeyedState::MultiBTree(_, l) => *l,
        }
    }

    /// Returns the number of keys stored in this keyed state
    pub(super) fn key_count(&self) -> usize {
        match self {
            KeyedState::AllRows(_) => 0,
            KeyedState::SingleBTree(partial_map) => partial_map.num_keys(),
            KeyedState::DoubleBTree(partial_map) => partial_map.num_keys(),
            KeyedState::TriBTree(partial_map) => partial_map.num_keys(),
            KeyedState::QuadBTree(partial_map) => partial_map.num_keys(),
            KeyedState::QuinBTree(partial_map) => partial_map.num_keys(),
            KeyedState::SexBTree(partial_map) => partial_map.num_keys(),
            KeyedState::MultiBTree(partial_map, _) => partial_map.num_keys(),
            KeyedState::SingleHash(index_map) => index_map.len(),
            KeyedState::DoubleHash(index_map) => index_map.len(),
            KeyedState::TriHash(index_map) => index_map.len(),
            KeyedState::QuadHash(index_map) => index_map.len(),
            KeyedState::QuinHash(index_map) => index_map.len(),
            KeyedState::SexHash(index_map) => index_map.len(),
            KeyedState::MultiHash(index_map, _) => index_map.len(),
        }
    }

    /// Look up all the rows corresponding to the given `key` and return them, or return None if no
    /// rows exist for the given key
    ///
    /// # Panics
    ///
    /// Panics if the length of `key` is different than the length of this `KeyedState`
    pub(super) fn lookup<'a>(&'a self, key: &PointKey) -> Option<&'a Rows> {
        match (self, key) {
            (KeyedState::AllRows(r), PointKey::Empty) => Some(r),
            (KeyedState::SingleBTree(m), PointKey::Single(k)) => m.get(k),
            (KeyedState::DoubleBTree(m), PointKey::Double(k)) => m.get(k),
            (KeyedState::TriBTree(m), PointKey::Tri(k)) => m.get(k),
            (KeyedState::QuadBTree(m), PointKey::Quad(k)) => m.get(k),
            (KeyedState::QuinBTree(m), PointKey::Quin(k)) => m.get(k),
            (KeyedState::SexBTree(m), PointKey::Sex(k)) => m.get(k),
            (&KeyedState::MultiBTree(ref m, len), PointKey::Multi(k)) if k.len() == len => {
                m.get(k.as_ref())
            }
            (KeyedState::SingleHash(m), PointKey::Single(k)) => m.get(k),
            (KeyedState::DoubleHash(m), PointKey::Double(k)) => m.get(k),
            (KeyedState::TriHash(m), PointKey::Tri(k)) => m.get(k),
            (KeyedState::QuadHash(m), PointKey::Quad(k)) => m.get(k),
            (KeyedState::QuinHash(m), PointKey::Quin(k)) => m.get(k),
            (KeyedState::SexHash(m), PointKey::Sex(k)) => m.get(k),
            (&KeyedState::MultiHash(ref m, len), PointKey::Multi(k)) if k.len() == len => {
                m.get(k.as_ref())
            }
            _ => {
                #[allow(clippy::panic)] // documented invariant
                {
                    panic!(
                        "Invalid key type for KeyedState, got key of length {}, but expected key \
                         of length {}",
                        key.len(),
                        self.key_len()
                    )
                }
            }
        }
    }

    /// Insert the given `row` into this `KeyedState`, using the column indices in `key_cols` to
    /// derive the key, and return whether or not the row was actually inserted
    ///
    /// If `partial` is `true`, and the key is not present, the row will not be inserted and
    /// `insert` will return `false`.
    ///
    /// # Invariants
    ///
    /// * The length of `key_cols` must be equal to the length of the key of this KeyedState
    /// * All column indices in `key_cols` must be in-bounds for `row`
    pub(super) fn insert(&mut self, key_cols: &[usize], row: Row, partial: bool) -> bool {
        macro_rules! single_insert {
            ($map: ident, $key_cols: expr, $row: expr, $partial: expr) => {{
                // treat this specially to avoid the extra Vec
                debug_assert_eq!($key_cols.len(), 1);
                // i *wish* we could use the entry API here, but it would mean an extra clone
                // in the common case of an entry already existing for the given key...
                let key = &row[key_cols[0]];
                if let Some(ref mut rs) = $map.get_mut(key) {
                    rs.insert(row);
                    return true;
                } else if $partial {
                    // trying to insert a record into partial materialization hole!
                    return false;
                }

                $map.insert(key.clone(), iter::once(row).collect());
            }};
        }

        macro_rules! multi_insert {
            ($map: ident, $key_cols: expr, $row:expr, $partial: expr, $entry:path) => {{
                let key = MakeKey::from_row($key_cols, &*$row);
                use $entry as Entry;
                match $map.entry(key) {
                    Entry::Occupied(rs) => {
                        rs.into_mut().insert($row);
                    }
                    Entry::Vacant(..) if $partial => return false,
                    rs @ Entry::Vacant(..) => {
                        rs.or_default().insert($row);
                    }
                }
            }};
        }

        match self {
            KeyedState::AllRows(rows) => {
                debug_assert!(key_cols.is_empty());
                rows.insert(row);
            }
            KeyedState::SingleBTree(map) => single_insert!(map, key_cols, row, partial),
            KeyedState::DoubleBTree(map) => {
                multi_insert!(map, key_cols, row, partial, partial_map::Entry)
            }
            KeyedState::TriBTree(map) => {
                multi_insert!(map, key_cols, row, partial, partial_map::Entry)
            }
            KeyedState::QuadBTree(map) => {
                multi_insert!(map, key_cols, row, partial, partial_map::Entry)
            }
            KeyedState::QuinBTree(map) => {
                multi_insert!(map, key_cols, row, partial, partial_map::Entry)
            }
            KeyedState::SexBTree(map) => {
                multi_insert!(map, key_cols, row, partial, partial_map::Entry)
            }
            KeyedState::MultiBTree(map, len) => {
                debug_assert_eq!(key_cols.len(), *len);
                multi_insert!(map, key_cols, row, partial, partial_map::Entry)
            }
            KeyedState::SingleHash(map) => single_insert!(map, key_cols, row, partial),
            KeyedState::DoubleHash(map) => {
                multi_insert!(map, key_cols, row, partial, indexmap::map::Entry)
            }
            KeyedState::TriHash(map) => {
                multi_insert!(map, key_cols, row, partial, indexmap::map::Entry)
            }
            KeyedState::QuadHash(map) => {
                multi_insert!(map, key_cols, row, partial, indexmap::map::Entry)
            }
            KeyedState::QuinHash(map) => {
                multi_insert!(map, key_cols, row, partial, indexmap::map::Entry)
            }
            KeyedState::SexHash(map) => {
                multi_insert!(map, key_cols, row, partial, indexmap::map::Entry)
            }
            KeyedState::MultiHash(map, len) => {
                debug_assert_eq!(key_cols.len(), *len);
                multi_insert!(map, key_cols, row, partial, indexmap::map::Entry)
            }
        }

        true
    }

    /// Remove one instance of the given `row` from this `KeyedState`, using the column indices in
    /// `key_cols` to derive the key, and return the row itself.
    ///
    /// If given, `hit` will be set to `true` if the key exists in `self` (but not necessarily if
    /// the row was found!)
    ///
    /// # Invariants
    ///
    /// * The length of `key_cols` must be equal to the length of the key of this KeyedState
    /// * All column indices in `key_cols` must be in-bounds for `row`
    pub(super) fn remove(
        &mut self,
        key_cols: &[usize],
        row: &[DfValue],
        hit: Option<&mut bool>,
    ) -> Option<Row> {
        let do_remove = |rs: &mut Rows| -> Option<Row> {
            if let Some(hit) = hit {
                *hit = true;
            }
            let rm = if rs.len() == 1 {
                // it *should* be impossible to get a negative for a record that we don't have,
                // so let's avoid hashing + eqing if we don't need to
                let left = rs.drain().next().unwrap();
                debug_assert_eq!(left.1, 1);
                debug_assert_eq!(&left.0[..], row);
                Some(left.0)
            } else {
                match rs.try_take(row) {
                    Ok(row) => Some(row),
                    Err(None) => None,
                    Err(Some((row, _))) => {
                        // there are still copies of the row left in rs
                        // SAFETY: row is never moved to another thread
                        Some(unsafe { row.clone() })
                    }
                }
            };
            rm
        };

        macro_rules! single_remove {
            ($map: ident, $key_cols: expr, $row: expr) => {{
                if let Some(rs) = $map.get_mut(&row[$key_cols[0]]) {
                    return do_remove(rs);
                }
            }};
        }

        macro_rules! multi_remove {
            ($map: ident, $key_cols: expr, $row: expr) => {
                multi_remove!($map, $key_cols, $row, _)
            };
            ($map: ident, $key_cols: expr, $row: expr, $hint: ty) => {{
                let key = <$hint as MakeKey<_>>::from_row(&$key_cols, $row);
                if let Some(rs) = $map.get_mut(&key) {
                    return do_remove(rs);
                }
            }};
        }

        match self {
            KeyedState::AllRows(rows) => return do_remove(rows),
            KeyedState::SingleBTree(map) => single_remove!(map, key_cols, row),
            KeyedState::DoubleBTree(map) => multi_remove!(map, key_cols, row),
            KeyedState::TriBTree(map) => multi_remove!(map, key_cols, row),
            KeyedState::QuadBTree(map) => multi_remove!(map, key_cols, row),
            KeyedState::QuinBTree(map) => multi_remove!(map, key_cols, row),
            KeyedState::SexBTree(map) => multi_remove!(map, key_cols, row),
            KeyedState::MultiBTree(map, len) => {
                debug_assert_eq!(key_cols.len(), *len);
                multi_remove!(map, key_cols, row, Vec<_>);
            }
            KeyedState::SingleHash(map) => single_remove!(map, key_cols, row),
            KeyedState::DoubleHash(map) => multi_remove!(map, key_cols, row, (_, _)),
            KeyedState::TriHash(map) => multi_remove!(map, key_cols, row, (_, _, _)),
            KeyedState::QuadHash(map) => multi_remove!(map, key_cols, row, (_, _, _, _)),
            KeyedState::QuinHash(map) => multi_remove!(map, key_cols, row, (_, _, _, _, _)),
            KeyedState::SexHash(map) => multi_remove!(map, key_cols, row, (_, _, _, _, _, _)),
            KeyedState::MultiHash(map, len) => {
                debug_assert_eq!(key_cols.len(), *len);
                multi_remove!(map, key_cols, row, Vec<_>);
            }
        }

        None
    }

    /// Mark the given range of keys as filled
    ///
    /// # Panics
    ///
    /// Panics if this `KeyedState` is backed by a HashMap index
    pub(super) fn insert_range(&mut self, range: (Bound<Vec1<DfValue>>, Bound<Vec1<DfValue>>)) {
        match self {
            KeyedState::SingleBTree(ref mut map) => map.insert_range((
                range.0.map(|k| k.split_off_first().0),
                range.1.map(|k| k.split_off_first().0),
            )),
            KeyedState::DoubleBTree(ref mut map) => {
                map.insert_range(<(DfValue, _) as MakeKey<_>>::from_range(&range))
            }
            KeyedState::TriBTree(ref mut map) => {
                map.insert_range(<(DfValue, _, _) as MakeKey<_>>::from_range(&range))
            }
            KeyedState::QuadBTree(ref mut map) => {
                map.insert_range(<(DfValue, _, _, _) as MakeKey<_>>::from_range(&range))
            }
            KeyedState::QuinBTree(ref mut map) => {
                map.insert_range(<(DfValue, _, _, _, _) as MakeKey<_>>::from_range(&range))
            }
            KeyedState::SexBTree(ref mut map) => {
                map.insert_range(<(DfValue, _, _, _, _, _) as MakeKey<_>>::from_range(&range))
            }
            // This is unwieldy, but allowing callers to insert the wrong length of Vec into us
            // would be very bad!
            KeyedState::MultiBTree(ref mut map, len) => {
                assert!(range.0.len() == *len && range.1.len() == *len);
                map.insert_range((range.0.map(Vec1::into_vec), range.1.map(Vec1::into_vec)))
            }
            KeyedState::AllRows(_) => panic!("insert_range called on an AllRows KeyedState"),
            KeyedState::SingleHash(_)
            | KeyedState::DoubleHash(_)
            | KeyedState::TriHash(_)
            | KeyedState::QuadHash(_)
            | KeyedState::QuinHash(_)
            | KeyedState::SexHash(_)
            | KeyedState::MultiHash(_, _) => panic!("insert_range called on a HashMap KeyedState"),
        };
    }

    /// Mark every key in the state as filled. Note that `KeyedState::insert_range()` cannot be
    /// used for this purpose, since a [`Bound`] cannot be used to represent an unbounded upper or
    /// lower bound.
    ///
    /// # Panics
    ///
    /// Panics if this `KeyedState` is backed by a HashMap index
    pub(super) fn insert_full_range(&mut self) {
        match self {
            KeyedState::SingleBTree(ref mut map) => {
                map.insert_full_range();
            }
            KeyedState::DoubleBTree(ref mut map) => {
                map.insert_full_range();
            }
            KeyedState::TriBTree(ref mut map) => {
                map.insert_full_range();
            }
            KeyedState::QuadBTree(ref mut map) => {
                map.insert_full_range();
            }
            KeyedState::QuinBTree(ref mut map) => {
                map.insert_full_range();
            }
            KeyedState::SexBTree(ref mut map) => {
                map.insert_full_range();
            }
            KeyedState::MultiBTree(ref mut map, _) => {
                map.insert_full_range();
            }
            KeyedState::AllRows(_) => panic!("insert_full_range called on an AllRows KeyedState"),
            KeyedState::SingleHash(_)
            | KeyedState::DoubleHash(_)
            | KeyedState::TriHash(_)
            | KeyedState::QuadHash(_)
            | KeyedState::QuinHash(_)
            | KeyedState::SexHash(_)
            | KeyedState::MultiHash(_, _) => {
                panic!("insert_full_range called on a HashMap KeyedState")
            }
        };
    }

    /// Look up all the keys in the given range `key`, and return either iterator over all the rows
    /// or a set of [`Misses`] indicating that some keys are not present
    ///
    /// # Panics
    ///
    /// * Panics if the length of `key` is different than the length of this `KeyedState`
    /// * Panics if this `KeyedState` is backed by a HashMap index
    pub(super) fn lookup_range<'a>(
        &'a self,
        key: &RangeKey,
    ) -> Result<Box<dyn Iterator<Item = &'a Row> + 'a>, Misses> {
        fn to_misses<K: TupleElements<Element = DfValue>>(
            misses: Vec<(Bound<K>, Bound<K>)>,
        ) -> Misses {
            misses
                .into_iter()
                .map(|(lower, upper)| {
                    (
                        lower.map(|k| k.into_elements().collect()),
                        upper.map(|k| k.into_elements().collect()),
                    )
                })
                .collect()
        }

        fn flatten_rows<'a, K: 'a, I: Iterator<Item = (&'a K, &'a Rows)> + 'a>(
            r: I,
        ) -> Box<dyn Iterator<Item = &'a Row> + 'a> {
            Box::new(r.flat_map(|(_, rows)| rows))
        }

        macro_rules! range {
            ($m: expr, $range: ident) => {
                $m.range($range).map(flatten_rows).map_err(to_misses)
            };
        }

        match (self, key) {
            (KeyedState::SingleBTree(m), RangeKey::Single(range)) => {
                m.range(range).map(flatten_rows).map_err(|misses| {
                    misses
                        .into_iter()
                        .map(|(lower, upper)| (lower.map(|k| vec![k]), upper.map(|k| vec![k])))
                        .collect()
                })
            }
            (KeyedState::DoubleBTree(m), RangeKey::Double(range)) => range!(m, range),
            (KeyedState::TriBTree(m), RangeKey::Tri(range)) => range!(m, range),
            (KeyedState::QuadBTree(m), RangeKey::Quad(range)) => range!(m, range),
            (KeyedState::QuinBTree(m), RangeKey::Quin(range)) => range!(m, range),
            (KeyedState::SexBTree(m), RangeKey::Sex(range)) => range!(m, range),
            (KeyedState::MultiBTree(m, _), RangeKey::Multi(range)) => m
                .range::<_, [DfValue]>(&(
                    range.0.as_ref().map(|b| b.as_ref()),
                    range.1.as_ref().map(|b| b.as_ref()),
                ))
                .map(flatten_rows),
            (
                KeyedState::SingleHash(_)
                | KeyedState::DoubleHash(_)
                | KeyedState::TriHash(_)
                | KeyedState::QuadHash(_)
                | KeyedState::QuinHash(_)
                | KeyedState::SexHash(_)
                | KeyedState::MultiHash(..),
                _,
            ) => panic!("lookup_range called on a HashMap KeyedState"),
            _ => panic!(
                "Invalid key type for KeyedState, got key of length {:?}",
                key.len()
            ),
        }
    }

    /// Remove all rows for a randomly chosen key seeded by `seed`, returning the rows along
    /// with the key. Returns `None` if map is empty.
    // For correct eviction we care about the number of elements present in the map, not intervals,
    // therefore have to compare len to 0
    pub(super) fn evict_with_seed(&mut self, seed: usize) -> Option<(Rows, Vec<DfValue>)> {
        macro_rules! evict_hash {
            ($m: expr, $seed: expr) => {{
                if $m.is_empty() {
                    return None;
                }
                let index = $seed % $m.len();
                let rows = $m
                    .swap_remove_index(index)
                    .map(|(k, rs)| (rs, k.into_elements().collect()));
                maybe_shrink!($m);
                rows
            }};
        }

        let (rs, key) = match self {
            KeyedState::SingleHash(ref mut m) if !m.is_empty() => {
                let index = seed % m.len();
                let rows = m.swap_remove_index(index).map(|(k, rs)| (rs, vec![k]));
                maybe_shrink!(m);
                rows
            }
            KeyedState::DoubleHash(ref mut m) => evict_hash!(m, seed),
            KeyedState::TriHash(ref mut m) => evict_hash!(m, seed),
            KeyedState::QuadHash(ref mut m) => evict_hash!(m, seed),
            KeyedState::QuinHash(ref mut m) => evict_hash!(m, seed),
            KeyedState::SexHash(ref mut m) => evict_hash!(m, seed),
            KeyedState::MultiHash(ref mut m, _) if !m.is_empty() => {
                let index = seed % m.len();
                let rows = m.swap_remove_index(index).map(|(k, rs)| (rs, k));
                maybe_shrink!(m);
                rows
            }

            // TODO(aspen): This way of evicting (which also happens in reader_map) is pretty icky -
            // we have to iterate the sequence of keys, *and* we have to clone out the
            // keys themselves! we should find a better way to do that.
            // https://app.clubhouse.io/readysettech/story/154
            KeyedState::SingleBTree(ref mut m) if m.num_keys() > 0 => {
                let index = seed % m.num_keys();
                let key = m.keys().nth(index).unwrap().clone();
                m.remove_entry(&key).map(|(k, rs)| (rs, vec![k]))
            }
            KeyedState::DoubleBTree(ref mut m) if m.num_keys() > 0 => {
                let index = seed % m.num_keys();
                let key = m.keys().nth(index).unwrap().clone();
                m.remove_entry(&key).map(|(k, rs)| (rs, vec![k.0, k.1]))
            }
            KeyedState::TriBTree(ref mut m) if m.num_keys() > 0 => {
                let index = seed % m.num_keys();
                let key = m.keys().nth(index).unwrap().clone();
                m.remove_entry(&key)
                    .map(|(k, rs)| (rs, vec![k.0, k.1, k.2]))
            }
            KeyedState::QuadBTree(ref mut m) if m.num_keys() > 0 => {
                let index = seed % m.num_keys();
                let key = m.keys().nth(index).unwrap().clone();
                m.remove_entry(&key)
                    .map(|(k, rs)| (rs, vec![k.0, k.1, k.2, k.3]))
            }
            KeyedState::QuinBTree(ref mut m) if m.num_keys() > 0 => {
                let index = seed % m.num_keys();
                let key = m.keys().nth(index).unwrap().clone();
                m.remove_entry(&key)
                    .map(|(k, rs)| (rs, vec![k.0, k.1, k.2, k.3, k.4]))
            }
            KeyedState::SexBTree(ref mut m) if m.num_keys() > 0 => {
                let index = seed % m.num_keys();
                let key = m.keys().nth(index).unwrap().clone();
                m.remove_entry(&key)
                    .map(|(k, rs)| (rs, vec![k.0, k.1, k.2, k.3, k.4, k.5]))
            }
            KeyedState::MultiBTree(ref mut m, _) if m.num_keys() > 0 => {
                let index = seed % m.num_keys();
                let key = m.keys().nth(index).unwrap().clone();
                m.remove_entry(&key).map(|(k, rs)| (rs, k))
            }
            _ => {
                // map must be empty, so no point in trying to evict from it.
                return None;
            }
        }?;
        Some((rs, key))
    }

    /// Remove all rows for the given key, returning the evicted rows.
    ///
    /// # Panics
    ///
    /// * Panics if `self` is [`KeyedState::AllRows`], which cannot be partial
    pub(super) fn evict(&mut self, key: &[DfValue]) -> Option<Rows> {
        macro_rules! evict_hash {
            ($m: expr, $ty:ty, $k: expr) => {{
                let rows = $m.swap_remove::<$ty>(&MakeKey::from_key($k));
                maybe_shrink!($m);
                rows
            }};
        }

        match self {
            KeyedState::AllRows(_) => panic!("Empty-column index cannot be partial"),
            KeyedState::SingleBTree(ref mut m) => m.remove(&(key[0])),
            KeyedState::DoubleBTree(ref mut m) => m.remove(&MakeKey::from_key(key)),
            KeyedState::TriBTree(ref mut m) => m.remove(&MakeKey::from_key(key)),
            KeyedState::QuadBTree(ref mut m) => m.remove(&MakeKey::from_key(key)),
            KeyedState::QuinBTree(ref mut m) => m.remove(&MakeKey::from_key(key)),
            KeyedState::SexBTree(ref mut m) => m.remove(&MakeKey::from_key(key)),
            // FIXME(eta): this clones unnecessarily, given we could make PartialMap do the Borrow
            // thing. That requires making the unbounded-interval-tree crate do that as
            // well, though, and that's painful. (also everything else in here clones --
            // I do wonder what the perf impacts of that are)
            KeyedState::MultiBTree(ref mut m, _) => m.remove(&key.to_owned()),

            KeyedState::SingleHash(ref mut m) => {
                let rows = m.swap_remove(&(key[0]));
                maybe_shrink!(m);
                rows
            }
            KeyedState::DoubleHash(ref mut m) => evict_hash!(m, (DfValue, _), key),
            KeyedState::TriHash(ref mut m) => evict_hash!(m, (DfValue, _, _), key),
            KeyedState::QuadHash(ref mut m) => evict_hash!(m, (DfValue, _, _, _), key),
            KeyedState::QuinHash(ref mut m) => evict_hash!(m, (DfValue, _, _, _, _), key),
            KeyedState::SexHash(ref mut m) => evict_hash!(m, (DfValue, _, _, _, _, _), key),
            KeyedState::MultiHash(ref mut m, _) => {
                let rows = m.swap_remove(&key.to_owned());
                maybe_shrink!(m);
                rows
            }
        }
    }

    /// Evict all rows in the given range of keys from this KeyedState, and return the removed rows
    ///
    /// # Panics
    ///
    /// Panics if this `KeyedState` is backed by a HashMap index
    pub(super) fn evict_range<R>(&mut self, range: &R) -> Rows
    where
        R: RangeBounds<Vec1<DfValue>>,
    {
        macro_rules! do_evict_range {
            ($m: expr, $range: expr, $hint: ty) => {
                $m.remove_range(<$hint as MakeKey<DfValue>>::from_range($range))
                    .flat_map(|(_, rows)| rows.into_iter().map(|(r, _)| r))
                    .collect()
            };
        }

        match self {
            KeyedState::SingleBTree(m) => do_evict_range!(m, range, DfValue),
            KeyedState::DoubleBTree(m) => do_evict_range!(m, range, (DfValue, _)),
            KeyedState::TriBTree(m) => do_evict_range!(m, range, (DfValue, _, _)),
            KeyedState::QuadBTree(m) => do_evict_range!(m, range, (DfValue, _, _, _)),
            KeyedState::QuinBTree(m) => do_evict_range!(m, range, (DfValue, _, _, _, _)),
            KeyedState::SexBTree(m) => do_evict_range!(m, range, (DfValue, _, _, _, _, _)),
            KeyedState::MultiBTree(m, _) => m
                .remove_range::<[DfValue], _>((
                    range.start_bound().map(Vec1::as_slice),
                    range.end_bound().map(Vec1::as_slice),
                ))
                .flat_map(|(_, rows)| rows.into_iter().map(|(r, _)| r))
                .collect(),
            _ => {
                #[allow(clippy::panic)] // documented invariant
                {
                    panic!("evict_range called on a HashMap KeyedState")
                }
            }
        }
    }

    pub(super) fn values<'a>(&'a self) -> Box<dyn Iterator<Item = &'a Rows> + 'a> {
        match self {
            KeyedState::AllRows(ref rows) => Box::new(iter::once(rows)),
            KeyedState::SingleBTree(ref map) => Box::new(map.values()),
            KeyedState::DoubleBTree(ref map) => Box::new(map.values()),
            KeyedState::TriBTree(ref map) => Box::new(map.values()),
            KeyedState::QuadBTree(ref map) => Box::new(map.values()),
            KeyedState::QuinBTree(ref map) => Box::new(map.values()),
            KeyedState::SexBTree(ref map) => Box::new(map.values()),
            KeyedState::MultiBTree(ref map, _) => Box::new(map.values()),
            KeyedState::SingleHash(ref map) => Box::new(map.values()),
            KeyedState::DoubleHash(ref map) => Box::new(map.values()),
            KeyedState::TriHash(ref map) => Box::new(map.values()),
            KeyedState::QuadHash(ref map) => Box::new(map.values()),
            KeyedState::QuinHash(ref map) => Box::new(map.values()),
            KeyedState::SexHash(ref map) => Box::new(map.values()),
            KeyedState::MultiHash(ref map, _) => Box::new(map.values()),
        }
    }
}

impl From<&Index> for KeyedState {
    fn from(index: &Index) -> Self {
        use IndexType::*;
        match (index.len(), &index.index_type) {
            (0, _) => KeyedState::AllRows(Default::default()),
            (1, BTreeMap) => KeyedState::SingleBTree(Default::default()),
            (2, BTreeMap) => KeyedState::DoubleBTree(Default::default()),
            (3, BTreeMap) => KeyedState::TriBTree(Default::default()),
            (4, BTreeMap) => KeyedState::QuadBTree(Default::default()),
            (5, BTreeMap) => KeyedState::QuinBTree(Default::default()),
            (6, BTreeMap) => KeyedState::SexBTree(Default::default()),
            (1, HashMap) => KeyedState::SingleHash(Default::default()),
            (2, HashMap) => KeyedState::DoubleHash(Default::default()),
            (3, HashMap) => KeyedState::TriHash(Default::default()),
            (4, HashMap) => KeyedState::QuadHash(Default::default()),
            (5, HashMap) => KeyedState::QuinHash(Default::default()),
            (6, HashMap) => KeyedState::SexHash(Default::default()),
            (x, HashMap) => KeyedState::MultiHash(Default::default(), x),
            (x, BTreeMap) => KeyedState::MultiBTree(Default::default(), x),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evict_with_seed_empty() {
        for index_cols in &[
            vec![0],
            vec![0, 1],
            vec![0, 1, 3],
            vec![0, 1, 3, 4],
            vec![0, 1, 3, 4, 5],
            vec![0, 1, 3, 4, 5, 6],
            vec![0, 1, 3, 4, 5, 6, 7],
        ] {
            for index_type in [IndexType::HashMap, IndexType::BTreeMap] {
                let index = Index::new(index_type, index_cols.clone());
                let mut state = KeyedState::from(&index);
                assert!(state.evict_with_seed(123).is_none());
            }
        }
    }
}
