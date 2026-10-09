use reports::stats::{mean, peak};
use reports::table;

#[test]
fn summary_table_lists_mean_and_peak() {
    let values = [4, 8, 6];
    let lines = table("week", &[("mean", mean(&values)), ("peak", peak(&values))]);
    assert_eq!(lines.len(), 3);
    assert!(lines[2].ends_with('8'));
}
