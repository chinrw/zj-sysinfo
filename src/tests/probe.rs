//! Async probe lifecycle and the identity tokens that fence it.

use super::super::*;

#[test]
fn probe_restarts_with_backoff_and_rejects_stale_results() {
    let mut probe = AsyncProbe::new(11, 3, 6);
    let ProbeAction::Start(first) = probe.on_cycle() else {
        panic!("first cycle did not start a probe");
    };
    assert_eq!(probe.on_cycle(), ProbeAction::Wait);
    assert_eq!(probe.on_cycle(), ProbeAction::Wait);
    let ProbeAction::Restart(second) = probe.on_cycle() else {
        panic!("third missed cycle did not restart the probe");
    };
    assert_ne!(first, second);
    assert!(!probe.complete(first));
    assert!(probe.complete(second));

    let ProbeAction::Start(third) = probe.on_cycle() else {
        panic!("completion did not return the probe to idle");
    };
    assert_eq!(third.generation, second.generation + 1);
}
#[test]
fn probe_token_includes_the_plugin_instance_nonce() {
    let mut current = AsyncProbe::new(11, 3, 6);
    let mut replacement = AsyncProbe::new(12, 3, 6);
    let ProbeAction::Start(current_token) = current.on_cycle() else {
        panic!("current instance did not start a probe");
    };
    let ProbeAction::Start(replacement_token) = replacement.on_cycle() else {
        panic!("replacement instance did not start a probe");
    };

    assert_eq!(current_token.generation, replacement_token.generation);
    assert!(!replacement.complete(current_token));
    assert!(replacement.complete(replacement_token));
}
#[test]
fn client_slot_one_is_the_only_active_runtime() {
    assert!(is_active_client(1));
    assert!(!is_active_client(0));
    assert!(!is_active_client(2));
}
#[test]
fn probe_context_round_trip_rejects_missing_or_wrong_metadata() {
    let token = ProbeToken {
        instance_nonce: 42,
        generation: 7,
    };
    let context = probe_context(token);
    assert_eq!(probe_token_from_context(&context), Some(token));

    for key in [
        PROBE_CONTEXT_KEY,
        PROBE_CONTEXT_NONCE_KEY,
        PROBE_CONTEXT_GENERATION_KEY,
    ] {
        let mut incomplete = context.clone();
        incomplete.remove(key);
        assert_eq!(probe_token_from_context(&incomplete), None);
    }

    let mut wrong_probe = context.clone();
    wrong_probe.insert(PROBE_CONTEXT_KEY.to_string(), "other".to_string());
    assert_eq!(probe_token_from_context(&wrong_probe), None);

    let mut malformed = context;
    malformed.insert(PROBE_CONTEXT_GENERATION_KEY.to_string(), "NaN".to_string());
    assert_eq!(probe_token_from_context(&malformed), None);
}
#[test]
fn publication_completion_requires_a_private_message_from_this_plugin() {
    let parse = |source_plugin_id, is_private, name: &str, payload| {
        publication_completion_nonce(17, source_plugin_id, is_private, name, payload)
    };

    assert_eq!(
        parse(Some(17), true, PUBLICATION_COMPLETE_MESSAGE, Some("42")),
        Some(42)
    );
    assert_eq!(
        parse(Some(18), true, PUBLICATION_COMPLETE_MESSAGE, Some("42")),
        None
    );
    assert_eq!(
        parse(Some(17), false, PUBLICATION_COMPLETE_MESSAGE, Some("42")),
        None
    );
    assert_eq!(parse(Some(17), true, "other", Some("42")), None);
    assert_eq!(
        parse(Some(17), true, PUBLICATION_COMPLETE_MESSAGE, Some("NaN")),
        None
    );
}
#[test]
fn random_nonce_preserves_all_entropy_bits() {
    let random = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    assert_eq!(
        instance_nonce_from_random(random),
        0xffeeddccbbaa99887766554433221100
    );
}
