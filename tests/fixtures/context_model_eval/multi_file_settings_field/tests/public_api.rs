use context_eval_config::{parse, Settings};

#[test]
fn parses_configuration_values() {
    assert_eq!(
        parse("3,false"),
        Some(Settings {
            retry_limit: 3,
            enabled: false,
        })
    );
}

#[test]
fn exposes_max_retries_as_the_public_retry_field() {
    assert_eq!(parse("3,false").unwrap().max_retries, 3);
}
