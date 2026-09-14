fn curated_if(x: bool) -> u32 {
    let a = if x {
        1
    } else {
        2
    };
    let b = if !x { 3 } /* why */ else { 4 };
    a + b
}
