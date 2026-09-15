fn audit(v: Option<u32>) {
    match v {
        None => panic!("nothing to audit"),
        Some(0) => panic!(),
        Some(n) => {
            assert!(n > 0, "positive");
            let msg = format!("{} sites", n);
            let _ = msg;
        }
    }
}
