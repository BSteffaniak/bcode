#[cfg(not(feature = "testing"))]
#[test]
fn testing_surface_requires_opt_in_feature() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/default_fail/*.rs");
}
