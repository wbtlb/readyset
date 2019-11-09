use crate::backlog::RangeLookupMiss;
use ahash::RandomState;
use common::DataType;
use evbtree::{self, refs::Values};
use noria::util::BoundFunctor;
use std::{mem, ops::RangeBounds};
use unbounded_interval_tree::IntervalTree;

#[derive(Clone, Debug)]
pub(super) enum Handle {
    Single(
        evbtree::handles::ReadHandle<DataType, Vec<DataType>, i64, RandomState>,
        IntervalTree<DataType>,
    ),
    Double(
        evbtree::handles::ReadHandle<(DataType, DataType), Vec<DataType>, i64, RandomState>,
        IntervalTree<(DataType, DataType)>,
    ),
    Many(
        evbtree::handles::ReadHandle<Vec<DataType>, Vec<DataType>, i64, RandomState>,
        IntervalTree<Vec<DataType>>,
    ),
}

pub(super) unsafe fn slice_to_2_tuple<T>(slice: &[T]) -> (T, T) {
    assert_eq!(slice.len(), 2);
    // we want to transmute &[T; 2] to &(T, T), but that's not actually safe
    // we're not guaranteed that they have the same memory layout
    // we *could* just clone DataType, but that would mean dealing with string refcounts
    // so instead, we play a trick where we memcopy onto the stack and then forget!
    //
    // h/t https://gist.github.com/mitsuhiko/f6478a0dd1ef174b33c63d905babc89a
    use std::ptr;
    let mut res: (mem::MaybeUninit<T>, mem::MaybeUninit<T>) =
        (mem::MaybeUninit::uninit(), mem::MaybeUninit::uninit());
    ptr::copy_nonoverlapping(&slice[0] as *const T, res.0.as_mut_ptr(), 1);
    ptr::copy_nonoverlapping(&slice[1] as *const T, res.1.as_mut_ptr(), 1);
    core::ptr::read(&res as *const (mem::MaybeUninit<T>, mem::MaybeUninit<T>) as *const (T, T))
}

impl Handle {
    pub(super) fn len(&self) -> usize {
        match *self {
            Handle::Single(ref h, _) => h.len(),
            Handle::Double(ref h, _) => h.len(),
            Handle::Many(ref h, _) => h.len(),
        }
    }

    pub(super) fn meta_get_and<F, T>(&self, key: &[DataType], then: F) -> Option<(Option<T>, i64)>
    where
        F: FnOnce(&Values<Vec<DataType>, RandomState>) -> T,
    {
        match *self {
            Handle::Single(ref h, _) => {
                assert_eq!(key.len(), 1);
                let map = h.enter()?;
                let v = map.get(&key[0]).map(then);
                let m = *map.meta();
                Some((v, m))
            }
            Handle::Double(ref h, _) => {
                assert_eq!(key.len(), 2);
                unsafe {
                    let tuple_key = slice_to_2_tuple(&key);
                    let map = h.enter()?;
                    let v = map.get(&tuple_key).map(then);
                    mem::forget(tuple_key);
                    let m = *map.meta();
                    Some((v, m))
                }
            }
            Handle::Many(ref h, _) => {
                let map = h.enter()?;
                let v = map.get(key).map(then);
                let m = *map.meta();
                Some((v, m))
            }
        }
    }

    pub(super) fn keys(&self) -> Vec<Vec<DataType>> {
        match *self {
            Handle::Single(ref h, _) => h.map_into(|k, _| vec![k.clone()]),
            Handle::Double(ref h, _) => h.map_into(|(k1, k2), _| vec![k1.clone(), k2.clone()]),
            Handle::Many(ref h, _) => h.map_into(|ks, _| ks.clone()),
        }
    }

    /// Retrieve the values corresponding to the given range of keys, apply `then` to them, and
    /// return the results, along with the metadata
    ///
    /// # Panics
    ///
    /// Panics if the vectors in the bounds of `range` are a different size than the length of our
    /// keys.
    pub(super) fn meta_get_range_and<F, T, R>(
        &self,
        range: R,
        mut then: F,
    ) -> Option<Result<(Vec<T>, i64), RangeLookupMiss>>
    where
        F: FnMut(&Values<Vec<DataType>, RandomState>) -> T,
        R: RangeBounds<Vec<DataType>>,
    {
        match *self {
            Handle::Single(ref h, ref t) => {
                let start_bound = range.start_bound().map(|v| {
                    assert!(v.len() == 1);
                    &v[0]
                });
                let end_bound = range.end_bound().map(|v| {
                    assert!(v.len() == 1);
                    &v[0]
                });
                let range = (start_bound.cloned(), end_bound.cloned());
                let diff = t.get_interval_difference(range.clone());
                if diff.is_empty() {
                    let map = h.enter()?;
                    let meta = h.meta()?;
                    Some(Ok((
                        map.range(range).map(|(_, row)| then(row)).collect(),
                        *meta,
                    )))
                } else {
                    Some(Err(RangeLookupMiss::Single(diff)))
                }
            }
            Handle::Double(ref h, ref t) => {
                let start_bound = range.start_bound().map(|r| {
                    assert_eq!(r.len(), 2);
                    (r[0].clone(), r[1].clone())
                });
                let end_bound = range.end_bound().map(|r| {
                    assert_eq!(r.len(), 2);
                    (r[0].clone(), r[1].clone())
                });
                let range = (start_bound, end_bound);
                let diff = t.get_interval_difference(range.clone());
                if diff.is_empty() {
                    let map = h.enter()?;
                    let meta = h.meta()?;
                    Some(Ok((
                        map.range(range).map(|(_, row)| then(row)).collect(),
                        *meta,
                    )))
                } else {
                    Some(Err(RangeLookupMiss::Double(diff)))
                }
            }
            Handle::Many(ref h, ref t) => {
                let range = (range.start_bound().cloned(), range.end_bound().cloned());
                let diff = t.get_interval_difference(range.clone());
                if diff.is_empty() {
                    let map = h.enter()?;
                    let meta = h.meta()?;
                    Some(Ok((
                        map.range(range).map(|(_, row)| then(row)).collect(),
                        *meta,
                    )))
                } else {
                    Some(Err(RangeLookupMiss::Many(diff)))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::*;
    use evbtree::handles::WriteHandle;
    use proptest::prelude::*;

    fn make_single() -> (
        WriteHandle<DataType, Vec<DataType>, i64, RandomState>,
        Handle,
    ) {
        let (w, r) = evbtree::Options::default()
            .with_meta(-1)
            .with_hasher(RandomState::default())
            .construct();
        (w, Handle::Single(r, IntervalTree::default()))
    }

    fn make_double() -> (
        WriteHandle<(DataType, DataType), Vec<DataType>, i64, RandomState>,
        Handle,
    ) {
        let (w, r) = evbtree::Options::default()
            .with_meta(-1)
            .with_hasher(RandomState::default())
            .construct();
        (w, Handle::Double(r, IntervalTree::default()))
    }

    proptest! {
        #[test]
        fn horrible_cursed_transmute(x: i32, y: i32) {
            unsafe {
                let arg = [x, y];
                let result = slice_to_2_tuple(&arg);
                assert_eq!(result, (x, y));
                mem::forget(result)
            }
        }

        #[test]
        fn get_double(key: (DataType, DataType), val: Vec<DataType>) {
            let (mut w, handle) = make_double();
            w.insert(key.clone(), val.clone());
            w.publish();
            handle.meta_get_and(&[key.0, key.1], |result| {
                assert_eq!(result.into_iter().cloned().collect::<Vec<_>>(), vec![val]);
            }).unwrap();
        }
    }

    #[test]
    fn get_single_range() {
        let (mut w, mut handle) = make_single();
        for n in 0..10 {
            w.insert(n.into(), vec![n.into(), n.into()]);
        }
        w.publish();
        match handle {
            Handle::Single(_, ref mut t) => t.insert((
                Bound::Included(DataType::from(2)),
                Bound::Included(DataType::from(3)),
            )),
            _ => unreachable!(),
        }
        let (res, meta) = handle
            .meta_get_range_and(vec![2.into()]..=vec![3.into()], |vals| {
                vals.into_iter().cloned().collect::<Vec<_>>()
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            res,
            (2..=3)
                .map(|n| vec![vec![DataType::from(n), DataType::from(n)]])
                .collect::<Vec<_>>()
        );
        assert_eq!(meta, -1);
    }

    #[test]
    fn get_double_range() {
        let (mut w, mut handle) = make_double();
        for n in 0..10 {
            w.insert((n.into(), n.into()), vec![n.into(), n.into()]);
        }
        w.publish();
        match handle {
            Handle::Double(_, ref mut t) => t.insert((
                Bound::Included((2.into(), 2.into())),
                Bound::Included((3.into(), 3.into())),
            )),
            _ => unreachable!(),
        }
        let (res, meta) = handle
            .meta_get_range_and(
                vec![2.into(), 2.into()]..=vec![3.into(), 3.into()],
                |vals| vals.into_iter().cloned().collect::<Vec<_>>(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            res,
            (2..=3)
                .map(|n| vec![vec![DataType::from(n), DataType::from(n)]])
                .collect::<Vec<_>>()
        );
        assert_eq!(meta, -1);
    }
}
