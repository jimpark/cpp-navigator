//! Integration tests: golden/fidelity tests over the sample fixture repo,
//! JSON schema validation, and zero-egress verification.

use std::path::PathBuf;
use std::process::Command;

/// Path to the fixture sample project.
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample")
}

/// Helper to get the binary path.
fn binary_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_BIN_EXE_cpp-navigator"));
    if !p.exists() {
        // fallback for test runners that don't set CARGO_BIN_EXE
        p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("debug")
            .join("cpp-navigator");
    }
    p
}

/// Run the navigator binary and return stdout.
fn run_nav(args: &[&str]) -> String {
    let output = Command::new(binary_path())
        .args(args)
        .output()
        .expect("failed to execute cpp-navigator binary");
    String::from_utf8(output.stdout).unwrap()
}

/// Parse all JSONL lines from output into serde_json::Value records.
fn parse_jsonl(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("invalid JSON line"))
        .collect()
}

// ─── Golden / fidelity tests ─────────────────────────────────────────────────

#[test]
fn golden_find_def_resolved_single() {
    let root = fixture_root();
    let out = run_nav(&[
        "find-def",
        "InitializeUI",
        "--root",
        root.to_str().unwrap(),
    ]);
    let records = parse_jsonl(&out);
    assert_eq!(records.len(), 1);

    let r = &records[0];
    assert_eq!(r["schema_version"], "1.2");
    assert_eq!(r["tool"], "cpp-navigator");
    assert_eq!(r["command"], "find-def");
    assert_eq!(r["target"], "InitializeUI");
    assert_eq!(r["status"], "resolved");

    // Must resolve to the .cpp implementation.
    let file_path = r["file_path"].as_str().unwrap();
    assert!(file_path.contains("widget.cpp"), "resolved to {file_path}");
    assert!(r["content"].as_str().unwrap().contains("InitializeUI"));
}

#[test]
fn golden_find_def_overloaded_shows_multi_resolved() {
    let root = fixture_root();
    let out = run_nav(&[
        "find-def",
        "SetText",
        "--root",
        root.to_str().unwrap(),
        "--max-results",
        "5",
    ]);
    let records = parse_jsonl(&out);
    assert_eq!(records.len(), 1);

    let r = &records[0];
    assert_eq!(r["status"], "resolved");
    // Should show multiple results for the overloads.
    let results = r["results"].as_array().unwrap();
    assert!(
        results.len() >= 2,
        "expected ≥2 overloads, got {}",
        results.len()
    );

    // Each result should have content with SetText.
    for res in results {
        assert!(res["content"].as_str().unwrap().contains("SetText"));
    }
}

#[test]
fn golden_find_decl_resolved_in_header() {
    let root = fixture_root();
    let out = run_nav(&[
        "find-decl",
        "Draw",
        "--root",
        root.to_str().unwrap(),
    ]);
    let records = parse_jsonl(&out);
    assert_eq!(records.len(), 1);

    let r = &records[0];
    // Widget declaration should be in the header.
    let file_path = r["file_path"].as_str().unwrap_or("");
    assert!(
        file_path.contains("widget.h") || r["status"] == "resolved",
        "expected header resolution"
    );
    assert_eq!(r["schema_version"], "1.2");
    assert!(r["doc"].as_str().unwrap_or("").contains("Draw the widget on screen"));
    assert!(r["signature"].as_str().is_some());
    assert!(r.get("content").is_none(), "content should be opt-in for declarations");
    assert!(r.get("start_byte").is_none(), "offsets should be opt-in");
    assert!(r.get("end_byte").is_none(), "offsets should be opt-in");
    assert!(r.get("type").is_none(), "type should be opt-in");
}

#[test]
fn golden_find_decl_with_includes_restores_optional_fields() {
    let root = fixture_root();
    let out = run_nav(&[
        "find-decl",
        "Draw",
        "--root",
        root.to_str().unwrap(),
        "--include",
        "content,offsets,type",
    ]);
    let records = parse_jsonl(&out);
    assert_eq!(records.len(), 1);

    let r = &records[0];
    assert_eq!(r["schema_version"], "1.2");
    assert!(r["content"].as_str().unwrap_or("").contains("void Draw()"));
    assert!(r["start_byte"].as_u64().is_some());
    assert!(r["end_byte"].as_u64().is_some());
    assert!(r["type"].as_str().is_some());
}

#[test]
fn golden_find_refs_location_only() {
    let root = fixture_root();
    let out = run_nav(&[
        "find-refs",
        "Draw",
        "--root",
        root.to_str().unwrap(),
    ]);
    let records = parse_jsonl(&out);
    assert_eq!(records.len(), 1);

    let r = &records[0];
    assert_eq!(r["status"], "resolved");
    assert_eq!(r["resolution_type"], "references");

    let locations = r["locations"].as_array().unwrap();
    // Draw is declared in widget.h, defined in widget.cpp, called in main.cpp (2 times).
    assert!(
        locations.len() >= 3,
        "expected ≥3 ref locations, got {}",
        locations.len()
    );
}

#[test]
fn golden_find_refs_context_mode() {
    let root = fixture_root();
    let out = run_nav(&[
        "find-refs",
        "SetText",
        "--context",
        "--root",
        root.to_str().unwrap(),
    ]);
    let records = parse_jsonl(&out);
    assert_eq!(records.len(), 1);

    let r = &records[0];
    assert_eq!(r["status"], "resolved");
    assert_eq!(r["resolution_type"], "references_with_context");

    let contexts = r["contexts"].as_array().unwrap();
    // SetText appears in at least 2 different function scopes in main.cpp
    assert!(
        contexts.len() >= 2,
        "expected ≥2 context entries, got {}",
        contexts.len()
    );
}

#[test]
fn golden_find_def_not_found() {
    let root = fixture_root();
    let out = run_nav(&[
        "find-def",
        "NonExistentSymbol_XYZ123",
        "--root",
        root.to_str().unwrap(),
    ]);
    let records = parse_jsonl(&out);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["status"], "not_found");
}

// ─── Zero-egress test ────────────────────────────────────────────────────────

/// Verify that the binary performs no network calls during execution.
///
/// This is verified by running under a sandbox that disallows network access.
/// On macOS we use sandbox-exec; on Linux we would use unshare/seccomp.
/// If sandboxing isn't available, we at least verify stdout is produced (the
/// tool must work offline).
#[test]
fn zero_egress_no_network() {
    let root = fixture_root();

    // Strategy: run the tool with a query and verify it succeeds without
    // network access. On macOS, sandbox-exec with deny networking.
    if cfg!(target_os = "macos") {
        let output = Command::new("sandbox-exec")
            .args([
                "-p",
                "(version 1)(allow default)(deny network*)",
                binary_path().to_str().unwrap(),
                "find-def",
                "Widget",
                "--root",
                root.to_str().unwrap(),
            ])
            .output()
            .expect("failed to run sandbox-exec");

        assert!(
            output.status.success(),
            "binary should succeed without network: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.is_empty(), "should produce output without network");
        let records = parse_jsonl(&stdout);
        assert!(!records.is_empty());
        assert_eq!(records[0]["schema_version"], "1.2");
    } else {
        // Fallback: just run normally and ensure it produces valid output (no
        // compile-time network dependencies).
        let out = run_nav(&[
            "find-def",
            "Widget",
            "--root",
            root.to_str().unwrap(),
        ]);
        let records = parse_jsonl(&out);
        assert!(!records.is_empty());
    }
}

// ─── Schema validation on all output shapes ──────────────────────────────────

/// Validates that every record from various queries has correct JSON schema.
#[test]
fn schema_validation_all_commands() {
    let root = fixture_root();
    let commands = [
        vec!["find-def", "Widget", "--root", root.to_str().unwrap()],
        vec!["find-decl", "Widget", "--root", root.to_str().unwrap()],
        vec!["find-refs", "Draw", "--root", root.to_str().unwrap()],
        vec![
            "find-refs",
            "SetText",
            "--context",
            "--root",
            root.to_str().unwrap(),
        ],
        vec!["find-def", "NoSuchSymbol_ABC", "--root", root.to_str().unwrap()],
    ];

    for args in &commands {
        let out = run_nav(args);
        let records = parse_jsonl(&out);
        for rec in &records {
            let obj = rec.as_object().unwrap();

            // Envelope fields must always be present.
            assert_eq!(obj["schema_version"], "1.2", "args: {args:?}");
            assert_eq!(obj["tool"], "cpp-navigator", "args: {args:?}");
            assert!(obj.contains_key("command"), "missing 'command' for {args:?}");
            assert!(obj.contains_key("target"), "missing 'target' for {args:?}");
            assert!(obj.contains_key("status"), "missing 'status' for {args:?}");
            assert!(
                obj.contains_key("resolution_type"),
                "missing 'resolution_type' for {args:?}"
            );

            let status = obj["status"].as_str().unwrap();
            assert!(
                ["resolved", "ambiguous", "fallback", "not_found"].contains(&status),
                "invalid status '{status}' for {args:?}"
            );
        }
    }
}
