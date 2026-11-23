use context_eval_records::{parse, Item};

#[test]
fn parses_public_values() {
    assert_eq!(
        parse("tools: wrench"),
        Some(Item {
            group: "tools".to_owned(),
            value: "wrench".to_owned(),
        })
    );
}

#[test]
fn exposes_category_as_the_public_grouping_field() {
    assert_eq!(parse("tools: wrench").unwrap().category, "tools");
}
