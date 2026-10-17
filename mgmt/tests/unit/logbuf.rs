use super::*;

/// One test owns the shared ring (tests run in parallel): order + limits + filters + ANSI, on lines it tags itself.
#[test]
fn ring_keeps_filters_and_strips_colors() {
    let tag = "logbuf-test";
    let (_, before) = query(u64::MAX, 1, "");
    push(&format!(
        "2026-10-17T00:00:00Z  INFO {tag} client booted\n\
         2026-10-17T00:00:01Z  WARN {tag} pool tight — widen it\n"
    ));
    push(&format!(
        "\u{1b}[2m2026-10-17T00:00:02Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m {tag} disk failing\n"
    ));

    let (all, last) = query(before, 1, tag);
    assert_eq!(all.len(), 3);
    assert!(last >= before + 3);
    assert!(all.windows(2).all(|w| w[0].0 < w[1].0), "seq increases");
    assert!(
        !all[2].1.contains('\u{1b}') && all[2].1.contains("ERROR"),
        "colors stripped: {}",
        all[2].1
    );
    assert_eq!(query(before, 2, tag).0.len(), 2, "warn and up");
    assert_eq!(query(before, 3, tag).0.len(), 1, "errors only");
    assert_eq!(
        query(before, 1, "POOL TIGHT").0.len(),
        1,
        "search, any case"
    );
    assert!(query(last, 1, tag).0.is_empty(), "nothing after the newest");

    // The ring never grows past KEEP.
    for i in 0..KEEP + 10 {
        push(&format!("2026-10-17T00:00:03Z  INFO {tag} filler {i}"));
    }
    assert!(RING.lock().lines.len() <= KEEP);
}
