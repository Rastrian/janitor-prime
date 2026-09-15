fn pipeline(o: std::sync::Mutex<u32>) -> u32 {
    let g = o.lock().unwrap_or(0);
    let h = o.lock().unwrap();
    let _kept = h.clone();
    g + h
}
