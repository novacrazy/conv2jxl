pub fn format_bytes(bytes: u64) -> String {
    const SUFFIXES: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut suffix_index = 0;
    while size >= 1024.0 && suffix_index < SUFFIXES.len() - 1 {
        size /= 1024.0;
        suffix_index += 1;
    }
    format!("{:.2} {}", size, SUFFIXES[suffix_index])
}

pub fn format_time(millis: f64) -> String {
    if millis < 1000.0 {
        format!("{:.2}ms", millis)
    } else if millis < 60_000.0 {
        format!("{:.2}s", millis / 1000.0)
    } else if millis < 3_600_000.0 {
        format!("{:.2}min", millis / 60_000.0)
    } else if millis < 86_400_000.0 {
        format!("{:.2}h", millis / 3_600_000.0)
    } else {
        format!("{:.2}d", millis / 86_400_000.0)
    }
}
