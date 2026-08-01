use checkout_fixture::checkout_total as total;

#[test]
fn total_includes_fee() {
    for number in 0..350 {
        println!("billing-trace step={number:03} customer=demo state=ready");
        if number == 175 {
            println!("billing-trace probe=only-in-the-full-log");
        }
    }
    assert_eq!(total(100, 5), 105);
}
