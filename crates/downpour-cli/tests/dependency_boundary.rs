//! The CLI is an IPC client, not an alternate transfer engine.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn normal_dependencies(manifest: &str) -> &str {
    let (_, after_header) = manifest
        .split_once("[dependencies]")
        .expect("manifest has dependencies");
    after_header.split("\n[").next().unwrap_or(after_header)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CLI crate lives below the workspace root")
        .to_path_buf()
}

fn workspace_edges(package: &str) -> Vec<String> {
    let manifest_path = workspace_root()
        .join("crates")
        .join(package)
        .join("Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("read workspace manifest");
    normal_dependencies(&manifest)
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.trim()))
        .filter(|name| workspace_root().join("crates").join(name).is_dir())
        .map(str::to_owned)
        .collect()
}

fn production_graph_reaches(start: &str, target: &str) -> bool {
    let mut pending = vec![start.to_owned()];
    let mut seen = BTreeSet::new();
    while let Some(package) = pending.pop() {
        if !seen.insert(package.clone()) {
            continue;
        }
        for dependency in workspace_edges(&package) {
            if dependency == target {
                return true;
            }
            pending.push(dependency);
        }
    }
    false
}

#[test]
fn cli_production_dependencies_cannot_reach_transfer_or_storage_crates() {
    for forbidden in ["downpour-http", "downpour-engine", "downpour-storage"] {
        assert!(
            !production_graph_reaches("downpour-cli", forbidden),
            "dp must be IPC-only; its production graph reaches {forbidden}"
        );
    }
}
