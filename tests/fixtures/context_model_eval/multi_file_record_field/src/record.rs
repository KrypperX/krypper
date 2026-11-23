#[derive(Debug, Eq, PartialEq)]
pub struct Item {
    pub group: String,
    pub value: String,
}

pub fn parse(input: &str) -> Option<Item> {
    let (group, value) = input.split_once(':')?;
    Some(Item {
        group: group.trim().to_owned(),
        value: value.trim().to_owned(),
    })
}
