pub fn day_name(index: usize) -> &'static str {
    ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        .get(index)
        .copied()
        .unwrap_or("unknown")
}
