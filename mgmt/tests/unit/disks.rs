use super::*;

#[test]
fn report_lines_parsed_and_bad_ones_skipped() {
    let d = parse_report(
        "eui.0025385b71b0 512110190592 1 nvme0n1 Samsung SSD 980 PRO 500GB\n\
         WDC_WD10EZEX-08WN4A0_WD-WCC6Y1 1000204886016 0 sda WDC  WD10EZEX\n\
         sdb-1953525168 1000204886016 0 sdb\n\
         bad;id 1 0 sdc x\n\
         short line\n\
         id9 notanumber 0 sdd x\n",
    );
    assert_eq!(d.len(), 3);
    assert_eq!(d[0].id, "eui.0025385b71b0");
    assert!(d[0].broom && !d[1].broom);
    assert_eq!(d[0].model, "Samsung SSD 980 PRO 500GB");
    assert_eq!(d[1].model, "WDC WD10EZEX", "spaces squeezed");
    assert_eq!((d[2].dev.as_str(), d[2].model.as_str()), ("sdb", ""));
    assert_eq!(parse_report(&"a 1 0 sda m\n".repeat(50)).len(), 32);
}

#[test]
fn mac_checked() {
    assert!(mac_ok("00:0c:29:ab:cd:ef"));
    assert!(!mac_ok("00-0c-29-ab-cd-ef") && !mac_ok("../x") && !mac_ok(""));
}
