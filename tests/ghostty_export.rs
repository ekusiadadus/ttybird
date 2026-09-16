//! Manual native integration entry point. The smoke driver creates every target.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a synthetic Ghostty UUID supplied by ghostty_export_smoke.py"]
fn synthetic_native_snapshot() {
    let id = std::env::var("TTYBIRD_TEST_GHOSTTY_UUID").expect("synthetic fixture UUID required");
    let expected = std::env::var("TTYBIRD_TEST_GHOSTTY_MARKER").expect("synthetic marker required");
    assert!(expected.starts_with("AX_"));
    let result = ttybird::ghostty_export::capture(&id);
    if std::env::var_os("TTYBIRD_TEST_GHOSTTY_CLOSED").is_some() {
        let error = result.expect_err("a closed UUID must be rejected");
        assert_eq!(
            error.to_string(),
            "The selected Ghostty terminal no longer exists"
        );
        return;
    }
    let bytes = result.expect("synthetic native export failed");
    assert!(
        bytes
            .windows(expected.len())
            .any(|w| w == expected.as_bytes()),
        "exact fixture marker missing"
    );
    for cols in [48, 93, 140] {
        let text = ttybird::preview::parse_export(&bytes, cols).expect("VT parse failed");
        assert!(
            text.lines
                .iter()
                .all(|line| line.width() <= usize::from(cols))
        );
        let rendered = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains(&expected),
            "fixture missing from rendered output"
        );
        assert!(
            rendered.contains("日本語"),
            "Unicode did not survive export and VT parsing"
        );
    }
    println!(
        "native_export_and_libghostty_render_verified=true bytes={}",
        bytes.len()
    );
}
