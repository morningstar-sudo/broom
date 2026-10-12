#[test]
fn prep_win_filled() {
    let s = super::fill_prep("guest", "1");
    assert!(!s.contains("__"), "placeholder left unreplaced");
    assert!(
        super::broom_done().is_ascii(),
        "broom-done is written with -Encoding ascii"
    );
    assert!(super::broom_bootorder().is_ascii());
    assert!(super::broom_games().is_ascii() && super::broom_watch().is_ascii());
    assert!(super::stub().is_ascii());
    // The golden gets the two stubs, not the scripts themselves (those come from BROOMWIN, newest every boot).
    assert!(s.contains("broom\\broom-done.ps1") && s.contains("broom\\broom-bootorder.ps1"));
    assert_eq!(s.matches(super::STUB_MARK).count(), 2);
    assert!(
        !s.contains("Register-ScheduledTask -TaskName BroomBootOrder"),
        "broom-done itself is not baked in"
    );
    // Boot-start disk drivers: set by the prep AFTER sysprep generalized (/quit, not /shutdown), then power off.
    assert!(s.contains("@('storahci', 'stornvme', "));
    let (sp, reg, off) = (
        s.find("'/quit'").unwrap(),
        s.find("reg add $k /v Start").unwrap(),
        s.find("Stop-Computer -Force").unwrap(),
    );
    assert!(sp < reg && reg < off && s.contains("boot-storage.ok"));
}
