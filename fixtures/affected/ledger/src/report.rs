/// Rows as comma-separated lines. No test covers this yet.
pub fn csv(rows: &[(&str, u64)]) -> String {
    rows.iter()
        .map(|(name, cents)| format!("{name},{cents}\n"))
        .collect()
}
