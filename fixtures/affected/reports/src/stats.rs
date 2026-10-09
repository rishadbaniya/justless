/// The arithmetic mean, or zero for no values.
pub fn mean(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.iter().sum::<u64>() / values.len() as u64
}

/// The middle value of a sorted copy.
pub fn median(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// The largest value, or zero for no values.
pub fn peak(values: &[u64]) -> u64 {
    values.iter().copied().max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_of_nothing_is_zero() {
        assert_eq!(mean(&[]), 0);
    }

    #[test]
    fn mean_rounds_down() {
        assert_eq!(mean(&[1, 2]), 1);
    }

    #[test]
    fn median_sorts_first() {
        assert_eq!(median(&[9, 1, 5]), 5);
    }

    #[test]
    fn median_of_nothing_is_zero() {
        assert_eq!(median(&[]), 0);
    }

    #[test]
    fn peak_finds_the_largest() {
        assert_eq!(peak(&[3, 8, 2]), 8);
    }
}
