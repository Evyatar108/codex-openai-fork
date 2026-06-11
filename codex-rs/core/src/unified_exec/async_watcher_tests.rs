use super::build_background_completion_output;
use super::split_valid_utf8_prefix_with_max;

use pretty_assertions::assert_eq;

#[test]
fn split_valid_utf8_prefix_respects_max_bytes_for_ascii() {
    let mut buf = b"hello word!".to_vec();

    let first =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 5).expect("expected prefix");
    assert_eq!(first, b"hello".to_vec());
    assert_eq!(buf, b" word!".to_vec());

    let second =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 5).expect("expected prefix");
    assert_eq!(second, b" word".to_vec());
    assert_eq!(buf, b"!".to_vec());
}

#[test]
fn split_valid_utf8_prefix_avoids_splitting_utf8_codepoints() {
    // "é" is 2 bytes in UTF-8. With a max of 3 bytes, we should only emit 1 char (2 bytes).
    let mut buf = "ééé".as_bytes().to_vec();

    let first =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 3).expect("expected prefix");
    assert_eq!(std::str::from_utf8(&first).unwrap(), "é");
    assert_eq!(buf, "éé".as_bytes().to_vec());
}

#[test]
fn split_valid_utf8_prefix_makes_progress_on_invalid_utf8() {
    let mut buf = vec![0xff, b'a', b'b'];

    let first =
        split_valid_utf8_prefix_with_max(&mut buf, /*max_bytes*/ 2).expect("expected prefix");
    assert_eq!(first, vec![0xff]);
    assert_eq!(buf, b"ab".to_vec());
}

#[test]
fn background_completion_output_passes_through_when_under_cap() {
    let out = build_background_completion_output(b"short output", /*buffer_omitted*/ 0, 1024);
    assert_eq!(out, "short output");
}

#[test]
fn background_completion_output_notes_upstream_omission_when_under_cap() {
    let out = build_background_completion_output(b"tail bytes", /*buffer_omitted*/ 4096, 1024);
    assert_eq!(
        out,
        "tail bytes\n[... output truncated, 4096 bytes omitted from the middle ...]"
    );
}

#[test]
fn background_completion_output_keeps_head_and_tail_when_over_cap() {
    let retained = "H".repeat(50).into_bytes();
    let out = build_background_completion_output(&retained, /*buffer_omitted*/ 0, 10);

    let head = "H".repeat(5);
    let tail = "H".repeat(5);
    let omitted = 50 - 10;
    assert_eq!(
        out,
        format!(
            "{head}\n[... output truncated, {omitted} bytes omitted from the middle ...]\n{tail}"
        )
    );
}

#[test]
fn background_completion_output_adds_upstream_and_window_omissions() {
    let retained = "H".repeat(50).into_bytes();
    let out = build_background_completion_output(&retained, /*buffer_omitted*/ 100, 10);
    // 100 dropped upstream by the 1 MiB cap + (50 - 10) dropped by the head/tail window.
    assert!(
        out.contains("[... output truncated, 140 bytes omitted from the middle ...]"),
        "got: {out}"
    );
}

#[test]
fn background_completion_output_respects_utf8_boundaries() {
    // "é" is 2 bytes; ensure the head/tail windows never split a codepoint.
    let retained = "é".repeat(40).into_bytes();
    let out = build_background_completion_output(&retained, /*buffer_omitted*/ 0, 9);
    // Output must remain valid UTF-8 (build returns String, so this is implicit) and the
    // visible head/tail must only contain whole "é" chars.
    let head = out.split('\n').next().expect("head segment");
    assert!(
        head.chars().all(|c| c == 'é'),
        "head not whole chars: {head:?}"
    );
}
