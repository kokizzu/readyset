//! This crate provides miscellaneous utilities and extensions to the Rust standard library, for use
//! in all crates in this workspace.
#![deny(missing_docs, rustdoc::missing_crate_level_docs)]
use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

#[cfg(feature = "failure_injection")]
pub mod failpoints;

pub mod arbitrary;
pub mod display;
pub mod fmt;
pub mod futures;
pub mod graphviz;
pub mod hash;
pub mod indexmap;
pub mod intervals;
pub mod iter;
pub mod math;
pub mod progress;
pub mod properties;
pub mod ranges;
pub mod redacted;
pub mod shared_cache;
pub mod shutdown;

mod time_scope;
pub use time_scope::time_scope;

/// The maximum NUMERIC scale we can accurately represent.
// XXX JCD rust_decimal made scale 28 possibly incorrect by rounding down to it:
// https://github.com/paupino/rust-decimal/pull/647
pub const NUMERIC_MAX_SCALE: u8 = 27;

/// Error (returned by [`Indices::indices`] and [`Indices::cloned_indices`]) for an out-of-bounds
/// index access
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct IndexOutOfBounds<Idx>(Idx);

/// Extension trait to add the [`indices`] and [`cloned_indices`] methods to all types that
/// implement [`Index`]
pub trait Indices<'idx, Idx: 'idx> {
    /// The type of values in self, returned as elements of the Vec returned by both [`indices`] and
    /// [`cloned_indices`]
    type Output;

    /// Return a vector of references to all the values in self corresponding to the indices in
    /// `indices`, or, if any of the indices were out of bounds, an error indicating the first such
    /// out-of-bound index
    ///
    /// # Examples
    ///
    /// ```rust
    /// use readyset_util::Indices;
    ///
    /// let v = vec![0, 1, 2, 3, 4];
    /// let res = v.indices(vec![1, 2, 3]).unwrap();
    /// assert_eq!(res, vec![&1, &2, &3]);
    /// ```
    fn indices<'a, I>(&'a self, indices: I) -> Result<Vec<&'a Self::Output>, IndexOutOfBounds<Idx>>
    where
        I: IntoIterator<Item = Idx> + 'idx;

    /// Return a vector of clones of all the values in self corresponding to the indices in
    /// `indices`, or, if any of the indices were out of bounds, an error indicating the first such
    /// out-of-bound index
    ///
    /// # Examples
    ///
    /// ```rust
    /// use readyset_util::Indices;
    ///
    /// let v = vec![0, 1, 2, 3, 4];
    /// let res = v.cloned_indices(vec![1, 2, 3]).unwrap();
    /// assert_eq!(res, vec![1, 2, 3]);
    /// ```
    fn cloned_indices<I>(&self, indices: I) -> Result<Vec<Self::Output>, IndexOutOfBounds<Idx>>
    where
        I: IntoIterator<Item = Idx> + 'idx,
        Self::Output: Clone;
}

impl<A> Indices<'_, usize> for [A] {
    type Output = A;

    fn indices<I>(&self, indices: I) -> Result<Vec<&Self::Output>, IndexOutOfBounds<usize>>
    where
        I: IntoIterator<Item = usize>,
    {
        indices
            .into_iter()
            .map(|i| self.get(i).ok_or(IndexOutOfBounds(i)))
            .collect()
    }

    fn cloned_indices<I>(&self, indices: I) -> Result<Vec<Self::Output>, IndexOutOfBounds<usize>>
    where
        I: IntoIterator<Item = usize>,
        A: Clone,
    {
        indices
            .into_iter()
            .map(|i| self.get(i).cloned().ok_or(IndexOutOfBounds(i)))
            .collect()
    }
}

impl<'idx, K, Q, V> Indices<'idx, &'idx Q> for HashMap<K, V>
where
    K: Eq + Hash + Borrow<Q>,
    Q: Eq + Hash,
{
    type Output = V;

    fn indices<I>(&self, indices: I) -> Result<Vec<&Self::Output>, IndexOutOfBounds<&'idx Q>>
    where
        I: IntoIterator<Item = &'idx Q>,
    {
        indices
            .into_iter()
            .map(|i| self.get(i).ok_or(IndexOutOfBounds(i)))
            .collect()
    }

    fn cloned_indices<I>(&self, indices: I) -> Result<Vec<Self::Output>, IndexOutOfBounds<&'idx Q>>
    where
        I: IntoIterator<Item = &'idx Q>,
        V: Clone,
    {
        indices
            .into_iter()
            .map(|i| self.get(i).cloned().ok_or(IndexOutOfBounds(i)))
            .collect()
    }
}

impl<'idx, K, Q, V> Indices<'idx, &'idx Q> for BTreeMap<K, V>
where
    K: Eq + Ord + Borrow<Q>,
    Q: Eq + Ord,
{
    type Output = V;

    fn indices<I>(&self, indices: I) -> Result<Vec<&Self::Output>, IndexOutOfBounds<&'idx Q>>
    where
        I: IntoIterator<Item = &'idx Q>,
    {
        indices
            .into_iter()
            .map(|i| self.get(i).ok_or(IndexOutOfBounds(i)))
            .collect()
    }

    fn cloned_indices<I>(&self, indices: I) -> Result<Vec<Self::Output>, IndexOutOfBounds<&'idx Q>>
    where
        I: IntoIterator<Item = &'idx Q>,
        V: Clone,
    {
        indices
            .into_iter()
            .map(|i| self.get(i).cloned().ok_or(IndexOutOfBounds(i)))
            .collect()
    }
}
