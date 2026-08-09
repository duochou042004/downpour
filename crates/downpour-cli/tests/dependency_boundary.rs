//! The CLI is an IPC client, not an alternate transfer engine.

fn normal_dependencies(manifest: &str) -> &str {
    let (_, after_header) = manifest
        .split_once("[dependencies]")
        .expect("manifest has dependencies");
    after_header.split("\n[").next().unwrap_or(after_header)
}

#[test]
fn cli_production_dependencies_cannot_reach_transfer_or_storage_crates() {
    let dependencies = normal_dependencies(include_str!("../Cargo.toml"));
    for forbidden in ["downpour-http", "downpour-engine", "downpour-storage"] {
        assert!(
            !dependencies.lines().any(|line| line.starts_with(forbidden)),
            "dp must be IPC-only; forbidden production dependency: {forbidden}"
        );
    }
}
