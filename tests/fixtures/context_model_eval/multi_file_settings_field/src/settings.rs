#[derive(Debug, Eq, PartialEq)]
pub struct Settings {
    pub retry_limit: u8,
    pub enabled: bool,
}

pub fn parse(input: &str) -> Option<Settings> {
    let (retry_limit, enabled) = input.split_once(',')?;
    Some(Settings {
        retry_limit: retry_limit.parse().ok()?,
        enabled: enabled.parse().ok()?,
    })
}
