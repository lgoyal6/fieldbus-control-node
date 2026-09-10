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

#[test]
fn the_frozen_manifest_puts_the_hog_in_a_higher_band_than_the_control_thread() {
    // The whole of change A in one assertion. If these two ever name the same
    // policy again, the cpu-hog control is back to being the same-priority
    // experiment that never fired in v1, and every claim made about it would
    // be describing something else.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../manifest/frozen.json");
    let raw = std::fs::read_to_string(path).expect("manifest/frozen.json must be committed");
    let m: serde_json::Value = serde_json::from_str(&raw).expect("manifest must be valid JSON");

    let hog = m["negative_controls"]
        .as_array()
        .expect("negative_controls must be an array")
        .iter()
        .find(|c| c["id"] == "cpu-hog")
        .expect("the manifest must define the cpu-hog control");

    assert_eq!(hog["control_thread_policy"], "default-timeshare");
    assert_eq!(hog["hog_thread_policy"], "mach-time-constraint");
    assert_ne!(
        hog["control_thread_policy"], hog["hog_thread_policy"],
        "a hog at the control thread's own policy is not a higher-priority hog"
    );

    // And the demotion is scoped to that one control. Nothing else in the file
    // may name a control-thread policy, because nothing else has one.
    for c in m["negative_controls"].as_array().unwrap() {
        if c["id"] != "cpu-hog" {
            assert!(
                c["control_thread_policy"].is_null(),
                "{} must not set a control-thread policy",
                c["id"]
            );
        }
    }
}

#[test]
fn the_frozen_manifest_keeps_the_v1_positive_gate_word_for_word() {
    // The claim the v2 file makes about itself, checked as bytes. Two changes
    // were pre-registered and neither of them is a threshold, so the block the
    // positive runs are judged against has to be the v1 block exactly.
    let v2 = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../manifest/frozen.json"
    ))
    .expect("manifest/frozen.json must be committed");
    let v1 = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../manifest/frozen-v1.json"
    ))
    .expect("manifest/frozen-v1.json must be committed");

    fn positive_gate_block(raw: &str) -> &str {
        let start = raw
            .find("\"positive_gate\": {")
            .expect("every manifest states a positive gate");
        let mut depth = 0usize;
        for (i, ch) in raw[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &raw[start..start + i + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated positive_gate block");
    }

    assert_eq!(
        positive_gate_block(&v2),
        positive_gate_block(&v1),
        "the v2 positive gate is not byte-identical to v1's"
    );
}

#[test]
fn the_frozen_watchdog_budget_is_eight_periods_and_the_bound_is_the_brief_s() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../manifest/frozen.json");
    let raw = std::fs::read_to_string(path).expect("manifest/frozen.json must be committed");
    let m: serde_json::Value = serde_json::from_str(&raw).expect("manifest must be valid JSON");

    let period = m["loop"]["target_period_us"].as_u64().unwrap();
    let timeout = m["watchdog"]["timeout_us"].as_u64().unwrap();
    assert_eq!(timeout, 8 * period, "change B froze eight periods");

    let freeze = m["negative_controls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "sensor-freeze")
        .expect("the manifest must define the sensor-freeze control");
    // The literal requirement: 100 ms from the last accepted frame to the safe
    // state, not one period past an internal budget.
    assert_eq!(freeze["max_reaction_time_us"].as_u64(), Some(100_000));
    // And the budget has to leave room inside that bound after rounding up to
    // the next whole period, or the design is back on v1's knife edge.
    assert!(timeout + period <= 100_000);
}
