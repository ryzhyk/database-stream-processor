use std::{cmp::min, mem::MaybeUninit};

const DEFAULT_SMALL_LIMIT: usize = 8;

/// Reports the number of elements satisfying the predicate.
///
/// This methods *relies strongly* on the assumption that the predicate
/// stays false once it becomes false, a joint property of the predicate
/// and the slice. This allows `advance` to use exponential search to
/// count the number of elements in time logarithmic in the result.
pub fn advance<T, F>(slice: &[T], function: F) -> usize
where
    F: Fn(&T) -> bool,
{
    advance_raw::<T, F, DEFAULT_SMALL_LIMIT>(slice, function)
}

/// Reports the number of elements satisfying the predicate with the additional
/// ability to specify the limit for linear searches
///
/// This methods *relies strongly* on the assumption that the predicate
/// stays false once it becomes false, a joint property of the predicate
/// and the slice. This allows `advance` to use exponential search to
/// count the number of elements in time logarithmic in the result.
pub fn advance_raw<T, F, const SMALL_LIMIT: usize>(slice: &[T], function: F) -> usize
where
    F: Fn(&T) -> bool,
{
    // Exponential search if the answer isn't within `SMALL_LIMIT`.
    if slice.len() > SMALL_LIMIT && function(&slice[SMALL_LIMIT]) {
        // Skip `slice[..SMALL_LIMIT]` outright, the above condition established
        // that nothing within it satisfies the predicate
        let mut index = SMALL_LIMIT + 1;

        // FIXME: This is kinda weird
        if index < slice.len() && function(&slice[index]) {
            // Advance in exponentially growing steps
            let mut step = 1;
            while index + step < slice.len() && function(&slice[index + step]) {
                index += step;
                step <<= 1;
            }

            // Advance in exponentially shrinking steps
            step >>= 1;
            while step > 0 {
                if index + step < slice.len() && function(&slice[index + step]) {
                    index += step;
                }
                step >>= 1;
            }

            index += 1;
        }

        index

    // If `slice[SMALL_LIMIT..]` doesn't satisfy the predicate, we can simply
    // perform a linear search on `slice[..SMALL_LIMIT]`
    } else {
        // Clamp to the length of the slice, this branch will also be hit if the slice
        // is smaller than SMALL_LIMIT
        let limit = min(slice.len(), SMALL_LIMIT);

        slice[..limit]
            .iter()
            .position(|x| !function(x))
            // If nothing within `slice[..limit]` satisfies the predicate, we can advance
            // past the searched prefix
            .unwrap_or(limit)
    }
}

pub fn advance_erased<F>(slice: &[MaybeUninit<u8>], size: usize, function: F) -> usize
where
    F: Fn(*const u8) -> bool,
{
    let slice = SlicePtr::new(slice, size);
    if slice.is_empty() {
        return 0;
    }

    // We have to use `.get_unchecked()` here since otherwise LLVM's not smart
    // enough to elide bounds checking (we still get checks in debug mode though)
    unsafe {
        // Exponential search if the answer isn't within `small_limit`.
        if slice.len() > DEFAULT_SMALL_LIMIT && function(slice.get_unchecked(DEFAULT_SMALL_LIMIT)) {
            // start with no advance
            let mut index = DEFAULT_SMALL_LIMIT + 1;
            if index < slice.len() && function(slice.get_unchecked(index)) {
                // advance in exponentially growing steps.
                let mut step = 1;
                while index + step < slice.len() && function(slice.get_unchecked(index + step)) {
                    index += step;
                    step <<= 1;
                }

                // advance in exponentially shrinking steps.
                step >>= 1;
                while step > 0 {
                    if index + step < slice.len() && function(slice.get_unchecked(index + step)) {
                        index += step;
                    }
                    step >>= 1;
                }

                index += 1;
            }

            index
        } else {
            let limit = min(slice.len(), DEFAULT_SMALL_LIMIT);
            for offset in 0..limit {
                if !function(slice.get_unchecked(offset)) {
                    return offset;
                }
            }

            limit
        }
    }
}

struct SlicePtr {
    ptr: *const u8,
    elements: usize,
    element_size: usize,
}

impl SlicePtr {
    #[inline]
    fn new(slice: &[MaybeUninit<u8>], element_size: usize) -> Self {
        assert!(slice.len() % element_size == 0);

        Self {
            ptr: slice.as_ptr().cast(),
            elements: slice.len() / element_size,
            element_size,
        }
    }

    #[inline]
    const fn len(&self) -> usize {
        self.elements
    }

    #[inline]
    const fn is_empty(&self) -> bool {
        self.elements == 0
    }

    #[inline]
    unsafe fn get_unchecked(&self, idx: usize) -> *const u8 {
        debug_assert!(idx < self.elements);
        unsafe { self.ptr.add(idx * self.element_size) }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        trace::layers::advance::{advance, advance_erased, DEFAULT_SMALL_LIMIT},
        utils::bytes_of,
    };
    use proptest::{
        arbitrary::any, collection::vec, prop_assert_eq, proptest, sample::SizeRange,
        strategy::Strategy, test_runner::TestCaseResult,
    };
    use std::mem::{align_of, size_of};

    const HALF: usize = usize::MAX / 2;

    #[test]
    fn advance_empty() {
        // Haystack that's smaller than `DEFAULT_SMALL_LIMIT`
        let haystack = &[false, false, false, false, false];
        assert_eq!(advance(haystack, |&x| x), 0);

        // Haystack that turns `false` before `DEFAULT_SMALL_LIMIT`
        let haystack = &[
            false, false, false, false, false, false, false, false, false, false,
        ];
        assert_eq!(advance(haystack, |&x| x), 0);
    }

    #[test]
    fn advance_small() {
        // Haystack that's smaller than `DEFAULT_SMALL_LIMIT`
        let haystack = &[true, true, false, false, false];
        assert_eq!(advance(haystack, |&x| x), 2);

        // Haystack that turns `false` before `DEFAULT_SMALL_LIMIT`
        let haystack = &[
            true, true, true, false, false, false, false, false, false, false,
        ];
        assert_eq!(advance(haystack, |&x| x), 3);
    }

    #[test]
    fn advance_medium() {
        // Haystack that's longer than `DEFAULT_SMALL_LIMIT`
        let haystack = &[
            true, true, true, true, true, true, true, true, true, true, false, false, false,
        ];
        assert_eq!(advance(haystack, |&x| x), 10);
    }

    #[test]
    fn advance_erased_empty() {
        // Haystack that's smaller than `DEFAULT_SMALL_LIMIT`
        let haystack: &[u64] = &[5, 5, 5, 5, 5];

        let count = advance_erased(bytes_of(haystack), size_of::<u64>(), |x| unsafe {
            let value = *x.cast::<u64>();
            assert_eq!(value, 5);
            value == 1
        });
        assert_eq!(count, 0);

        // Haystack that turns `5` before `DEFAULT_SMALL_LIMIT`
        let haystack: &[u64] = &[5, 5, 5, 5, 5, 5, 5, 5, 5, 5];

        let count = advance_erased(bytes_of(haystack), size_of::<u64>(), |x| unsafe {
            let value = *x.cast::<u64>();
            assert_eq!(value, 5);
            value == 1
        });
        assert_eq!(count, 0);
    }

    #[test]
    fn advance_erased_small() {
        // Haystack that's smaller than `DEFAULT_SMALL_LIMIT`
        let haystack: &[u64] = &[1, 1, 568, 568, 568];

        let count = advance_erased(bytes_of(haystack), size_of::<u64>(), |x| unsafe {
            let value = *x.cast::<u64>();
            assert!(value == 1 || value == 568);
            value == 1
        });
        assert_eq!(count, 2);

        // Haystack that turns false before `DEFAULT_SMALL_LIMIT`
        let haystack: &[u64] = &[1, 1, 1, 568, 568, 568, 568, 568, 568, 568];

        let count = advance_erased(bytes_of(haystack), size_of::<u64>(), |x| unsafe {
            let value = *x.cast::<u64>();
            assert!(value == 1 || value == 568);
            value == 1
        });
        assert_eq!(count, 3);
    }

    #[test]
    fn advance_erased_medium() {
        // Haystack that's longer than `DEFAULT_SMALL_LIMIT`
        let haystack: &[u64] = &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 568, 568, 568];

        let count = advance_erased(bytes_of(haystack), size_of::<u64>(), |x| unsafe {
            let value = *x.cast::<u64>();
            assert!(value == 1 || value == 568);
            value == 1
        });
        assert_eq!(count, 10);
    }

    fn haystack(
        length: impl Into<SizeRange>,
        value: impl Strategy<Value = usize>,
    ) -> impl Strategy<Value = Vec<usize>> {
        vec(value, length.into()).prop_map(|mut vec| {
            vec.sort();
            vec
        })
    }

    fn advance_test(needle: usize, haystack: &[usize]) -> TestCaseResult {
        let count = advance(haystack, |&x| x < needle);
        let expected = haystack
            .iter()
            .position(|&x| x >= needle)
            .unwrap_or(haystack.len());

        prop_assert_eq!(count, expected);
        Ok(())
    }

    fn advance_erased_test(needle: usize, haystack: &[usize]) -> TestCaseResult {
        let count = advance_erased(bytes_of(haystack), size_of::<usize>(), |x| unsafe {
            assert!(
                x as usize & (align_of::<usize>() - 1) == 0,
                "unaligned pointer",
            );
            *x.cast::<usize>() < needle
        });
        let expected = haystack
            .iter()
            .position(|&x| x >= needle)
            .unwrap_or(haystack.len());

        prop_assert_eq!(count, expected);
        Ok(())
    }

    proptest! {
        #[test]
        fn advance_less_than(needle in any::<usize>(), haystack in haystack(0..100_000usize, any::<usize>())) {
            advance_test(needle, &haystack)?;
        }

        // Force `advance()` to search the entire haystack
        #[test]
        fn advance_less_than_unsat(needle in ..HALF, haystack in haystack(0..100_000usize, HALF..)) {
            advance_test(needle, &haystack)?;
        }

        // Ensure that we check the case of the haystack being shorter than `DEFAULT_SMALL_LIMIT`
        #[test]
        fn advance_less_than_small(needle in any::<usize>(), haystack in haystack(0..=DEFAULT_SMALL_LIMIT, any::<usize>())) {
            advance_test(needle, &haystack)?;
        }

        // Force `advance()` to search the entire haystack
        #[test]
        fn advance_less_than_small_unsat(needle in ..HALF, haystack in haystack(0..=DEFAULT_SMALL_LIMIT, HALF..)) {
            advance_test(needle, &haystack)?;
        }

        #[test]
        fn advance_erased_less_than(needle in any::<usize>(), haystack in haystack(0..100_000usize, any::<usize>())) {
            advance_erased_test(needle, &haystack)?;
        }

        // Force `advance_erased()` to search the entire haystack
        #[test]
        fn advance_erased_less_than_unsat(needle in ..HALF, haystack in haystack(0..100_000usize, HALF..)) {
            advance_erased_test(needle, &haystack)?;
        }

        // Ensure that we check the case of the haystack being shorter than `DEFAULT_SMALL_LIMIT`
        #[test]
        fn advance_erased_less_than_small(needle in any::<usize>(), haystack in haystack(0..=DEFAULT_SMALL_LIMIT, any::<usize>())) {
            advance_erased_test(needle, &haystack)?;
        }

        // Force `advance_erased()` to search the entire haystack
        #[test]
        fn advance_erased_less_than_small_unsat(needle in ..HALF, haystack in haystack(0..=DEFAULT_SMALL_LIMIT, HALF..)) {
            advance_erased_test(needle, &haystack)?;
        }
    }
}
