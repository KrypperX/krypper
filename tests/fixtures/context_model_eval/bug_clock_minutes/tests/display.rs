use context_eval_clock::display;

#[test]
fn keeps_seconds_zero_padded() {
    assert_eq!(display(61), "1:01");
}

#[test]
fn displays_zero_minutes() {
    assert_eq!(display(9), "0:09");
}
