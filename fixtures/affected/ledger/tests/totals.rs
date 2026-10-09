use ledger_core::vat;

macro_rules! check {
    ($left:expr, $right:expr) => {
        assert_eq!($left, $right)
    };
}

#[test]
fn invoice_includes_vat() {
    check!(100 + vat(100), 120);
}

#[test]
fn zero_has_no_vat() {
    check!(vat(0), 0);
}
