use super::*;

#[test]
fn default_delivery_flattens_all_newlines_without_command_keys() {
    assert_eq!(
        prepare("é\r\n中\r😀\ne\u{301}\u{2028}tail\u{2029}", false, true).expect("plain text"),
        "é 中 😀 e\u{301} tail "
    );
    assert_eq!(
        prepare("a\nb", true, false).expect("explicit multiline"),
        "a\nb"
    );
    assert!(matches!(
        prepare("a\nb", true, true),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn control_characters_are_rejected_for_every_output() {
    for character in ['\0', '\t', '\u{1b}', '\u{7f}', '\u{85}', '\u{9b}'] {
        let input = format!("prefix{character}suffix");
        for multiline in [false, true] {
            for typing in [false, true] {
                assert!(matches!(
                    prepare(&input, multiline, typing),
                    Err(Error::Invalid(_))
                ));
            }
        }
    }
}

#[test]
fn unicode_and_syntax_are_literal_not_commands() {
    let input = "é中😀e\u{301}👩\u{200d}💻 $() `command` ; [Enter] {Shift} \\\"";
    assert_eq!(prepare(input, false, true).expect("literal Unicode"), input);
    assert_eq!(prepare("", false, true).expect("empty text"), "");
    assert!(prepare(&"x".repeat(MAX_TEXT_BYTES + 1), false, false).is_err());
}

#[test]
fn automatic_selection_never_downgrades_a_wayland_session() {
    assert_eq!(
        select_backend("auto", true, true).expect("Wayland"),
        Backend::Wayland
    );
    assert_eq!(
        select_backend("auto", false, true).expect("X11"),
        Backend::X11
    );
    assert_eq!(
        select_backend("x11", true, false).expect("explicit X11"),
        Backend::X11
    );
    assert_eq!(
        select_backend("wayland", false, true).expect("explicit Wayland"),
        Backend::Wayland
    );
    assert!(select_backend("auto", false, false).is_err());
    assert!(matches!(
        select_backend("unknown", true, true),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn standalone_host_delivery_is_never_fake_success() {
    let config = Config {
        delivery: DeliveryMode::Host,
        ..Config::default()
    };
    assert!(matches!(sink(&config), Err(Error::Unsupported(_))));
}

#[test]
fn stdout_selection_does_not_require_a_desktop_or_valid_typing_backend() {
    let config = Config {
        delivery: DeliveryMode::Stdout,
        delivery_backend: "not-a-backend".into(),
        ..Config::default()
    };
    assert!(sink(&config).is_ok());
}

#[test]
fn stdout_writes_exact_utf8_without_an_added_newline() {
    let mut output = Vec::new();
    write_output(&mut output, "é中😀".as_bytes(), &Cancellation::new()).expect("memory writer");
    assert_eq!(output, "é中😀".as_bytes());
}

#[test]
fn precancelled_output_does_not_write_or_flush() {
    let cancel = Cancellation::new();
    cancel.cancel();
    let mut output = FailAfter {
        written: Vec::new(),
        flushes: 0,
        calls: 0,
    };
    assert!(matches!(
        write_output(&mut output, b"private", &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(output.calls, 0);
    assert_eq!(output.flushes, 0);
}

struct FailAfter {
    written: Vec<u8>,
    flushes: usize,
    calls: usize,
}
impl Write for FailAfter {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        self.calls += 1;
        if self.calls == 1 {
            let count = input.len().min(2);
            self.written.extend_from_slice(&input[..count]);
            Ok(count)
        } else {
            Err(std::io::Error::other("sensitive backend diagnostic"))
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[test]
fn partial_write_is_not_replayed_or_leaked_in_the_error() {
    let mut output = FailAfter {
        written: Vec::new(),
        flushes: 0,
        calls: 0,
    };
    let error = write_output(&mut output, b"secret transcript", &Cancellation::new())
        .expect_err("injected failure");
    assert_eq!(output.written, b"se");
    assert_eq!(output.calls, 2);
    assert_eq!(output.flushes, 0);
    assert!(!error.to_string().contains("sensitive backend diagnostic"));
    assert!(!error.to_string().contains("secret transcript"));
}

struct Keys {
    events: Vec<(u32, bool)>,
    cancel_on_press: Option<Cancellation>,
    fail_press: bool,
    fail_release: bool,
}
impl KeyEvents for Keys {
    fn press(&mut self, key: u32) -> Result<()> {
        self.events.push((key, true));
        if let Some(cancel) = &self.cancel_on_press {
            cancel.cancel();
        }
        if self.fail_press {
            Err(Error::Backend("press failed".into()))
        } else {
            Ok(())
        }
    }
    fn release(&mut self, key: u32) -> Result<()> {
        self.events.push((key, false));
        if self.fail_release {
            Err(Error::Backend("release failed".into()))
        } else {
            Ok(())
        }
    }
}

#[test]
fn cancellation_and_failed_presses_still_attempt_one_release() {
    for fail_press in [false, true] {
        for fail_release in [false, true] {
            let cancel = Cancellation::new();
            let mut keys = Keys {
                events: Vec::new(),
                cancel_on_press: Some(cancel.clone()),
                fail_press,
                fail_release,
            };
            assert!(key_pair(&mut keys, 42, &cancel).is_err());
            assert_eq!(keys.events, [(42, true), (42, false)]);
        }
    }
}

#[test]
fn cancellation_before_press_has_no_key_events() {
    let cancel = Cancellation::new();
    cancel.cancel();
    let mut keys = Keys {
        events: Vec::new(),
        cancel_on_press: None,
        fail_press: false,
        fail_release: false,
    };
    assert!(matches!(
        key_pair(&mut keys, 42, &cancel),
        Err(Error::Cancelled)
    ));
    assert!(keys.events.is_empty());
}

#[test]
fn cancellation_stops_subsequent_text_without_repeating_the_prefix() {
    let cancel = Cancellation::new();
    let mut keys = Keys {
        events: Vec::new(),
        cancel_on_press: Some(cancel.clone()),
        fail_press: false,
        fail_release: false,
    };
    for key in [30, 31, 32] {
        if key_pair(&mut keys, key, &cancel).is_err() {
            break;
        }
    }
    assert_eq!(keys.events, [(30, true), (30, false)]);
}

#[test]
fn precancelled_desktop_delivery_never_connects() {
    let cancel = Cancellation::new();
    cancel.cancel();
    for backend in [Backend::Wayland, Backend::X11] {
        if let Ok(mut sink) = typing_sink(backend, false) {
            assert!(matches!(
                sink.deliver("literal text", &cancel),
                Err(Error::Cancelled)
            ));
        }
    }
}

#[test]
fn malformed_typing_is_rejected_before_desktop_discovery() {
    for backend in [Backend::Wayland, Backend::X11] {
        if let Ok(mut sink) = typing_sink(backend, false) {
            assert!(matches!(
                sink.deliver("text\u{1b}[31m", &Cancellation::new()),
                Err(Error::Invalid(_))
            ));
        }
    }
}

#[cfg(not(feature = "clipboard"))]
#[test]
fn absent_clipboard_feature_is_an_error_not_another_sink() {
    let config = Config {
        delivery: DeliveryMode::Clipboard,
        ..Config::default()
    };
    assert!(matches!(sink(&config), Err(Error::Unsupported(_))));
}
