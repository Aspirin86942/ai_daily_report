use super::*;

#[test]
fn relative_path_accepts_both_windows_path_representations() {
    for (root, file) in [
        (r"\\?\C:\work", r"C:\work\采购方案.md"),
        (r"C:\work", r"\\?\C:\work\采购方案.md"),
        (r"\\?\UNC\server\work", r"\\server\work\采购方案.md"),
    ] {
        assert_eq!(
            relative_contract_path(Path::new(root), file).unwrap(),
            "采购方案.md"
        );
    }
}

#[test]
fn normalized_paths_still_reject_other_directories_and_parent_traversal() {
    for file in [r"C:\work-other\report.md", r"C:\work\..\report.md"] {
        assert!(relative_contract_path(Path::new(r"\\?\C:\work"), file).is_err());
    }
}
