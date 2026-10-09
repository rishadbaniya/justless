#[test]
fn extra_feature_is_enabled() {
    assert!(cfg!(feature = "extra"));
}
