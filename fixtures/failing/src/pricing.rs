pub fn checkout_total(subtotal: u32, fee: u32) -> u32 {
    apply_fee(subtotal, fee)
}

fn apply_fee(subtotal: u32, fee: u32) -> u32 {
    subtotal + fee + 1
}
