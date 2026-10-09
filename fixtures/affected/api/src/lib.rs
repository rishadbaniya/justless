use ledger_core::money::Money;

/// The invoice total for a net amount, formatted for display.
pub fn invoice_total(net: u64) -> String {
    Money(net).with_vat().to_string()
}

pub fn greeting(name: &str) -> String {
    format!("Invoice for {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invoice_total_adds_vat() {
        assert_eq!(invoice_total(1000), "12.00");
    }

    #[test]
    fn greeting_names_the_customer() {
        assert_eq!(greeting("Ada"), "Invoice for Ada");
    }
}
