const README: &str = include_str!("../README.md");
const SPEC: &str = include_str!("../spec.md");

#[test]
fn readme_links_the_normative_spec_with_shipped_behavior() {
    assert!(README.contains("[product specification](spec.md)"));
    for behavior in [
        "**`gh envoy list`**",
        "`--resume`",
        "nested shell",
        "closed issues",
        "operation journal",
        "**`gh envoy status [--strict]`**",
    ] {
        assert!(
            SPEC.contains(behavior),
            "normative spec is missing shipped behavior {behavior:?}"
        );
    }
}
