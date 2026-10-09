pub mod stats;

/// A fixed-width table row.
pub fn row(name: &str, value: u64) -> String {
    format!("{name:<12}{value:>8}")
}

/// Lines for a titled table.
pub fn table(title: &str, rows: &[(&str, u64)]) -> Vec<String> {
    let mut lines = vec![title.to_uppercase()];
    lines.extend(rows.iter().map(|(name, value)| row(name, *value)));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_pads_both_columns() {
        assert_eq!(row("tea", 3), "tea                3");
    }

    #[test]
    fn table_starts_with_the_title() {
        assert_eq!(table("sales", &[])[0], "SALES");
    }

    #[test]
    fn table_has_one_line_per_row() {
        assert_eq!(table("sales", &[("tea", 3), ("cake", 5)]).len(), 3);
    }
}
