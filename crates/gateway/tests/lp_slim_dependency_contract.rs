#![cfg(not(feature = "full-gateway"))]
#![expect(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

#[test]
fn slim_gateway_should_not_compile_full_only_product_dependencies() {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-e",
            "normal",
            "--no-default-features",
            "--features",
            "lp-slim-gateway",
            "--manifest-path",
        ])
        .arg(manifest_path)
        .args(["--prefix", "none"])
        .output()
        .expect("cargo tree should run for the slim gateway contract");

    assert!(
        output.status.success(),
        "cargo tree should succeed for lp-slim-gateway: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    for crate_name in ["durable"] {
        assert!(
            normal_dependency_tree_contains(&tree, crate_name),
            "{crate_name} is the only expected durable crate in lp-slim-gateway, kept for Postgres schema migration compatibility"
        );
    }

    for crate_name in [
        "autopilot-client",
        "autopilot-tools",
        "autopilot-worker",
        "aws-config",
        "clarabel",
        "durable-tools",
        "durable-tools-spawn",
        "evaluations",
        "google-cloud-auth",
        "opentelemetry-otlp",
        "tensorzero-mcp",
        "tensorzero-optimizers",
    ] {
        assert!(
            !normal_dependency_tree_contains(&tree, crate_name),
            "{crate_name} must stay out of the lp-slim-gateway normal dependency graph"
        );
    }
}

fn normal_dependency_tree_contains(tree: &str, crate_name: &str) -> bool {
    tree.lines()
        .filter_map(|line| line.trim_start().split_once(' '))
        .any(|(name, _version)| name == crate_name)
}
