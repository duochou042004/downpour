//! The protocol backend owns network behavior, not interval allocation or durable storage.

fn normal_dependencies(manifest: &str) -> &str {
    let (_, after_header) = manifest
        .split_once("[dependencies]")
        .expect("manifest has dependencies");
    after_header.split("\n[").next().unwrap_or(after_header)
}

#[test]
fn http_production_dependencies_cannot_reach_allocator_or_storage_crates() {
    let dependencies = normal_dependencies(include_str!("../Cargo.toml"));
    for forbidden in ["downpour-intervals", "downpour-storage"] {
        assert!(
            !dependencies.lines().any(|line| line.starts_with(forbidden)),
            "protocol crate must not own orchestration or durability: {forbidden}"
        );
    }
}
