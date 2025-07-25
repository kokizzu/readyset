//! Macros that generate proptest test suites checking laws of stdlib traits

/// Generate a suite of tests to check the laws of the [`Eq`] impl for the given type
///
/// This generates 3 tests:
///
/// * One to check reflexivity; that `∀ x. x == x`
/// * One to check symmetry; that `∀ x y. (x == y) == (y == x)`
/// * One to check transitivity; that `∀ x y z, x == y ∧ y == z → x == z`
///
/// # Examples
///
/// ```
/// #[derive(Debug, Eq, PartialEq)]
/// struct MyType;
///
/// #[cfg(test)]
/// mod tests {
///     use readyset_util::eq_laws;
///
///     eq_laws!(MyType);
/// }
/// ```
#[macro_export]
macro_rules! eq_laws {
    ($ty: ty) => {
        $crate::eq_laws!(
            #[strategy(::proptest::arbitrary::any::<$ty>())]
            $ty
        );
    };
    (#[$meta: meta] $ty: ty) => {
        #[allow(clippy::eq_op)]
        mod eq {
            use super::*;

            #[::test_utils::tags(no_retry)]
            #[::test_strategy::proptest]
            fn reflexive(#[$meta] x: $ty) {
                assert!(x == x);
            }

            #[::test_utils::tags(no_retry)]
            #[::test_strategy::proptest]
            fn symmetric(#[$meta] x: $ty, #[$meta] y: $ty) {
                assert_eq!(x == y, y == x);
            }

            #[::test_utils::tags(no_retry)]
            #[::test_strategy::proptest]
            fn transitive(#[$meta] x: $ty, #[$meta] y: $ty, #[$meta] z: $ty) {
                if x == y && y == z {
                    assert!(x == z);
                }
            }
        }
    };
}

/// Generate a suite of tests to check the laws of the [`Ord`] impl for the given type
///
/// # Examples
///
/// ```
/// #[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
/// struct MyType;
///
/// #[cfg(test)]
/// mod tests {
///     use readyset_util::ord_laws;
///
///     ord_laws!(MyType);
/// }
/// ```
#[macro_export]
macro_rules! ord_laws {
    ($ty: ty) => {
        $crate::ord_laws!(
            #[strategy(::proptest::arbitrary::any::<$ty>())]
            $ty
        );
    };
    (#[$meta: meta] $ty: ty) => {
        mod ord {
            use super::*;

            #[::test_strategy::proptest]
            fn partial_cmp_matches_cmp(#[$meta] x: $ty, #[$meta] y: $ty) {
                assert_eq!(x.partial_cmp(&y), Some(x.cmp(&y)));
            }

            #[::test_strategy::proptest]
            fn dual(#[$meta] x: $ty, #[$meta] y: $ty) {
                if x < y {
                    assert!(y > x);
                }
                if y < x {
                    assert!(x > y);
                }
            }

            #[::test_strategy::proptest]
            fn le_transitive(#[$meta] x: $ty, #[$meta] y: $ty, #[$meta] z: $ty) {
                if x < y && y < z {
                    assert!(x < z)
                }
            }

            #[::test_strategy::proptest]
            fn gt_transitive(#[$meta] x: $ty, #[$meta] y: $ty, #[$meta] z: $ty) {
                if x > y && y > z {
                    assert!(x > z)
                }
            }

            #[::test_strategy::proptest]
            fn trichotomy(#[$meta] x: $ty, #[$meta] y: $ty) {
                let less = x < y;
                let greater = x > y;
                let eq = x == y;

                if less {
                    assert!(!greater);
                    assert!(!eq);
                }

                if greater {
                    assert!(!less);
                    assert!(!eq);
                }

                if eq {
                    assert!(!less);
                    assert!(!greater);
                }
            }
        }
    };
}

/// Generate a test to check the laws of the [`Hash`] impl for the given type
///
/// # Examples
///
/// ```
/// #[derive(Eq, PartialEq, Hash)]
/// struct MyType;
///
/// #[cfg(test)]
/// mod tests {
///     use readyset_util::hash_laws;
///
///     hash_laws!(MyType);
/// }
/// ```
#[macro_export]
macro_rules! hash_laws {
    ($ty: ty) => {
        $crate::hash_laws!(
            #[strategy(::proptest::arbitrary::any::<$ty>())]
            $ty
        );
    };
    (#[$meta: meta] $ty: ty) => {
        mod hash {
            use super::*;

            #[::test_strategy::proptest]
            fn matches_eq(#[$meta] x: $ty, #[$meta] y: $ty) {
                if x == y {
                    assert_eq!($crate::hash::hash(&x), $crate::hash::hash(&y));
                }
            }
        }
    };
}
