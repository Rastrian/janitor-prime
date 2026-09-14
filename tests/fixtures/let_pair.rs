fn curated_let(v: u32) -> u32 {
    let a = v;
    let b = a;
    b;
    let c = 1;
    // comment between lets
    let d = 2;
    d
}
