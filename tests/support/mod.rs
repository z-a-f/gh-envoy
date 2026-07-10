use std::path::Path;

pub fn assert_same_existing_path(actual: impl AsRef<Path>, expected: impl AsRef<Path>) {
    let actual = canonicalize_for_comparison(actual.as_ref(), "actual");
    let expected = canonicalize_for_comparison(expected.as_ref(), "expected");
    assert_eq!(actual, expected, "paths do not identify the same location");
}

fn canonicalize_for_comparison(path: &Path, label: &str) -> std::path::PathBuf {
    path.canonicalize()
        .unwrap_or_else(|error| panic!("failed to canonicalize {label} path {path:?}: {error}"))
}
