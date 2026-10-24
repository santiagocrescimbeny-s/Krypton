//! The manifest and the lockfile must agree (audit finding #48).
//!
//! The `grpc` feature is shipped empty so that the default build and the three
//! `--locked` gates need nothing from crates.io. These tests keep it that way, and
//! keep the way to *enable* it a three-line edit: they pass both while the gRPC
//! dependencies are absent (today) and after somebody has enabled them correctly,
//! and fail for every half-done state in between (which would break `--locked`).

const MANIFEST: &str = include_str!("../Cargo.toml");
const LOCK: &str = include_str!("../Cargo.lock");
const CI: &str = include_str!("../.github/workflows/ci.yml");

/// Crates that exist only for the gRPC link. `tokio-stream` is left out: other
/// dependencies already bring it into `Cargo.lock`.
const GRPC_ONLY_CRATES: [&str; 2] = ["yellowstone-grpc-proto", "tonic"];
/// Everything the link needs, as named in the manifest.
const GRPC_CRATES: [&str; 3] = ["yellowstone-grpc-proto", "tonic", "tokio-stream"];

/// The lines of `[header]`, up to the next table, comments excluded.
fn active_lines(header: &str) -> Vec<&str> {
    let marker = format!("[{header}]");
    let mut inside = false;
    let mut lines = Vec::new();
    for line in MANIFEST.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == marker;
            continue;
        }
        if inside && !trimmed.is_empty() && !trimmed.starts_with('#') {
            lines.push(trimmed);
        }
    }
    lines
}

fn names_crate(line: &str, name: &str) -> bool {
    line.strip_prefix(name)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

fn used_by_the_package() -> bool {
    active_lines("dependencies")
        .iter()
        .any(|line| GRPC_ONLY_CRATES.iter().any(|name| names_crate(line, name)))
}

fn in_the_lockfile() -> bool {
    GRPC_ONLY_CRATES
        .iter()
        .any(|name| LOCK.contains(&format!("name = \"{name}\"")))
}

fn grpc_feature() -> Option<&'static str> {
    active_lines("features")
        .into_iter()
        .find(|line| names_crate(line, "grpc"))
}

#[test]
fn the_grpc_feature_is_either_empty_or_wired_with_dep_syntax() {
    let feature = grpc_feature();
    assert!(feature.is_some(), "the `grpc` feature must exist");
    let line = feature.unwrap_or_default();
    let empty = line.replace(' ', "") == "grpc=[]";
    if empty {
        return;
    }
    for name in GRPC_CRATES {
        assert!(
            line.contains(&format!("\"dep:{name}\"")),
            "`grpc` must enable `dep:{name}`: {line}"
        );
    }
}

#[test]
fn the_package_dependencies_and_the_lockfile_never_disagree() {
    // Declaring a dependency in the package needs its lock entry (else `--locked`
    // fails), and a lock entry nobody uses means the lockfile is stale.
    assert_eq!(
        used_by_the_package(),
        in_the_lockfile(),
        "Cargo.toml and Cargo.lock disagree about the gRPC dependencies: run \
         `cargo update -w` on a machine that reaches crates.io, or undo the edit"
    );
}

#[test]
fn an_enabled_link_declares_every_dependency_optional_and_the_feature_matches() {
    if !used_by_the_package() {
        // Disabled: nothing may reference them, and the feature must be empty.
        let line = grpc_feature().unwrap_or_default().replace(' ', "");
        assert_eq!(
            line, "grpc=[]",
            "dependencies are absent, so the feature is empty"
        );
        return;
    }
    let dependencies = active_lines("dependencies");
    for name in GRPC_CRATES {
        let line = dependencies
            .iter()
            .find(|line| names_crate(line, name))
            .copied()
            .unwrap_or_default();
        assert!(
            !line.is_empty(),
            "`{name}` must be declared when the link is enabled"
        );
        assert!(
            line.contains("optional = true"),
            "`{name}` must be optional: {line}"
        );
    }
    assert_ne!(
        grpc_feature().unwrap_or_default().replace(' ', ""),
        "grpc=[]",
        "dependencies are declared, so the feature must enable them"
    );
}

#[test]
fn the_versions_live_in_one_place_ready_to_be_pinned() {
    let shared = active_lines("workspace.dependencies");
    for name in GRPC_CRATES {
        assert!(
            shared.iter().any(|line| names_crate(line, name)),
            "`{name}` must be declared in [workspace.dependencies]"
        );
    }
}

#[test]
fn the_template_for_enabling_the_link_is_in_the_manifest() {
    for name in GRPC_CRATES {
        let template = format!("# {name} = {{ workspace = true, optional = true }}");
        // Present while disabled; once enabled it has become a real line.
        assert!(
            MANIFEST.contains(&template) || used_by_the_package(),
            "the commented template for `{name}` is missing"
        );
    }
    assert!(
        MANIFEST.contains("\"dep:yellowstone-grpc-proto\""),
        "the manifest must show the feature line that enables the link"
    );
}

#[test]
fn ci_keeps_the_three_locked_gates_and_checks_the_link_when_it_exists() {
    assert!(CI.contains("cargo check --all-targets --locked"));
    assert!(CI.contains("cargo clippy --all-targets --locked -- -D warnings"));
    assert!(CI.contains("cargo test --locked"));
    assert!(
        CI.contains("--features grpc"),
        "CI must build the gRPC link once its dependencies are declared"
    );
}
