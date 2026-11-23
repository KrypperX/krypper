use context_eval_normalizer::normalize;

#[test]
fn trims_and_lowercases() {
    assert_eq!(normalize("  HELLO  "), "hello");
}

#[test]
fn converts_word_separators() {
    assert_eq!(normalize("Hello_World"), "hello-world");
}
