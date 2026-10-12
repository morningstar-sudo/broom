use super::*;

#[test]
fn hostname_rule() {
    for ok in ["PC01", "FPS-43", "a", "ABCDEFGHIJKLMNO"] {
        assert!(hostname_ok(ok), "{ok}");
    }
    for bad in [
        "",
        "ABCDEFGHIJKLMNOP",
        "PC 01",
        "PC_01",
        "-PC",
        "PC-",
        "123",
        "Zoë1",
    ] {
        assert!(!hostname_ok(bad), "{bad}");
    }
}
