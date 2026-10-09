/// Value-added tax in cents, rounded down.
///
/// ```
/// assert_eq!(ledger_core::tax::vat(1000), 200);
/// ```
pub fn vat(cents: u64) -> u64 {
    cents * RATE / 100
}

/// The amount before tax for a gross amount.
pub fn net(gross: u64) -> u64 {
    gross * 100 / (100 + RATE)
}

const RATE: u64 = 20;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vat_is_a_fifth() {
        assert_eq!(vat(500), 100);
    }

    #[test]
    fn vat_rounds_down() {
        assert_eq!(vat(7), 1);
    }

    #[test]
    fn net_removes_tax() {
        assert_eq!(net(120), 100);
    }
}
