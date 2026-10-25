//! Behavioral coverage for the public processor factory. These tests never
//! require a recognizer, desktop session, model, microphone or network access.

use super::processor;
use crate::config::Config;
use crate::contracts::Error;

fn configured(replacements: &[(&str, &str)], punctuation: bool) -> Config {
    Config {
        replacements: replacements
            .iter()
            .map(|&(a, b)| (a.into(), b.into()))
            .collect(),
        spoken_punctuation: punctuation,
        ..Config::default()
    }
}

fn transform(config: &Config, text: &str) -> String {
    processor(config)
        .expect("valid test config")
        .process(text)
        .expect("text processing")
}

#[test]
fn longest_phrase_wins_at_each_source_position() {
    let config = configured(
        &[("new", "N"), ("new york", "NY"), ("new york city", "NYC")],
        false,
    );
    assert_eq!(
        transform(&config, "new york city, new york; new"),
        "NYC, NY; N"
    );
}

#[test]
fn matching_is_left_to_right_not_global_longest_first() {
    let config = configured(&[("a b", "X"), ("b c d", "Y")], false);
    assert_eq!(transform(&config, "a b c d"), "X c d");
}

#[test]
fn replacements_are_nonrecursive_and_support_cycles() {
    let config = configured(
        &[("alpha", "beta"), ("beta", "alpha"), ("same", "same")],
        false,
    );
    assert_eq!(transform(&config, "alpha beta same"), "beta alpha same");
}

#[test]
fn replacement_output_is_not_a_spoken_command() {
    let config = configured(&[("word", "comma")], true);
    assert_eq!(transform(&config, "word comma"), "comma,");
}

#[test]
fn custom_phrase_wins_equal_length_and_longer_matches() {
    let config = configured(&[("comma", "COMMA"), ("new line please", "break")], true);
    assert_eq!(transform(&config, "comma new line please"), "COMMA break");
}

#[test]
fn custom_matching_is_case_sensitive_and_whitespace_exact() {
    let config = configured(&[("vox", "Vox"), ("two words", "phrase")], false);
    assert_eq!(
        transform(&config, "vox VOX two  words two words"),
        "Vox VOX two  words phrase"
    );
}

#[test]
fn matches_phrases_not_substrings_or_contraction_fragments() {
    let config = configured(&[("cat", "dog"), ("can", "may")], false);
    assert_eq!(
        transform(&config, "cat scatter cats (cat) can't"),
        "dog scatter cats (dog) can't"
    );
}

#[test]
fn unicode_letters_and_combining_marks_are_not_ascii_boundaries() {
    let config = configured(&[("cafe", "X"), ("café", "Y"), ("cafe\u{301}", "Z")], false);
    assert_eq!(
        transform(&config, "écafe cafeé cafe\u{301} café cafe"),
        "écafe cafeé Z Y X"
    );
}

#[test]
fn canonical_forms_are_preserved_not_silently_normalized() {
    let config = configured(&[("café", "coffee")], false);
    assert_eq!(transform(&config, "café cafe\u{301}"), "coffee cafe\u{301}");
}

#[test]
fn emoji_sequences_are_not_split_by_replacements() {
    let config = configured(&[("👩", "person"), ("👍", "yes"), ("👩‍💻", "coder")], false);
    assert_eq!(transform(&config, "👩‍💻 👍🏽 👩 👍"), "coder 👍🏽 person yes");
}

#[test]
fn non_latin_phrases_and_surrounding_punctuation_work() {
    let config = configured(&[("你好", "您好"), ("привет", "здравствуйте")], false);
    assert_eq!(
        transform(&config, "你好！ привет, приветствие"),
        "您好！ здравствуйте, приветствие"
    );
}

#[test]
fn punctuation_can_be_disabled_without_enabling_literal_escape() {
    let config = configured(&[], false);
    assert_eq!(
        transform(&config, "Hello comma literal period"),
        "Hello comma literal period"
    );
}

#[test]
fn punctuation_grammar_is_case_insensitive_and_longest_first() {
    let config = configured(&[], true);
    assert_eq!(
        transform(
            &config,
            "Hello COMMA world FULL STOP Why QUESTION MARK Yes EXCLAMATION POINT"
        ),
        "Hello, world. Why? Yes!"
    );
    assert_eq!(
        transform(&config, "a colon b semi colon c semicolon d ellipsis"),
        "a: b; c; d…"
    );
    assert_eq!(transform(&config, "a exclamation mark b period"), "a! b.");
}

#[test]
fn spoken_commands_do_not_match_inside_words() {
    assert_eq!(
        transform(&Config::default(), "commander periods colons"),
        "commander periods colons"
    );
}

#[test]
fn literal_escape_protects_exactly_one_spoken_command() {
    let config = configured(&[("comma", "renamed")], true);
    assert_eq!(
        transform(&config, "literal comma comma literal QUESTION MARK"),
        "comma renamed QUESTION MARK"
    );
    assert_eq!(
        transform(&config, "literal new paragraph period"),
        "new paragraph."
    );
    assert_eq!(transform(&config, "literal    colon"), "colon");
    assert_eq!(
        transform(&config, "literal zebra literal"),
        "literal zebra literal"
    );
}

#[test]
fn literal_escape_does_not_cross_a_line_or_match_word_suffixes() {
    assert_eq!(
        transform(&Config::default(), "nonliteral comma literal\ncomma"),
        "nonliteral, literal\n,"
    );
}

#[test]
fn suppresses_adjacent_duplicate_spoken_and_asr_punctuation() {
    let config = Config::default();
    for (input, expected) in [
        ("hello comma, world", "hello, world"),
        ("hello comma , world", "hello, world"),
        ("hello, comma world", "hello, world"),
        ("hello, comma, world", "hello, world"),
        ("hello comma, comma, world", "hello, world"),
        ("hello comma comma world", "hello, world"),
        ("hello period.", "hello."),
        ("hello. period", "hello."),
        ("really question mark.", "really."),
        ("really! period", "really!"),
        ("hello comma period", "hello,."),
    ] {
        assert_eq!(transform(&config, input), expected);
    }
}

#[test]
fn preserves_literal_existing_punctuation_without_a_spoken_command() {
    assert_eq!(
        transform(&Config::default(), "Really?! ... yes!!"),
        "Really?! ... yes!!"
    );
}

#[test]
fn newlines_belong_to_delivery_policy_not_text_policy() {
    let mut config = Config::default();
    let input = "one new line two new paragraph three";
    assert_eq!(transform(&config, input), "one\ntwo\n\nthree");
    config.multiline = true;
    assert_eq!(transform(&config, input), "one\ntwo\n\nthree");
    assert_eq!(transform(&config, "one new line\ntwo"), "one\ntwo");
    assert_eq!(transform(&config, "one new paragraph\n\ntwo"), "one\n\ntwo");
}

#[test]
fn sanitizes_input_and_replacement_controls_without_merging_words() {
    let config = configured(&[("secret", "x\u{1b}\u{0}y\r\nz\tq\u{202e}")], false);
    assert_eq!(
        transform(&config, "a\u{0}b\u{1b}c\u{7f}d\u{85}e"),
        "a b c d e"
    );
    assert_eq!(transform(&config, "secret"), "x  y\nz q ");
    assert_eq!(
        transform(&config, "a\r\nb\rc\u{2028}d\u{2029}e\tf"),
        "a\nb\nc\nd\ne f"
    );
}

#[test]
fn removes_invisible_direction_overrides_but_keeps_language_and_emoji_joiners() {
    let config = configured(&[], false);
    assert_eq!(
        transform(&config, "a\u{200b}b\u{2066}c\u{2069}d\u{feff}"),
        "a b c d "
    );
    let text = "می\u{200c}روم 👨‍👩‍👧‍👦 e\u{301} ❤️ 中文";
    assert_eq!(transform(&config, text), text);
}

#[test]
fn empty_replacements_and_metacharacters_are_literal() {
    let config = configured(&[("remove", ""), ("token", "$1 \\ path; $(false)")], false);
    assert_eq!(transform(&config, "remove token"), " $1 \\ path; $(false)");
}

#[test]
fn rejects_invalid_keys_without_echoing_configured_text() {
    for phrase in [
        "",
        " ",
        " secret",
        "secret ",
        "secret\u{0}",
        "secret\rvalue",
        "secret\u{202e}",
    ] {
        let Err(error) = processor(&configured(&[(phrase, "value")], false)) else {
            panic!("invalid phrase accepted");
        };
        assert!(matches!(error, Error::Invalid(_)));
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn processor_owns_a_config_snapshot_and_has_no_per_call_state() {
    let mut config = configured(&[("a", "b")], false);
    let processor = processor(&config).expect("config");
    config.replacements.insert("a".into(), "changed".into());
    for _ in 0..3 {
        assert_eq!(processor.process("a").expect("processing"), "b");
    }
}

#[test]
fn preserves_empty_whitespace_and_long_transcripts() {
    let config = configured(&[], false);
    for text in ["", "  ", "\n\n", " - leading dash "] {
        assert_eq!(transform(&config, text), text);
    }
    let text = "中文 👩‍💻 cafe\u{301} plain ".repeat(10_000);
    assert_eq!(transform(&config, &text), text);
}
