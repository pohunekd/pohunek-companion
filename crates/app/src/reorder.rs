//! Row moves between groups, independent of Slint.

/// Moves the item at `(from_group, from_index)` so it lands at insertion
/// `slot` of `to_group`. `slot` counts positions in the group before the
/// removal, so a move down within one group compensates for the shifted index.
/// Returns the index the item ends up at, or `None` when an index is out of range.
pub fn move_item<T>(
    groups: &mut [Vec<T>],
    from_group: usize,
    from_index: usize,
    to_group: usize,
    slot: usize,
) -> Option<usize> {
    let (Some(source), Some(target)) = (groups.get(from_group), groups.get(to_group)) else {
        return None;
    };
    if from_index >= source.len() || slot > target.len() {
        return None;
    }
    let slot = if from_group == to_group && slot > from_index {
        slot - 1
    } else {
        slot
    };
    let item = groups[from_group].remove(from_index);
    groups[to_group].insert(slot, item);
    Some(slot)
}

#[cfg(test)]
mod tests {
    use super::move_item;

    #[test]
    fn move_down_within_group_accounts_for_removal() {
        let mut groups = vec![vec![1, 2, 3, 4], vec![]];
        assert_eq!(move_item(&mut groups, 0, 0, 0, 3), Some(2));
        assert_eq!(groups[0], [2, 3, 1, 4]);
    }

    #[test]
    fn move_up_and_across_groups() {
        let mut groups = vec![vec![1, 2, 3], vec![9]];
        assert_eq!(move_item(&mut groups, 0, 2, 0, 0), Some(0));
        assert_eq!(groups[0], [3, 1, 2]);
        assert_eq!(move_item(&mut groups, 0, 1, 1, 1), Some(1));
        assert_eq!(groups, [vec![3, 2], vec![9, 1]]);
    }

    #[test]
    fn rejects_out_of_range() {
        let mut groups = vec![vec![1], vec![]];
        assert_eq!(move_item(&mut groups, 0, 1, 1, 0), None);
        assert_eq!(move_item(&mut groups, 0, 0, 1, 2), None);
        assert_eq!(groups, [vec![1], vec![]]);
    }
}
