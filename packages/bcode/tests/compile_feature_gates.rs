#[cfg(not(feature = "testing"))]
#[test]
fn testing_surface_requires_opt_in_feature() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/default_fail/testing_feature_is_opt_in.rs");
}

#[cfg(not(feature = "evaluation"))]
#[test]
fn evaluation_surface_requires_opt_in_feature() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/default_fail/evaluation_feature_is_opt_in.rs");
}
