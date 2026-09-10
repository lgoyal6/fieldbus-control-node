//! Guards on the frozen manifests themselves.
//!
//! `manifest/frozen.json` is the manifest in force. `manifest/frozen-v1.json`
//! is the one experiment v1 was gated against, kept byte for byte so the v1
//! results under `results/v1/` remain checkable against the file that produced
//! them rather than against a later edit of it.

use std::path::Path;

use fieldbus_host::report::ManifestRef;

/// The SHA-256 the v1 result files name as the manifest they were gated
/// against. It appears in `results/v1/completion.json`, in
/// `results/v1/completion-rerun.json` and in the README, so this is the hash
/// every v1 claim is anchored to.
const V1_SHA256: &str = "e8c0a1d77ad76de81bacc12ffba565207440bcd6d9c5cbdcf84a0e1e24e92220";

#[test]
fn the_v1_manifest_is_byte_identical_to_the_one_v1_was_gated_against() {
    // A superseded manifest that can still be edited is not a record. If this
    // fails, either the file changed or the v1 results are describing an
    // experiment whose definition no longer exists.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../manifest/frozen-v1.json");
    let got = ManifestRef::of(Path::new(path)).expect("manifest/frozen-v1.json must be committed");
    assert_eq!(
        got.sha256, V1_SHA256,
        "manifest/frozen-v1.json no longer hashes to the value recorded in results/v1/"
    );
}

#[test]
fn the_v1_results_name_the_v1_manifest_hash() {
    // The other half of the same check, from the results' side: the evidence
    // files must still point at the hash above. Together the two make the v1
    // record self-verifying without a reader having to trust the README.
    for name in ["completion.json", "completion-rerun.json"] {
        let path = format!("{}/../results/v1/{name}", env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let v: serde_json::Value = serde_json::from_str(&raw).expect("v1 result must be JSON");
        assert_eq!(
            v["manifest"]["sha256"].as_str(),
            Some(V1_SHA256),
            "{name} names a different manifest hash"
        );
    }
}
