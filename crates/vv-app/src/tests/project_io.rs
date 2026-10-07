use super::*;

#[test]
fn with_upper_case_adds_uppercase_variants() {
    assert_eq!(
        with_upper_case(&["mp4", "mov"]),
        ["mp4", "MP4", "mov", "MOV"]
    );
}
