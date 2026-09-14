fn curated_match(n: u32) -> u32 {
    match n {
        1 => n,
        // comment between arms
        2 => {
            3
        }
        _ => 4,
    }
}
