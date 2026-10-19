//! Fractional sort keys: short strings over the alphabet `a..=z` that order
//! lexicographically and always leave room between two distinct neighbours,
//! so a move rewrites exactly one key and precision never runs out.

const MIN: u8 = b'a';
const BASE: u8 = 26;

/// Whether `key` is a well-formed sort key: non-empty, lowercase ASCII, and
/// not ending in the minimum letter (a trailing `a` would leave no room
/// before the key).
pub(crate) fn is_valid(key: &str) -> bool {
    key.bytes().all(|b| b.is_ascii_lowercase()) && !key.is_empty() && !key.ends_with('a')
}

/// A key strictly between `lower` and `upper`; `None` is the open end.
/// Both bounds must be valid keys with `lower < upper`; an `upper` that does
/// not exceed `lower` is treated as open.
pub(crate) fn between(lower: Option<&str>, upper: Option<&str>) -> String {
    let lower = lower.unwrap_or("");
    let upper = upper.filter(|upper| *upper > lower);
    if lower.is_empty() && upper.is_none() {
        return char::from(MIN + BASE / 2).to_string();
    }
    midpoint(lower.as_bytes(), upper.map(str::as_bytes))
}

fn digit(bytes: &[u8], index: usize) -> u8 {
    bytes.get(index).map_or(0, |b| b - MIN)
}

fn midpoint(lower: &[u8], upper: Option<&[u8]>) -> String {
    if let Some(upper) = upper {
        let mut common = 0;
        while common < upper.len() && digit(lower, common) == upper[common] - MIN {
            common += 1;
        }
        if common > 0 {
            let lower_rest = lower.get(common..).unwrap_or(&[]);
            let mut key = String::from_utf8_lossy(&upper[..common]).into_owned();
            key.push_str(&midpoint(lower_rest, Some(&upper[common..])));
            return key;
        }
    }
    let low = digit(lower, 0);
    if upper.is_none() && low < BASE - 1 {
        // Appending steps by one letter instead of halving the remaining
        // range, so a run of appends lengthens the key by a letter only every
        // 25 keys.
        return char::from(MIN + low + 1).to_string();
    }
    let high = upper.map_or(BASE, |upper| digit(upper, 0));
    if high - low > 1 {
        return char::from(MIN + (low + high) / 2).to_string();
    }
    match upper {
        Some(upper) if upper.len() > 1 => char::from(upper[0]).to_string(),
        _ => {
            let mut key = char::from(MIN + low).to_string();
            key.push_str(&midpoint(lower.get(1..).unwrap_or(&[]), None));
            key
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_inserts_between_the_same_neighbours_stay_strictly_ordered() {
        let (lower, upper) = ("n".to_owned(), "o".to_owned());
        let mut newest_below_upper = upper.clone();
        let mut newest_above_lower = lower.clone();
        let mut below = Vec::new();
        let mut above = Vec::new();
        for _ in 0..1000 {
            let key = between(Some(&lower), Some(&newest_below_upper));
            assert!(is_valid(&key), "{key}");
            assert!(lower < key && key < newest_below_upper);
            newest_below_upper = key.clone();
            below.push(key);
            let key = between(Some(&newest_above_lower), Some(&upper));
            assert!(is_valid(&key), "{key}");
            assert!(newest_above_lower < key && key < upper);
            newest_above_lower = key.clone();
            above.push(key);
        }
        let mut sorted = below.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), below.len());
        assert!(below.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(above.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn open_ends_extend_in_both_directions() {
        let mut last = between(None, None);
        for _ in 0..200 {
            let next = between(Some(&last), None);
            assert!(is_valid(&next) && next > last);
            last = next;
        }
        assert!(last.len() <= 10, "{last}");
        let mut first = between(None, None);
        for _ in 0..200 {
            let previous = between(None, Some(&first));
            assert!(is_valid(&previous) && previous < first);
            first = previous;
        }
    }
}
