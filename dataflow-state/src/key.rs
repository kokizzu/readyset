use common::DfValue;
use derive_more::From;
use readyset_data::{Bound, BoundedRange, RangeBounds, TextRef};
use serde::ser::{SerializeSeq, SerializeTuple};
use serde::Serialize;
use test_strategy::Arbitrary;
use tuple::TupleElements;
use vec1::Vec1;

/// An internal type used as the key when performing point lookups and inserts into node state.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, From, Arbitrary)]
pub enum PointKey {
    Empty,
    Single(DfValue),
    Double((DfValue, DfValue)),
    Tri((DfValue, DfValue, DfValue)),
    Quad((DfValue, DfValue, DfValue, DfValue)),
    Quin((DfValue, DfValue, DfValue, DfValue, DfValue)),
    Sex((DfValue, DfValue, DfValue, DfValue, DfValue, DfValue)),
    Multi(#[any(((7usize..100).into(), Default::default()))] Box<[DfValue]>),
}

#[allow(clippy::len_without_is_empty)]
impl PointKey {
    /// Return the value at the given index within this [`PointKey`].
    ///
    /// # Panics
    ///
    /// * Panics if the index is out-of-bounds
    pub fn get(&self, idx: usize) -> Option<&DfValue> {
        match self {
            PointKey::Empty => panic!("get() called on PointKey::Empty"),
            PointKey::Single(x) if idx == 0 => Some(x),
            PointKey::Single(_) => None,
            PointKey::Double(x) => TupleElements::get(x, idx),
            PointKey::Tri(x) => TupleElements::get(x, idx),
            PointKey::Quad(x) => TupleElements::get(x, idx),
            PointKey::Quin(x) => TupleElements::get(x, idx),
            PointKey::Sex(x) => TupleElements::get(x, idx),
            PointKey::Multi(arr) => arr.get(idx),
        }
    }

    /// Construct a [`PointKey`] from an iterator of [`DfValue`]s
    #[track_caller]
    pub fn from<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = DfValue>,
        <I as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        let mut iter = iter.into_iter();
        let len = iter.len();
        let mut more = move || iter.next().unwrap().normalize();
        match len {
            0 => PointKey::Empty,
            1 => PointKey::Single(more()),
            2 => PointKey::Double((more(), more())),
            3 => PointKey::Tri((more(), more(), more())),
            4 => PointKey::Quad((more(), more(), more(), more())),
            5 => PointKey::Quin((more(), more(), more(), more(), more())),
            6 => PointKey::Sex((more(), more(), more(), more(), more(), more())),
            x => PointKey::Multi((0..x).map(|_| more()).collect()),
        }
    }

    /// Return the length of this key
    pub fn len(&self) -> usize {
        match self {
            PointKey::Empty => 0,
            PointKey::Single(_) => 1,
            PointKey::Double(_) => 2,
            PointKey::Tri(_) => 3,
            PointKey::Quad(_) => 4,
            PointKey::Quin(_) => 5,
            PointKey::Sex(_) => 6,
            PointKey::Multi(k) => k.len(),
        }
    }

    /// Return true if any of the elements is null
    pub fn has_null(&self) -> bool {
        match self {
            PointKey::Empty => false,
            PointKey::Single(e) => e.is_none(),
            PointKey::Double((e0, e1)) => e0.is_none() || e1.is_none(),
            PointKey::Tri((e0, e1, e2)) => e0.is_none() || e1.is_none() || e2.is_none(),
            PointKey::Quad((e0, e1, e2, e3)) => {
                e0.is_none() || e1.is_none() || e2.is_none() || e3.is_none()
            }
            PointKey::Quin((e0, e1, e2, e3, e4)) => {
                e0.is_none() || e1.is_none() || e2.is_none() || e3.is_none() || e4.is_none()
            }
            PointKey::Sex((e0, e1, e2, e3, e4, e5)) => {
                e0.is_none()
                    || e1.is_none()
                    || e2.is_none()
                    || e3.is_none()
                    || e4.is_none()
                    || e5.is_none()
            }
            PointKey::Multi(k) => k.iter().any(DfValue::is_none),
        }
    }
}

impl Serialize for PointKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        macro_rules! serialize_val {
            ($ser: ident, $v: ident) => {{
                let val = $v.transform_for_serialized_key();
                match val.as_str() {
                    Some(s) => $ser.serialize_element(&TextRef(s))?, // Don't serialize collation
                    None => $ser.serialize_element(val.as_ref())?,
                }
            }};
        }

        macro_rules! serialize {
            ($count: expr, $($v:ident),+) => {{
                let mut tup = serializer.serialize_tuple($count)?;
                $({ serialize_val!(tup, $v) })+
                tup.end()
            }};
        }

        match self {
            PointKey::Empty => serializer.serialize_unit(),
            PointKey::Single(v) => v.transform_for_serialized_key().serialize(serializer),
            PointKey::Double((v1, v2)) => serialize!(2, v1, v2),
            PointKey::Tri((v1, v2, v3)) => serialize!(3, v1, v2, v3),
            PointKey::Quad((v1, v2, v3, v4)) => serialize!(4, v1, v2, v3, v4),
            PointKey::Quin((v1, v2, v3, v4, v5)) => serialize!(5, v1, v2, v3, v4, v5),
            PointKey::Sex((v1, v2, v3, v4, v5, v6)) => serialize!(6, v1, v2, v3, v4, v5, v6),
            PointKey::Multi(vs) => {
                let mut seq = serializer.serialize_seq(Some(vs.len()))?;
                for v in vs.iter() {
                    serialize_val!(seq, v);
                }
                seq.end()
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub enum RangeKey {
    Single(BoundedRange<DfValue>),
    Double(BoundedRange<(DfValue, DfValue)>),
    Tri(BoundedRange<(DfValue, DfValue, DfValue)>),
    Quad(BoundedRange<(DfValue, DfValue, DfValue, DfValue)>),
    Quin(BoundedRange<(DfValue, DfValue, DfValue, DfValue, DfValue)>),
    Sex(BoundedRange<(DfValue, DfValue, DfValue, DfValue, DfValue, DfValue)>),
    Multi(BoundedRange<Box<[DfValue]>>),
}

#[allow(clippy::len_without_is_empty)]
impl RangeKey {
    /// Build a [`RangeKey`] from a type that implements [`RangeBounds`] over a vector of keys.
    ///
    /// # Panics
    ///
    /// Panics if the lengths of the bounds are different, or if the length is greater than 6
    ///
    /// # Examples
    ///
    /// ```rust
    /// use dataflow_state::RangeKey;
    /// use readyset_data::Bound::*;
    /// use readyset_data::DfValue;
    /// use vec1::vec1;
    ///
    /// // Can build RangeKeys from bounded range expressions
    /// assert_eq!(
    ///     RangeKey::from(&(vec1![DfValue::from(0)]..vec1![DfValue::from(1)])),
    ///     RangeKey::Single((Included(0.into()), Excluded(1.into())))
    /// );
    /// ```
    pub fn from<R>(range: &R) -> Self
    where
        R: RangeBounds<Vec1<DfValue>>,
    {
        use Bound::*;
        let len = match (range.start_bound(), range.end_bound()) {
            (Included(start) | Excluded(start), Included(end) | Excluded(end)) => {
                assert_eq!(start.len(), end.len());
                start.len()
            }
        };

        macro_rules! make {
            ($variant: ident, |$elem: ident| $make_tuple: expr) => {
                RangeKey::$variant((
                    make!(bound, start_bound, $elem, $make_tuple),
                    make!(bound, end_bound, $elem, $make_tuple),
                ))
            };
            (bound, $bound_type: ident, $elem: ident, $make_tuple: expr) => {
                range.$bound_type().map(|key| {
                    let mut key = key.into_iter();
                    let mut $elem = move || key.next().unwrap().clone();
                    $make_tuple
                })
            };
        }

        match len {
            0 => unreachable!("Vec1 cannot be empty"),
            1 => make!(Single, |elem| elem()),
            2 => make!(Double, |elem| (elem(), elem())),
            3 => make!(Tri, |elem| (elem(), elem(), elem())),
            4 => make!(Quad, |elem| (elem(), elem(), elem(), elem())),
            5 => make!(Quin, |elem| (elem(), elem(), elem(), elem(), elem())),
            6 => make!(Sex, |elem| (elem(), elem(), elem(), elem(), elem(), elem())),
            _ => RangeKey::Multi((
                range
                    .start_bound()
                    .map(|key| key.clone().into_vec().into_boxed_slice()),
                range
                    .end_bound()
                    .map(|key| key.clone().into_vec().into_boxed_slice()),
            )),
        }
    }
}

#[allow(clippy::len_without_is_empty)]
impl RangeKey {
    /// Returns the upper bound of the range key
    pub fn upper_bound(&self) -> Bound<Vec<&DfValue>> {
        match self {
            RangeKey::Single((_, upper)) => upper.as_ref().map(|dt| vec![dt]),
            RangeKey::Double((_, upper)) => upper.as_ref().map(|dts| dts.elements().collect()),
            RangeKey::Tri((_, upper)) => upper.as_ref().map(|dts| dts.elements().collect()),
            RangeKey::Quad((_, upper)) => upper.as_ref().map(|dts| dts.elements().collect()),
            RangeKey::Quin((_, upper)) => upper.as_ref().map(|dts| dts.elements().collect()),
            RangeKey::Sex((_, upper)) => upper.as_ref().map(|dts| dts.elements().collect()),
            RangeKey::Multi((_, upper)) => upper.as_ref().map(|dts| dts.iter().collect()),
        }
    }

    /// Return the length of this range key
    pub fn len(&self) -> usize {
        match self {
            RangeKey::Single(_) => 1,
            RangeKey::Double(_) => 2,
            RangeKey::Tri(_) => 3,
            RangeKey::Quad(_) => 4,
            RangeKey::Quin(_) => 5,
            RangeKey::Sex(_) => 6,
            RangeKey::Multi((Bound::Included(k), _) | (Bound::Excluded(k), _)) => k.len(),
        }
    }

    /// Convert this [`RangeKey`] into a pair of bounds on [`PointKey`]s, for use during
    /// serialization of lookup keys for ranges
    pub(crate) fn into_point_keys(self) -> BoundedRange<PointKey> {
        macro_rules! point_keys {
            ($r:ident, $variant:ident) => {
                ($r.0.map(PointKey::$variant), $r.1.map(PointKey::$variant))
            };
        }

        match self {
            RangeKey::Single((l, u)) => (l.map(PointKey::Single), u.map(PointKey::Single)),
            RangeKey::Double(r) => point_keys!(r, Double),
            RangeKey::Tri(r) => point_keys!(r, Tri),
            RangeKey::Quad(r) => point_keys!(r, Quad),
            RangeKey::Quin(r) => point_keys!(r, Quin),
            RangeKey::Sex(r) => point_keys!(r, Sex),
            RangeKey::Multi(r) => point_keys!(r, Multi),
        }
    }

    pub fn as_bounded_range(&self) -> BoundedRange<Vec<DfValue>> {
        fn as_bounded_range<T>(bound_pair: &BoundedRange<T>) -> BoundedRange<Vec<DfValue>>
        where
            T: TupleElements<Element = DfValue>,
        {
            (
                bound_pair
                    .0
                    .as_ref()
                    .map(|v| v.elements().cloned().collect()),
                bound_pair
                    .1
                    .as_ref()
                    .map(|v| v.elements().cloned().collect()),
            )
        }

        match self {
            RangeKey::Single((lower, upper)) => (
                lower.as_ref().map(|dt| vec![dt.clone()]),
                upper.as_ref().map(|dt| vec![dt.clone()]),
            ),
            RangeKey::Double(bp) => as_bounded_range(bp),
            RangeKey::Tri(bp) => as_bounded_range(bp),
            RangeKey::Quad(bp) => as_bounded_range(bp),
            RangeKey::Quin(bp) => as_bounded_range(bp),
            RangeKey::Sex(bp) => as_bounded_range(bp),
            RangeKey::Multi((lower, upper)) => (
                lower.as_ref().map(|dts| dts.to_vec()),
                upper.as_ref().map(|dts| dts.to_vec()),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use test_strategy::proptest;
    use test_utils::tags;

    use super::*;

    #[tags(no_retry)]
    #[proptest]
    fn single_point_key_serialize_injective(v1: DfValue, v2: DfValue) {
        let k1 = PointKey::Single(v1);
        let k2 = PointKey::Single(v2);
        assert_eq!(
            k1 == k2,
            bincode::serialize(&k1).unwrap() == bincode::serialize(&k2).unwrap()
        )
    }
}
