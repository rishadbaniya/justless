#[test]
fn rates_file_lists_the_standard_rate() {
    let rates = include_str!("../fixtures/rates.json");
    assert!(rates.contains("\"standard\": 20"));
}
