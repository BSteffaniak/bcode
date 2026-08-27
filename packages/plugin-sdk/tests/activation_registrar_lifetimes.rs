#[test]
fn activation_registrars_cannot_escape_to_threads() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/command_registrar_not_send.rs");
    tests.compile_fail("tests/ui/auth_registrar_not_send.rs");
}
