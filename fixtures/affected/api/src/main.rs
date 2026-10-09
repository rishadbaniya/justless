fn main() {
    let net = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    println!("{}", api::invoice_total(net));
}
