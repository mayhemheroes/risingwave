use super::variants::BACKWARD;
use crate::hummock::iterator::sorted_inner::SortedIteratorInner;

pub type ReverseSortedIterator = SortedIteratorInner<BACKWARD>;

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use itertools::Itertools;

    use super::*;
    use crate::hummock::iterator::test_utils::{
        default_builder_opt_for_test, gen_test_table, iterator_test_key_of, test_key,
        test_value_of, TestIteratorBuilder, TEST_KEYS_COUNT,
    };
    use crate::hummock::iterator::{BoxedHummockIterator, HummockIterator};
    use crate::hummock::ReverseTableIterator;

    #[tokio::test]
    async fn test_reverse_sorted_basic() {
        let base_key_value = usize::MAX - 10;
        let (iters, validators): (Vec<_>, Vec<_>) = (0..3)
            .map(|iter_id| {
                TestIteratorBuilder::<BACKWARD>::default()
                    .id(0)
                    .map_key(move |id, x| {
                        iterator_test_key_of(id, base_key_value - x * 3 + (3 - iter_id as usize))
                    })
                    .map_value(move |id, x| {
                        test_value_of(id, base_key_value - x * 3 + (3 - iter_id as usize))
                    })
                    .finish()
            })
            .unzip();

        let iters: Vec<BoxedHummockIterator> = iters
            .into_iter()
            .map(|x| Box::new(x) as BoxedHummockIterator)
            .collect_vec();

        let mut mi = ReverseSortedIterator::new(iters);
        let mut i = 0;
        mi.rewind().await.unwrap();
        while mi.is_valid() {
            let key = mi.key();
            let val = mi.value();
            validators[i % 3].assert_key(i / 3, key);
            validators[i % 3].assert_hummock_value(i / 3, val);
            i += 1;
            mi.next().await.unwrap();
            if i == TEST_KEYS_COUNT * 3 {
                assert!(!mi.is_valid());
                break;
            }
        }
        assert!(i >= TEST_KEYS_COUNT);
    }

    #[tokio::test]
    async fn test_reverse_sorted_seek() {
        let base_key = usize::MAX - 100;
        let (iters, validators): (Vec<_>, Vec<_>) = (0..3)
            .map(|iter_id| {
                TestIteratorBuilder::<BACKWARD>::default()
                    .id(0)
                    .total(20)
                    .map_key(move |id, x| {
                        iterator_test_key_of(id, base_key - x * 3 + (3 - iter_id as usize))
                    })
                    .finish()
            })
            .unzip();
        let iters: Vec<BoxedHummockIterator> = iters
            .into_iter()
            .map(|x| Box::new(x) as BoxedHummockIterator)
            .collect_vec();

        let mut mi = ReverseSortedIterator::new(iters);
        let test_validator = &validators[2];

        // right edge case
        mi.seek(test_key!(test_validator, 3 * TEST_KEYS_COUNT))
            .await
            .unwrap();
        assert!(!mi.is_valid());

        // normal case
        mi.seek(test_key!(test_validator, 4)).await.unwrap();
        let k = mi.key();
        let v = mi.value();
        test_validator.assert_hummock_value(4, v);
        test_validator.assert_key(4, k);

        mi.seek(test_key!(test_validator, 17)).await.unwrap();
        let k = mi.key();
        let v = mi.value();
        test_validator.assert_hummock_value(17, v);
        test_validator.assert_key(17, k);

        // left edge case
        mi.seek(test_key!(test_validator, 0)).await.unwrap();
        let k = mi.key();
        let v = mi.value();
        test_validator.assert_hummock_value(0, v);
        test_validator.assert_key(0, k);
    }

    #[tokio::test]
    async fn test_reverse_sorted_invalidate_reset() {
        let table0 = gen_test_table(0, default_builder_opt_for_test()).await;
        let table1 = gen_test_table(1, default_builder_opt_for_test()).await;
        let iters: Vec<BoxedHummockIterator> = vec![
            Box::new(ReverseTableIterator::new(Arc::new(table1))),
            Box::new(ReverseTableIterator::new(Arc::new(table0))),
        ];

        let mut si = ReverseSortedIterator::new(iters);

        si.rewind().await.unwrap();
        let mut count = 0;
        while si.is_valid() {
            count += 1;
            si.next().await.unwrap();
        }
        assert_eq!(count, TEST_KEYS_COUNT * 2);

        si.rewind().await.unwrap();
        let mut count = 0;
        while si.is_valid() {
            count += 1;
            si.next().await.unwrap();
        }
        assert_eq!(count, TEST_KEYS_COUNT * 2);
    }
}
