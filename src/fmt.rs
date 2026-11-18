//! Small human-readable formatters.

pub fn bytes(n: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let v = n as f64;
    if v >= GIB {
        format!("{:.1} GiB", v / GIB)
    } else if v >= MIB {
        format!("{:.0} MiB", v / MIB)
    } else {
        format!("{:.0} KiB", v / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_units() {
        assert_eq!(bytes(0), "0 KiB");
        assert_eq!(bytes(2048), "2 KiB");
        assert_eq!(bytes(5 * 1024 * 1024), "5 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GiB");
    }
}
