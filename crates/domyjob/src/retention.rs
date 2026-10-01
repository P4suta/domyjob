pub(crate) fn beyond_newest<T: Ord>(mut items: Vec<T>, keep: usize) -> Vec<T> {
    items.sort_unstable_by(|left, right| right.cmp(left));
    items.split_off(keep.min(items.len()))
}

#[cfg(test)]
mod tests {
    use super::beyond_newest;

    #[test]
    fn the_newest_stay_and_the_rest_are_returned_oldest_last() {
        assert_eq!(beyond_newest(vec![3, 1, 4, 2], 2), [2, 1]);
        assert_eq!(beyond_newest(vec![1], 2).len(), 0);
    }
}
