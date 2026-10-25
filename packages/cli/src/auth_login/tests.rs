use super::*;
use std::io::Cursor;

fn prompter(input: &str, interactive: bool) -> Prompter<Cursor<Vec<u8>>, Vec<u8>> {
    Prompter {
        reader: Cursor::new(input.as_bytes().to_vec()),
        writer: Vec::new(),
        interactive,
        rich: false,
        back_requested: false,
    }
}

fn provider() -> RegisteredAuthProvider {
    RegisteredAuthProvider {
        plugin_id: "bcode.test".to_owned(),
        contribution: bcode_provider_auth_models::AuthProviderContribution {
            schema_version: 1,
            provider_id: "test".to_owned(),
            display_name: "Test Provider".to_owned(),
            methods: ["browser", "device"]
                .into_iter()
                .map(|id| AuthMethodContribution::Interactive {
                    method_id: id.to_owned(),
                    display_name: format!("Use {id}"),
                    operation: "flow".to_owned(),
                    credentials: Vec::new(),
                    supports_revocation: false,
                })
                .collect(),
        },
    }
}

#[test]
fn registered_provider_and_method_choices_are_interactive() {
    let provider = provider();
    let mut prompt = prompter("1\n2\n", true);
    assert_eq!(prompt.provider(&[&provider]).unwrap(), "test");
    assert_eq!(
        prompt.method(&provider, None).unwrap().method_id(),
        "device"
    );
    let output = String::from_utf8(prompt.writer).unwrap();
    assert!(output.contains("Test Provider"));
    assert!(output.contains("Use device"));
}

#[test]
fn explicit_method_never_consumes_input_or_falls_back() {
    let provider = provider();
    let mut prompt = prompter("q\n", true);
    assert_eq!(
        prompt
            .method(&provider, Some("browser"))
            .unwrap()
            .method_id(),
        "browser"
    );
    assert!(prompt.method(&provider, Some("unknown")).is_err());
    assert_eq!(prompt.reader.position(), 0);
    assert!(prompt.writer.is_empty());
}

#[test]
fn noninteractive_choices_fail_without_consuming_input() {
    let provider = provider();
    let mut prompt = prompter("1\n", false);
    assert!(
        prompt
            .provider(&[&provider])
            .unwrap_err()
            .to_string()
            .contains("<provider>")
    );
    assert!(
        prompt
            .method(&provider, None)
            .unwrap_err()
            .to_string()
            .contains("--method")
    );
    assert_eq!(prompt.reader.position(), 0);
    assert!(prompt.writer.is_empty());
}

#[test]
fn invalid_choices_retry_and_cancellation_and_eof_abort() {
    let mut prompt = prompter("0\n999\nbad\n2\nsecret\n", true);
    assert_eq!(
        prompt
            .choose("Choice", &["one".to_owned(), "two".to_owned()], "hint")
            .unwrap(),
        1
    );
    assert_eq!(read_auth_input_line(&mut prompt.reader).unwrap(), "secret");
    for input in ["q\n", ""] {
        assert!(
            prompter(input, true)
                .choose("Choice", &["one".to_owned()], "hint")
                .is_err()
        );
    }
}

#[test]
fn output_failure_does_not_consume_input() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("broken"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut prompt = Prompter {
        reader: Cursor::new(b"1\n"),
        writer: Broken,
        interactive: true,
        rich: false,
        back_requested: false,
    };
    assert!(
        prompt
            .choose("Choice", &["one".to_owned()], "hint")
            .is_err()
    );
    assert_eq!(prompt.reader.position(), 0);
}

#[test]
fn account_and_pool_choices_use_validated_enrollment_plans() {
    let provider = provider();
    let config = BcodeConfig::default();
    let runtime = RuntimeAuthSubscriptions::default();
    let capability = AuthPoolCompatibility {
        schema_version: 1,
        provider_id: "test".to_owned(),
        group: "subscription".to_owned(),
        methods: vec!["browser".to_owned(), "device".to_owned()],
    };
    let planning = Planning {
        config: &config,
        runtime: &runtime,
        provider: &provider,
        method: &provider.contribution.methods[0],
        compatibility: Some(&capability),
    };
    let mut prompt = prompter("1\n1\n", true);
    let profile = prompt.profile(&planning, None, None).unwrap();
    assert_eq!(profile.as_deref(), Some("test"));
    let plan = prompt
        .plan(
            &planning,
            EnrollmentDestination {
                profile,
                ..EnrollmentDestination::default()
            },
            PoolEnrollment::Automatic,
        )
        .unwrap();
    assert_eq!(plan.prepared.resolved.profile_name, "test");
    assert!(
        String::from_utf8(prompt.writer)
            .unwrap()
            .contains("preserve existing pool memberships")
    );
}

#[test]
fn conflicting_pool_is_not_selectable_and_explicit_pool_never_falls_back() {
    let provider = provider();
    let mut config = BcodeConfig::default();
    config.auth.pools.insert(
        "foreign".to_owned(),
        bcode_config::AuthPoolConfig {
            provider_plugin_id: Some("bcode.foreign".to_owned()),
            ..Default::default()
        },
    );
    let runtime = RuntimeAuthSubscriptions::default();
    let capability = AuthPoolCompatibility {
        schema_version: 1,
        provider_id: "test".to_owned(),
        group: "subscription".to_owned(),
        methods: vec!["browser".to_owned()],
    };
    let planning = Planning {
        config: &config,
        runtime: &runtime,
        provider: &provider,
        method: &provider.contribution.methods[0],
        compatibility: Some(&capability),
    };
    let destination = EnrollmentDestination {
        profile: Some("test".to_owned()),
        ..Default::default()
    };
    let mut prompt = prompter("1\n", true);
    assert!(
        prompt
            .plan(&planning, destination.clone(), PoolEnrollment::Automatic)
            .is_ok()
    );
    let output = String::from_utf8(prompt.writer).unwrap();
    assert!(output.contains("Pool foreign is unavailable"));
    assert!(!output.contains("Join pool foreign"));
    let mut explicit = prompter("1\n", true);
    assert!(
        explicit
            .plan(
                &planning,
                destination,
                PoolEnrollment::Explicit("foreign".to_owned())
            )
            .is_err()
    );
    assert_eq!(explicit.reader.position(), 0);
}

#[test]
fn bare_login_accepts_explicit_choices_without_provider_argument() {
    use clap::Parser as _;
    assert!(
        super::super::Cli::try_parse_from([
            "bcode",
            "auth",
            "login",
            "--method",
            "device",
            "--standalone"
        ])
        .is_ok()
    );
}
