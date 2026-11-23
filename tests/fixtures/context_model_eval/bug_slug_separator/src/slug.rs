pub fn normalize(input: &str) -> String {
    input.trim().to_ascii_lowercase().replace('_', "_")
}
