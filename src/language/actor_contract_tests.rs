use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

use serde_json::{Value, json};

use crate::{
    DocumentId, DocumentStamp, EditRevision, NavigationGeneration,
    language::{
        actor::*,
        framing::{FrameDecoder, JsonRpcMessage, RpcId, encode_message},
        snapshot::{
            CaretGeneration, ClientRequestId, DefinitionResultId, LanguageSnapshot, LanguageStatus,
            LspVersion, NoticeId, ProcessGeneration, SnapshotRevision, WriteSequence,
        },
        text_index::{MAX_ACCEPTED_DIAGNOSTICS, MAX_DIAGNOSTIC_MESSAGE_BYTES, MAX_SOURCE_BYTES},
    },
};

fn stamp(document: u64, revision: u64) -> DocumentStamp {
    DocumentStamp {
        document_id: DocumentId::from_raw(document).expect("document"),
        edit_revision: EditRevision::from_raw(revision).expect("revision"),
    }
}

fn desired(document: u64, revision: u64, source: &str) -> DurableDesiredDocument {
    DurableDesiredDocument {
        stamp: stamp(document, revision),
        source: Arc::from(source),
    }
}

fn rpc(value: Value) -> AccountedJsonRpcMessage {
    let body = serde_json::to_vec(&value).expect("serialize fixture");
    let mut wire = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    wire.extend(body);
    let message = FrameDecoder::new()
        .push(&wire)
        .expect("decode fixture")
        .into_iter()
        .next()
        .expect("one message");
    AccountedJsonRpcMessage::from_message(message)
}

fn decode_frame(bytes: &[u8]) -> JsonRpcMessage {
    FrameDecoder::new()
        .push(bytes)
        .expect("decode actor frame")
        .into_iter()
        .next()
        .expect("one actor frame")
}

fn io_effect(effects: &[ActorEffect]) -> Option<&ActorEffect> {
    effects
        .iter()
        .find(|effect| !matches!(effect, ActorEffect::PublishSnapshot { .. }))
}

fn send(effects: &[ActorEffect]) -> (ProcessGeneration, WriteSequence, FrameKind, Value) {
    let ActorEffect::SendFrame {
        generation,
        sequence,
        kind,
        bytes,
    } = io_effect(effects).expect("send effect")
    else {
        panic!("expected send effect: {effects:#?}");
    };
    (
        *generation,
        *sequence,
        kind.clone(),
        decode_frame(bytes).into_value(),
    )
}

fn valid_initialize(id: i32) -> AccountedJsonRpcMessage {
    rpc(json!({
        "jsonrpc":"2.0",
        "id":id,
        "result":{
            "capabilities":{
                "positionEncoding":"utf-16",
                "textDocumentSync":{"openClose":true,"change":1},
                "definitionProvider":true,
                "semanticTokensProvider":{
                    "legend":{"tokenTypes":["keyword","comment","string","number","variable","operator"],"tokenModifiers":[]},
                    "full":true
                }
            }
        }
    }))
}

fn new_actor() -> LanguageActor {
    LanguageActor::new(ActorSeeds::default()).expect("actor")
}

fn launch(actor: &mut LanguageActor) -> ProcessGeneration {
    assert!(
        actor
            .reduce(ActorEvent::Start {
                now: ManualTick::from_raw(0),
            })
            .is_empty()
    );
    let effects = actor.reduce(ActorEvent::Drive);
    let ActorEffect::LaunchGeneration { generation } = io_effect(&effects).expect("launch") else {
        panic!("expected launch: {effects:#?}");
    };
    actor.assert_invariants().expect("launch invariants");
    *generation
}

fn initialize(
    actor: &mut LanguageActor,
    generation: ProcessGeneration,
) -> (WriteSequence, ClientRequestId) {
    assert!(
        actor
            .reduce(ActorEvent::LaunchFinished {
                generation,
                outcome: LaunchOutcome::Ready,
            })
            .iter()
            .all(|effect| matches!(effect, ActorEffect::PublishSnapshot { .. }))
    );
    let effects = actor.reduce(ActorEvent::Drive);
    let (
        _,
        sequence,
        FrameKind::ClientRequest {
            id,
            kind: RequestKind::Initialize,
        },
        value,
    ) = send(&effects)
    else {
        panic!("expected initialize")
    };
    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["method"], "initialize");
    assert!(actor.has_request(id));
    (sequence, id)
}

fn make_ready(actor: &mut LanguageActor, generation: ProcessGeneration) {
    let (sequence, id) = initialize(actor, generation);
    assert!(
        actor
            .reduce(ActorEvent::ReaderMessage {
                generation,
                message: valid_initialize(id.get()),
            })
            .is_empty()
    );
    assert_eq!(actor.writer_sequence(), Some(sequence));
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    let effects = actor.reduce(ActorEvent::Drive);
    let (_, initialized_sequence, FrameKind::Initialized, value) = send(&effects) else {
        panic!("expected initialized")
    };
    assert_eq!(value["method"], "initialized");
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: initialized_sequence,
        outcome: WriterOutcome::Flushed,
    });
    assert_eq!(actor.protocol_phase(), Some(ProtocolPhase::Ready));
}

fn open_current(
    actor: &mut LanguageActor,
    generation: ProcessGeneration,
) -> (WriteSequence, LspVersion) {
    let effects = actor.reduce(ActorEvent::Drive);
    let (_, sequence, FrameKind::DidOpen { fence }, value) = send(&effects) else {
        panic!("expected didOpen")
    };
    assert_eq!(value["method"], "textDocument/didOpen");
    let version = fence.lsp_version;
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    (sequence, version)
}

fn ready_with_open(source: &str) -> (LanguageActor, ProcessGeneration, DocumentFence) {
    let mut actor = new_actor();
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, source)),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    open_current(&mut actor, generation);
    let fence = actor.written_fence().expect("written fence").clone();
    (actor, generation, fence)
}

fn idle_active_actor() -> (LanguageActor, ProcessGeneration) {
    let mut actor = new_actor();
    let generation = launch(&mut actor);
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    (actor, generation)
}

fn actor_with_change_flight() -> (LanguageActor, ProcessGeneration) {
    let (mut actor, generation, _) = ready_with_open("var a = 1;");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, "var middle = 2;")),
    });
    let (_, _, FrameKind::DidChange { .. }, _) = send(&actor.reduce(ActorEvent::Drive)) else {
        panic!("expected change flight")
    };
    (actor, generation)
}

fn published_snapshot(effects: &[ActorEffect]) -> Option<&LanguageSnapshot> {
    effects.iter().find_map(|effect| match effect {
        ActorEffect::PublishSnapshot { snapshot } => Some(snapshot.as_ref()),
        _ => None,
    })
}

fn diagnostics(uri: &str, version: Option<i32>, items: Value) -> AccountedJsonRpcMessage {
    let mut params = serde_json::Map::new();
    params.insert("uri".to_owned(), Value::String(uri.to_owned()));
    if let Some(version) = version {
        params.insert("version".to_owned(), Value::from(version));
    }
    params.insert("diagnostics".to_owned(), items);
    rpc(json!({
        "jsonrpc":"2.0",
        "method":"textDocument/publishDiagnostics",
        "params":params
    }))
}

fn response(id: ClientRequestId, result: Value) -> AccountedJsonRpcMessage {
    rpc(json!({"jsonrpc":"2.0","id":id.get(),"result":result}))
}

fn error_response(id: ClientRequestId, message: &str) -> AccountedJsonRpcMessage {
    rpc(json!({
        "jsonrpc":"2.0",
        "id":id.get(),
        "error":{"code":-32603,"message":message}
    }))
}

fn request_from_effect(
    effects: &[ActorEffect],
    expected: RequestKind,
) -> (WriteSequence, ClientRequestId) {
    let (_, sequence, FrameKind::ClientRequest { id, kind }, _) = send(effects) else {
        panic!("expected request")
    };
    assert_eq!(kind, expected);
    (sequence, id)
}

#[test]
fn start_initialize_and_initialized_are_drive_only_and_two_phase() {
    for response_first in [false, true] {
        let mut actor = new_actor();
        actor.reduce(ActorEvent::DesiredDocumentChanged {
            desired: Some(desired(7, 1, "var value = 1;")),
        });
        let generation = launch(&mut actor);
        let (sequence, id) = initialize(&mut actor, generation);

        if response_first {
            actor.reduce(ActorEvent::ReaderMessage {
                generation,
                message: valid_initialize(id.get()),
            });
            assert_eq!(actor.writer_sequence(), Some(sequence));
            assert!(
                actor
                    .reduce(ActorEvent::Drive)
                    .iter()
                    .all(|effect| { matches!(effect, ActorEffect::PublishSnapshot { .. }) })
            );
            actor.reduce(ActorEvent::WriterFinished {
                generation,
                sequence,
                outcome: WriterOutcome::Flushed,
            });
        } else {
            actor.reduce(ActorEvent::WriterFinished {
                generation,
                sequence,
                outcome: WriterOutcome::Flushed,
            });
            assert_eq!(actor.writer_sequence(), None);
            actor.reduce(ActorEvent::ReaderMessage {
                generation,
                message: valid_initialize(id.get()),
            });
        }
        assert_eq!(actor.protocol_phase(), Some(ProtocolPhase::NeedInitialized));
        let effects = actor.reduce(ActorEvent::Drive);
        let (_, initialized, FrameKind::Initialized, _) = send(&effects) else {
            panic!("expected initialized")
        };
        assert_eq!(
            actor.protocol_phase(),
            Some(ProtocolPhase::InitializedWriting)
        );
        assert!(actor.written_fence().is_none());
        actor.reduce(ActorEvent::WriterFinished {
            generation,
            sequence: initialized,
            outcome: WriterOutcome::Flushed,
        });
        assert_eq!(actor.protocol_phase(), Some(ProtocolPhase::Ready));
        assert!(actor.written_fence().is_none());
        actor.assert_invariants().expect("two-phase invariants");
    }
}

#[test]
fn document_sync_coalesces_changes_and_closes_before_replacement_open() {
    let mut actor = new_actor();
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    let (_, first_version) = open_current(&mut actor, generation);
    assert_eq!(first_version.get(), 1);
    assert_eq!(actor.written_fence().expect("written").stamp, stamp(1, 1));

    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, "var a = 2;")),
    });
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 3, "var a = 3;")),
    });
    let (_, change_sequence, FrameKind::DidChange { fence }, value) =
        send(&actor.reduce(ActorEvent::Drive))
    else {
        panic!("expected didChange")
    };
    assert_eq!(fence.stamp, stamp(1, 3));
    assert_eq!(value["params"]["contentChanges"][0]["text"], "var a = 3;");
    assert_eq!(
        actor.written_fence().expect("old written").stamp,
        stamp(1, 1)
    );
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: change_sequence,
        outcome: WriterOutcome::Flushed,
    });
    assert_eq!(actor.written_fence().expect("changed").stamp, stamp(1, 3));

    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(2, 1, "print a;")),
    });
    let (_, close_sequence, FrameKind::DidClose { .. }, _) = send(&actor.reduce(ActorEvent::Drive))
    else {
        panic!("expected didClose")
    };
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: close_sequence,
        outcome: WriterOutcome::Flushed,
    });
    assert!(actor.written_fence().is_none());
    let (_, _, FrameKind::DidOpen { fence }, _) = send(&actor.reduce(ActorEvent::Drive)) else {
        panic!("expected replacement didOpen")
    };
    assert_eq!(fence.stamp, stamp(2, 1));
    assert_eq!(fence.lsp_version.get(), 3);
    actor.assert_invariants().expect("sync invariants");
}

#[test]
fn same_stamp_different_source_is_terminal_and_future_finalization_keeps_the_barrier_safe() {
    let mut actor = new_actor();
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "first")),
    });
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "second")),
    });
    assert!(actor.is_terminal());
    actor
        .assert_invariants()
        .expect("terminal desired invariant");

    let mut actor = new_actor();
    let generation = launch(&mut actor);
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    let effects = actor.reduce(ActorEvent::ReaderFatal {
        generation,
        cause: ReaderFatalCause::Framing,
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup { generation: value, .. }) if *value == generation
    ));
    let future = ProcessGeneration::from_raw(generation.get() + 1).expect("future");
    actor.reduce(ActorEvent::FinalizedGeneration {
        generation: future,
        stderr_tail: None,
    });
    assert!(actor.is_terminal());
    assert_eq!(actor.cleanup_pending(), Some(generation));
    actor
        .assert_invariants()
        .expect("future finalization invariant");
}

#[test]
fn shutdown_waits_for_shutdown_response_and_exit_flush() {
    let mut actor = new_actor();
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    actor.reduce(ActorEvent::ShutdownRequested);
    let (
        _,
        shutdown_sequence,
        FrameKind::ClientRequest {
            id,
            kind: RequestKind::Shutdown,
        },
        _,
    ) = send(&actor.reduce(ActorEvent::Drive))
    else {
        panic!("expected shutdown")
    };
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: rpc(json!({"jsonrpc":"2.0","id":id.get(),"result":null})),
    });
    assert_eq!(actor.writer_sequence(), Some(shutdown_sequence));
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: shutdown_sequence,
        outcome: WriterOutcome::Flushed,
    });
    assert_eq!(actor.protocol_phase(), Some(ProtocolPhase::NeedExit));
    let (_, exit_sequence, FrameKind::Exit, _) = send(&actor.reduce(ActorEvent::Drive)) else {
        panic!("expected exit")
    };
    assert!(actor.cleanup_pending().is_none());
    let effects = actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: exit_sequence,
        outcome: WriterOutcome::Flushed,
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            mode: CleanupMode::GracefulExitFlushed,
            ..
        })
    ));
    assert_eq!(actor.cleanup_pending(), Some(generation));
    actor.assert_invariants().expect("shutdown invariants");
}

#[test]
fn exact_deadlines_and_cleanup_failure_are_fail_closed_without_spinning() {
    let mut actor = new_actor();
    let generation = launch(&mut actor);
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    let effects = actor.reduce(ActorEvent::Tick {
        now: ManualTick::from_raw(LAUNCH_TIMEOUT_MS),
    });
    assert!(effects.is_empty(), "resolved launch has no launch deadline");
    let (_, sequence, _, _) = send(&actor.reduce(ActorEvent::Drive));
    assert!(
        actor
            .reduce(ActorEvent::Tick {
                now: ManualTick::from_raw(LAUNCH_TIMEOUT_MS + WRITE_ACK_TIMEOUT_MS - 1),
            })
            .is_empty()
    );
    let effects = actor.reduce(ActorEvent::Tick {
        now: ManualTick::from_raw(LAUNCH_TIMEOUT_MS + WRITE_ACK_TIMEOUT_MS),
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::WriteTimeout,
            ..
        })
    ));
    assert_ne!(actor.writer_sequence(), Some(sequence));
    actor.reduce(ActorEvent::CleanupFailed {
        generation,
        cause: CleanupFailure::Reap,
        stderr_tail: None,
    });
    assert!(actor.is_terminal());
    assert_eq!(actor.cleanup_pending(), Some(generation));
    assert!(
        actor
            .reduce(ActorEvent::Drive)
            .iter()
            .all(|effect| { matches!(effect, ActorEffect::PublishSnapshot { .. }) })
    );
    actor
        .assert_invariants()
        .expect("cleanup failure invariants");
}

#[test]
fn snapshot_acknowledgments_are_revision_fenced() {
    let mut actor = new_actor();
    let generation = launch(&mut actor);
    let effects = actor.reduce(ActorEvent::Drive);
    let published = effects.iter().find_map(|effect| match effect {
        ActorEffect::PublishSnapshot { snapshot } => Some(snapshot.revision()),
        _ => None,
    });
    if let Some(revision) = published {
        actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
            process_generation: generation,
            observed_revision: SnapshotRevision::from_raw(revision.get()).expect("ordinary"),
            definition: None,
            notices: Arc::from([]),
        });
    }
    assert_eq!(actor.active_generation(), Some(generation));
    actor.assert_invariants().expect("ack invariants");
}

#[test]
fn configured_budget_constants_match_the_adapter_contract() {
    assert_eq!(READER_INBOX_ITEMS, 64);
    assert_eq!(READER_INBOX_BODY_BYTES, 24 * 1024 * 1024);
    assert_eq!(MAX_LEDGER_ENTRIES, 4);
    assert_eq!(MAX_SERVER_REPLIES, 32);
    assert_eq!(MAX_SERVER_REPLY_BYTES, 64 * 1024);
    assert_eq!(MAX_ACTOR_STATE_BYTES, 128 * 1024 * 1024);
}

#[test]
fn definition_request_type_carries_app_wide_navigation_and_captured_character() {
    let _ = ActorEvent::DefinitionRequested {
        fence: UiDocumentFence {
            process_generation: ProcessGeneration::from_raw(1).expect("generation"),
            stamp: stamp(1, 1),
            lsp_version: Some(LspVersion::from_raw(1).expect("version")),
        },
        caret_generation: CaretGeneration::from_raw(1).expect("caret"),
        navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
        primary_character: 0,
    };
}

#[test]
fn matching_pre_ack_diagnostics_replace_then_publish_only_after_sync_flush() {
    let mut actor = new_actor();
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    let effects = actor.reduce(ActorEvent::Drive);
    let (_, open_sequence, FrameKind::DidOpen { fence }, _) = send(&effects) else {
        panic!("expected didOpen")
    };
    let first = json!([{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "severity":2,"code":"first","source":"rlox","message":"first",
        "data":{"phase":"scanner"}
    }]);
    let replacement = json!([{
        "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}},
        "severity":1,"code":"second","source":"rlox","message":"replacement",
        "data":{"phase":"parser"}
    }]);
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(&fence.uri, Some(fence.lsp_version.get()), first),
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(&fence.uri, Some(fence.lsp_version.get()), replacement),
    });
    let before_ack = actor.reduce(ActorEvent::Drive);
    assert!(
        published_snapshot(&before_ack).is_some_and(|snapshot| snapshot.diagnostics().is_none())
    );

    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: open_sequence,
        outcome: WriterOutcome::Flushed,
    });
    let after_ack = actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&after_ack).expect("diagnostic publication");
    let batch = snapshot.diagnostics().expect("diagnostic batch");
    assert_eq!(batch.items.len(), 1);
    assert_eq!(batch.items[0].message.as_ref(), "replacement");
    assert_eq!(
        batch.items[0].phase,
        Some(crate::language::snapshot::AnalysisPhase::Parser)
    );
    actor.assert_invariants().expect("pre-ack invariants");
}

#[test]
fn only_matching_versioned_empty_diagnostics_clear_a_visible_batch() {
    let (mut actor, generation, fence) = ready_with_open("var a = 1;");
    let nonempty = json!([{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "severity":1,"message":"problem","data":{"phase":"compiler"}
    }]);
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(&fence.uri, Some(fence.lsp_version.get()), nonempty),
    });
    let visible = actor.reduce(ActorEvent::Drive);
    let first = published_snapshot(&visible)
        .and_then(LanguageSnapshot::diagnostics)
        .expect("visible diagnostics")
        .revision;

    for message in [
        diagnostics(&fence.uri, None, json!([])),
        diagnostics(&fence.uri, Some(fence.lsp_version.get() + 1), json!([])),
        diagnostics(
            "oxide-document://local/99.ox",
            Some(fence.lsp_version.get()),
            json!([]),
        ),
    ] {
        actor.reduce(ActorEvent::ReaderMessage {
            generation,
            message,
        });
        assert_eq!(
            actor
                .reduce(ActorEvent::Drive)
                .iter()
                .find_map(|effect| match effect {
                    ActorEffect::PublishSnapshot { snapshot } => {
                        snapshot.diagnostics().map(|batch| batch.revision)
                    }
                    _ => None,
                })
                .unwrap_or(first),
            first
        );
    }

    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(&fence.uri, Some(fence.lsp_version.get()), json!([])),
    });
    let cleared = actor.reduce(ActorEvent::Drive);
    let empty = published_snapshot(&cleared)
        .and_then(LanguageSnapshot::diagnostics)
        .expect("matching empty batch");
    assert!(empty.items.is_empty());
    assert!(empty.revision > first);
}

#[test]
fn semantic_tokens_are_fenced_and_invalid_results_clear_only_syntax() {
    let (mut actor, generation, _fence) = ready_with_open("var a = 1;");
    let effects = actor.reduce(ActorEvent::Drive);
    let (sequence, id) = request_from_effect(&effects, RequestKind::SemanticTokens);
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(id, json!({"data":[0,0,3,0,0,0,4,1,4,0]})),
    });
    let published = actor.reduce(ActorEvent::Drive);
    assert_eq!(
        published_snapshot(&published)
            .and_then(LanguageSnapshot::syntax)
            .expect("syntax")
            .runs
            .len(),
        2
    );

    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, "var b = 1;")),
    });
    let (_, change_sequence, FrameKind::DidChange { .. }, _) =
        send(&actor.reduce(ActorEvent::Drive))
    else {
        panic!("expected change")
    };
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: change_sequence,
        outcome: WriterOutcome::Flushed,
    });
    let effects = actor.reduce(ActorEvent::Drive);
    let (sequence, id) = request_from_effect(&effects, RequestKind::SemanticTokens);
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(id, json!({"data":[0,0,0,0,0]})),
    });
    let invalid = actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&invalid).expect("invalid syntax publication");
    assert!(snapshot.syntax().is_none());
    assert!(!snapshot.notices().is_empty());
    assert_eq!(actor.protocol_phase(), Some(ProtocolPhase::Ready));
}

#[test]
fn definition_echoes_exact_intent_and_stale_ack_cannot_remove_it() {
    let (mut actor, generation, fence) = ready_with_open("var a = 1;\nprint a;");
    let caret = CaretGeneration::from_raw(1).expect("caret");
    let navigation = NavigationGeneration::from_raw(7).expect("navigation");
    let ui_fence = UiDocumentFence {
        process_generation: generation,
        stamp: fence.stamp,
        lsp_version: Some(fence.lsp_version),
    };
    actor.reduce(ActorEvent::CaretChanged {
        fence: ui_fence,
        caret_generation: caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: caret,
        navigation_generation: navigation,
        primary_character: 17,
    });
    let effects = actor.reduce(ActorEvent::Drive);
    let (sequence, id) = request_from_effect(&effects, RequestKind::Definition);
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(
            id,
            json!([{
                "uri":fence.uri,
                "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}}
            }]),
        ),
    });
    let published = actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&published).expect("definition snapshot");
    let definition = snapshot.definition().expect("definition result");
    assert_eq!(definition.navigation_generation, navigation);
    assert_eq!(definition.caret_character, 17);
    assert_eq!(definition.targets.len(), 1);
    let result_id = definition.id;
    let published_revision = snapshot.revision();

    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: SnapshotRevision::from_raw(published_revision.get() - 1)
            .expect("stale revision"),
        definition: Some(result_id),
        notices: Arc::from([]),
    });
    actor.reduce(ActorEvent::StderrTailChanged {
        generation,
        tail: BoundedStderrTail {
            text: Arc::from("unrelated"),
            line_count: 1,
            truncated: false,
        },
    });
    let retained = actor.reduce(ActorEvent::Drive);
    assert_eq!(
        published_snapshot(&retained)
            .and_then(LanguageSnapshot::definition)
            .map(|result| result.id),
        Some(result_id)
    );
    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: published_revision,
        definition: Some(result_id),
        notices: Arc::from([]),
    });
    let cleared = actor.reduce(ActorEvent::Drive);
    assert!(published_snapshot(&cleared).is_some_and(|snapshot| snapshot.definition().is_none()));
}

#[test]
fn newer_definition_at_same_caret_never_lends_its_identity_to_old_response() {
    let (mut actor, generation, fence) = ready_with_open("var a = 1;\nprint a;");
    let caret = CaretGeneration::from_raw(1).expect("caret");
    let ui_fence = UiDocumentFence {
        process_generation: generation,
        stamp: fence.stamp,
        lsp_version: Some(fence.lsp_version),
    };
    actor.reduce(ActorEvent::CaretChanged {
        fence: ui_fence,
        caret_generation: caret,
    });
    let first_navigation = NavigationGeneration::from_raw(1).expect("navigation");
    let second_navigation = NavigationGeneration::from_raw(2).expect("navigation");
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: caret,
        navigation_generation: first_navigation,
        primary_character: 17,
    });
    let (first_sequence, first_id) =
        request_from_effect(&actor.reduce(ActorEvent::Drive), RequestKind::Definition);
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: caret,
        navigation_generation: second_navigation,
        primary_character: 17,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(first_id, Value::Null),
    });
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: first_sequence,
        outcome: WriterOutcome::Flushed,
    });
    let (second_sequence, second_id) =
        request_from_effect(&actor.reduce(ActorEvent::Drive), RequestKind::Definition);
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: second_sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(second_id, Value::Null),
    });
    let published = actor.reduce(ActorEvent::Drive);
    assert_eq!(
        published_snapshot(&published)
            .and_then(LanguageSnapshot::definition)
            .map(|result| result.navigation_generation),
        Some(second_navigation)
    );
}

#[test]
fn server_requests_preserve_ids_fifo_and_overflow_fails_the_generation() {
    let mut actor = new_actor();
    let generation = launch(&mut actor);
    let (_, initialize_id) = initialize(&mut actor, generation);
    let ids = [json!(-7), json!("server-2")];
    for id in &ids {
        actor.reduce(ActorEvent::ReaderMessage {
            generation,
            message: rpc(
                json!({"jsonrpc":"2.0","id":id,"method":"server/unsupported","params":{}}),
            ),
        });
    }
    let initialize_sequence = actor.writer_sequence().expect("initialize writer");
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: initialize_sequence,
        outcome: WriterOutcome::Flushed,
    });
    for expected in ids {
        let effects = actor.reduce(ActorEvent::Drive);
        let (_, sequence, FrameKind::ServerResponse { .. }, value) = send(&effects) else {
            panic!("expected server response")
        };
        assert_eq!(value["id"], expected);
        assert_eq!(value["error"]["code"], -32601);
        actor.reduce(ActorEvent::WriterFinished {
            generation,
            sequence,
            outcome: WriterOutcome::Flushed,
        });
    }
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: valid_initialize(initialize_id.get()),
    });

    let mut actor = new_actor();
    let generation = launch(&mut actor);
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    for id in 0..MAX_SERVER_REPLIES {
        actor.reduce(ActorEvent::ReaderMessage {
            generation,
            message: rpc(
                json!({"jsonrpc":"2.0","id":format!("s{id}"),"method":"server/x","params":{}}),
            ),
        });
    }
    let effects = actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: rpc(json!({"jsonrpc":"2.0","id":"overflow","method":"server/x","params":{}})),
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::AdapterInvariant,
            ..
        })
    ));
}

#[test]
fn wrong_current_ack_is_fatal_while_old_generation_ack_is_ignored() {
    let mut actor = new_actor();
    let generation = launch(&mut actor);
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    let (_, sequence, _, _) = send(&actor.reduce(ActorEvent::Drive));
    let wrong = WriteSequence::from_raw(sequence.get() + 1).expect("wrong sequence");
    let effects = actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: wrong,
        outcome: WriterOutcome::Flushed,
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::AdapterInvariant,
            ..
        })
    ));

    let stale_generation = generation;
    actor.reduce(ActorEvent::FinalizedGeneration {
        generation,
        stderr_tail: None,
    });
    actor.reduce(ActorEvent::Tick {
        now: ManualTick::from_raw(BACKOFF_INITIAL_MS),
    });
    let next = actor.reduce(ActorEvent::Drive);
    let ActorEffect::LaunchGeneration {
        generation: next_generation,
    } = io_effect(&next).expect("retry launch")
    else {
        panic!("expected retry launch")
    };
    assert!(*next_generation > stale_generation);
    assert!(
        actor
            .reduce(ActorEvent::WriterFinished {
                generation: stale_generation,
                sequence,
                outcome: WriterOutcome::Flushed,
            })
            .is_empty()
    );
}

#[test]
fn snapshot_revision_exhaustion_suppresses_the_next_frame_and_publishes_one_terminal_snapshot() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_snapshot_revision: u64::MAX - 1,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::Start {
        now: ManualTick::from_raw(0),
    });
    let first = actor.reduce(ActorEvent::Drive);
    let ActorEffect::LaunchGeneration { generation } = io_effect(&first).expect("launch") else {
        panic!("expected launch")
    };
    assert_eq!(
        published_snapshot(&first)
            .expect("last ordinary")
            .revision()
            .get(),
        u64::MAX - 1
    );
    actor.reduce(ActorEvent::LaunchFinished {
        generation: *generation,
        outcome: LaunchOutcome::Ready,
    });
    let terminal = actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&terminal),
        Some(ActorEffect::BeginCleanup {
            generation: cleanup,
            cause: CleanupCause::CounterExhausted,
            ..
        }) if cleanup == generation
    ));
    assert!(!terminal.iter().any(|effect| matches!(
        effect,
        ActorEffect::SendFrame { .. } | ActorEffect::LaunchGeneration { .. }
    )));
    assert_eq!(
        published_snapshot(&terminal)
            .expect("terminal snapshot")
            .revision()
            .get(),
        u64::MAX
    );
    assert!(actor.is_terminal());
    assert_eq!(actor.cleanup_pending(), Some(*generation));
    assert!(actor.reduce(ActorEvent::Drive).is_empty());
    actor
        .assert_invariants()
        .expect("terminal revision invariants");
}

#[test]
fn stale_oversized_source_is_rejected_before_source_validation() {
    let mut actor = new_actor();
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, "current")),
    });
    let oversized = "x".repeat(MAX_SOURCE_BYTES + 1);
    assert!(
        actor
            .reduce(ActorEvent::DesiredDocumentChanged {
                desired: Some(desired(1, 1, &oversized)),
            })
            .is_empty()
    );
    assert!(!actor.is_terminal());
    actor.assert_invariants().expect("stale source ignored");
}

#[test]
fn unexpected_current_finalization_or_cleanup_failure_is_terminal() {
    for cleanup_failed in [false, true] {
        let mut actor = new_actor();
        let generation = launch(&mut actor);
        let effects = if cleanup_failed {
            actor.reduce(ActorEvent::CleanupFailed {
                generation,
                cause: CleanupFailure::Reap,
                stderr_tail: None,
            })
        } else {
            actor.reduce(ActorEvent::FinalizedGeneration {
                generation,
                stderr_tail: None,
            })
        };
        assert!(actor.is_terminal());
        assert_eq!(actor.cleanup_pending(), Some(generation));
        assert!(matches!(
            io_effect(&effects),
            Some(ActorEffect::BeginCleanup {
                generation: cleaned,
                cause: CleanupCause::AdapterInvariant,
                ..
            }) if *cleaned == generation
        ));
        actor
            .assert_invariants()
            .expect("unexpected finalization signal fails closed");
    }
}

#[test]
fn shutdown_drains_feature_correlation_without_publishing_late_results() {
    for kind in [RequestKind::SemanticTokens, RequestKind::Definition] {
        for response_first in [false, true] {
            for succeeds in [false, true] {
                let (mut actor, generation, fence) = ready_with_open("var a = 1;\nprint a;");
                if kind == RequestKind::Definition {
                    let ui_fence = UiDocumentFence {
                        process_generation: generation,
                        stamp: fence.stamp,
                        lsp_version: Some(fence.lsp_version),
                    };
                    let caret = CaretGeneration::from_raw(1).expect("caret");
                    actor.reduce(ActorEvent::CaretChanged {
                        fence: ui_fence,
                        caret_generation: caret,
                    });
                    actor.reduce(ActorEvent::DefinitionRequested {
                        fence: ui_fence,
                        caret_generation: caret,
                        navigation_generation: NavigationGeneration::from_raw(1)
                            .expect("navigation"),
                        primary_character: 17,
                    });
                }
                let (sequence, id) = request_from_effect(&actor.reduce(ActorEvent::Drive), kind);
                actor.reduce(ActorEvent::ShutdownRequested);
                let result = match kind {
                    RequestKind::SemanticTokens => json!({"data":[0,0,3,0,0]}),
                    RequestKind::Definition => Value::Null,
                    _ => unreachable!(),
                };
                let response = if succeeds {
                    response(id, result)
                } else {
                    error_response(id, "feature failed")
                };
                let response_event = ActorEvent::ReaderMessage {
                    generation,
                    message: response,
                };
                let ack_event = ActorEvent::WriterFinished {
                    generation,
                    sequence,
                    outcome: WriterOutcome::Flushed,
                };
                if response_first {
                    actor.reduce(response_event);
                    actor.reduce(ack_event);
                } else {
                    actor.reduce(ack_event);
                    actor.reduce(response_event);
                }

                let effects = actor.reduce(ActorEvent::Drive);
                let (_, _, FrameKind::ClientRequest { kind, .. }, _) = send(&effects) else {
                    panic!("shutdown did not follow drained feature")
                };
                assert_eq!(kind, RequestKind::Shutdown);
                let snapshot = published_snapshot(&effects).expect("shutdown snapshot");
                assert!(snapshot.syntax().is_none());
                assert!(snapshot.definition().is_none());
                assert!(snapshot.notices().is_empty());
                actor.assert_invariants().expect("shutdown feature drain");
            }
        }
    }
}

#[test]
fn retained_early_response_payload_is_included_in_actor_accounting() {
    let (mut actor, generation, _) = ready_with_open("var a = 1;");
    let (sequence, id) = request_from_effect(
        &actor.reduce(ActorEvent::Drive),
        RequestKind::SemanticTokens,
    );
    assert_eq!(actor.writer_sequence(), Some(sequence));
    let before = actor.accounted_state_bytes();
    let message = "e".repeat(4 * 1024);
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: error_response(id, &message),
    });
    let after = actor.accounted_state_bytes();
    assert!(after >= before + message.len());
    actor
        .assert_invariants()
        .expect("retained response remains within actor budget");
}

#[test]
fn sync_write_exhaustion_does_not_advance_lsp_version() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_write: u64::MAX - 1,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    assert_eq!(actor.test_next_lsp_version(), Some(1));
    let effects = actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::CounterExhausted,
            ..
        })
    ));
    assert_eq!(
        actor.test_next_lsp_version(),
        None,
        "active state is cleaned"
    );
    assert_eq!(actor.test_counter_state().1, None);
}

#[test]
fn definition_and_followup_notice_allocate_atomically() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_notice: u64::MAX - 1,
        next_definition_result: 41,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;\nprint a;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    open_current(&mut actor, generation);
    let fence = actor.written_fence().expect("written").clone();
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(
            &fence.uri,
            Some(fence.lsp_version.get()),
            json!([{"message":"invalid"}]),
        ),
    });
    let before = actor.test_counter_state().2;
    let ui_fence = UiDocumentFence {
        process_generation: generation,
        stamp: fence.stamp,
        lsp_version: Some(fence.lsp_version),
    };
    let caret = CaretGeneration::from_raw(1).expect("caret");
    actor.reduce(ActorEvent::CaretChanged {
        fence: ui_fence,
        caret_generation: caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: caret,
        navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
        primary_character: 17,
    });
    let (sequence, id) =
        request_from_effect(&actor.reduce(ActorEvent::Drive), RequestKind::Definition);
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(id, Value::Null),
    });
    assert!(actor.is_terminal());
    assert_eq!(actor.test_counter_state().2, before);
}

#[test]
fn bounded_fallback_does_not_mark_hidden_definition_as_published() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_definition_result: 41,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;\nprint a;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    open_current(&mut actor, generation);
    let fence = actor.written_fence().expect("written").clone();
    let ui_fence = UiDocumentFence {
        process_generation: generation,
        stamp: fence.stamp,
        lsp_version: Some(fence.lsp_version),
    };
    let caret = CaretGeneration::from_raw(1).expect("caret");
    actor.reduce(ActorEvent::CaretChanged {
        fence: ui_fence,
        caret_generation: caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: caret,
        navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
        primary_character: 17,
    });
    let (sequence, id) =
        request_from_effect(&actor.reduce(ActorEvent::Drive), RequestKind::Definition);
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(
            id,
            json!([{
                "uri":fence.uri,
                "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}}
            }]),
        ),
    });

    let huge_items = Value::Array(
        (0..MAX_ACCEPTED_DIAGNOSTICS)
            .map(|_| {
                json!({
                    "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                    "severity":1,
                    "message":"x".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES)
                })
            })
            .collect(),
    );
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(&fence.uri, Some(fence.lsp_version.get()), huge_items),
    });
    let fallback_effects = actor.reduce(ActorEvent::Drive);
    let fallback = published_snapshot(&fallback_effects).expect("bounded fallback");
    assert_eq!(fallback.status(), LanguageStatus::Limited);
    assert!(fallback.definition().is_none());
    let hidden_id = DefinitionResultId::from_raw(41).expect("known seeded result");
    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: fallback.revision(),
        definition: Some(hidden_id),
        notices: Arc::from([]),
    });

    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(&fence.uri, Some(fence.lsp_version.get()), json!([])),
    });
    let revealed = actor.reduce(ActorEvent::Drive);
    assert_eq!(
        published_snapshot(&revealed)
            .and_then(LanguageSnapshot::definition)
            .map(|definition| definition.id),
        Some(hidden_id)
    );
}

#[test]
fn old_and_new_maximum_indexes_are_preflighted_before_sync_frame_commit() {
    let first_source = "a".repeat(MAX_SOURCE_BYTES);
    let second_source = "b".repeat(MAX_SOURCE_BYTES);
    let mut actor = new_actor();
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, &first_source)),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    open_current(&mut actor, generation);
    assert!(actor.accounted_state_bytes() < MAX_ACTOR_STATE_BYTES);
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, &second_source)),
    });
    let write_before = actor.test_counter_state().1;
    let effects = actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            generation: cleaned,
            cause: CleanupCause::ActorBudget,
            ..
        }) if *cleaned == generation
    ));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, ActorEffect::SendFrame { .. }))
    );
    assert_eq!(actor.test_counter_state().1, write_before);
    assert!(actor.is_terminal());
}

#[test]
fn stale_ui_fence_cannot_recreate_definition_intent_after_desired_edit() {
    let (mut actor, generation, old_fence) = ready_with_open("var a = 1;\nprint a;");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, "var b = 2;\nprint b;")),
    });
    let stale_ui_fence = UiDocumentFence {
        process_generation: generation,
        stamp: old_fence.stamp,
        lsp_version: Some(old_fence.lsp_version),
    };
    let caret = CaretGeneration::from_raw(1).expect("caret");
    actor.reduce(ActorEvent::CaretChanged {
        fence: stale_ui_fence,
        caret_generation: caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: stale_ui_fence,
        caret_generation: caret,
        navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
        primary_character: 17,
    });
    let (_, change_sequence, FrameKind::DidChange { fence }, _) =
        send(&actor.reduce(ActorEvent::Drive))
    else {
        panic!("expected desired change")
    };
    assert_eq!(fence.stamp, stamp(1, 2));
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: change_sequence,
        outcome: WriterOutcome::Flushed,
    });
    let (_, _) = request_from_effect(
        &actor.reduce(ActorEvent::Drive),
        RequestKind::SemanticTokens,
    );
    actor.assert_invariants().expect("stale UI fence ignored");
}

#[test]
fn lsp_version_exhaustion_restarts_at_one_only_after_exact_finalization() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_lsp_version: i32::MAX,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let first_generation = launch(&mut actor);
    make_ready(&mut actor, first_generation);
    let (_, first_version) = open_current(&mut actor, first_generation);
    assert_eq!(first_version.get(), i32::MAX);
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, "var a = 2;")),
    });
    let exhausted = actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&exhausted),
        Some(ActorEffect::BeginCleanup {
            generation,
            cause: CleanupCause::LspVersionExhausted,
            ..
        }) if *generation == first_generation
    ));
    actor.reduce(ActorEvent::FinalizedGeneration {
        generation: first_generation,
        stderr_tail: None,
    });
    actor.reduce(ActorEvent::Tick {
        now: ManualTick::from_raw(BACKOFF_INITIAL_MS),
    });
    let launch_effects = actor.reduce(ActorEvent::Drive);
    let ActorEffect::LaunchGeneration {
        generation: second_generation,
    } = io_effect(&launch_effects).expect("second launch")
    else {
        panic!("expected second launch")
    };
    let second_generation = *second_generation;
    assert!(second_generation > first_generation);
    make_ready(&mut actor, second_generation);
    let (_, restarted_version) = open_current(&mut actor, second_generation);
    assert_eq!(restarted_version.get(), 1);
}

#[test]
fn caret_generation_exhaustion_disables_definition_until_a_fresh_generation() {
    let (mut actor, first_generation, first_fence) = ready_with_open("var a = 1;\nprint a;");
    let first_ui_fence = UiDocumentFence {
        process_generation: first_generation,
        stamp: first_fence.stamp,
        lsp_version: Some(first_fence.lsp_version),
    };
    actor.reduce(ActorEvent::CaretGenerationExhausted {
        fence: first_ui_fence,
    });
    let max_caret = CaretGeneration::from_raw(u64::MAX).expect("last caret");
    actor.reduce(ActorEvent::CaretChanged {
        fence: first_ui_fence,
        caret_generation: max_caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: first_ui_fence,
        caret_generation: max_caret,
        navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
        primary_character: 17,
    });
    let (semantic_sequence, semantic_id) = request_from_effect(
        &actor.reduce(ActorEvent::Drive),
        RequestKind::SemanticTokens,
    );
    actor.reduce(ActorEvent::WriterFinished {
        generation: first_generation,
        sequence: semantic_sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::ReaderMessage {
        generation: first_generation,
        message: response(semantic_id, Value::Null),
    });
    let cleanup = actor.reduce(ActorEvent::ReaderFatal {
        generation: first_generation,
        cause: ReaderFatalCause::Io,
    });
    assert!(matches!(
        io_effect(&cleanup),
        Some(ActorEffect::BeginCleanup { .. })
    ));
    actor.reduce(ActorEvent::FinalizedGeneration {
        generation: first_generation,
        stderr_tail: None,
    });
    actor.reduce(ActorEvent::Tick {
        now: ManualTick::from_raw(BACKOFF_INITIAL_MS),
    });
    let second_launch = actor.reduce(ActorEvent::Drive);
    let ActorEffect::LaunchGeneration {
        generation: second_generation,
    } = io_effect(&second_launch).expect("second launch")
    else {
        panic!("expected second launch")
    };
    let second_generation = *second_generation;
    make_ready(&mut actor, second_generation);
    open_current(&mut actor, second_generation);
    let second_fence = actor.written_fence().expect("second written").clone();
    let second_ui_fence = UiDocumentFence {
        process_generation: second_generation,
        stamp: second_fence.stamp,
        lsp_version: Some(second_fence.lsp_version),
    };
    let caret = CaretGeneration::from_raw(1).expect("fresh caret");
    actor.reduce(ActorEvent::CaretChanged {
        fence: second_ui_fence,
        caret_generation: caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: second_ui_fence,
        caret_generation: caret,
        navigation_generation: NavigationGeneration::from_raw(2).expect("navigation"),
        primary_character: 17,
    });
    let (_, _) = request_from_effect(&actor.reduce(ActorEvent::Drive), RequestKind::Definition);
}

#[test]
fn aggregate_budget_boundaries_are_transactional_for_sideband_ingress() {
    for extra in [0usize, 1] {
        let (mut actor, generation) = idle_active_actor();
        let base = actor.accounted_state_bytes();
        actor.test_set_accounting_bias(
            MAX_ACTOR_STATE_BYTES
                .checked_sub(base + 1)
                .expect("stderr fixture fits")
                + extra,
        );
        let effects = actor.reduce(ActorEvent::StderrTailChanged {
            generation,
            tail: BoundedStderrTail {
                text: Arc::from("x"),
                line_count: 1,
                truncated: false,
            },
        });
        if extra == 0 {
            assert!(effects.is_empty());
            assert_eq!(actor.accounted_state_bytes(), MAX_ACTOR_STATE_BYTES);
        } else {
            assert!(matches!(
                io_effect(&effects),
                Some(ActorEffect::BeginCleanup {
                    cause: CleanupCause::ActorBudget,
                    ..
                })
            ));
            assert!(actor.is_terminal());
        }
    }

    let server_message = || {
        rpc(json!({
            "jsonrpc":"2.0",
            "id":"budget-reply",
            "method":"server/unsupported",
            "params":{}
        }))
    };
    let (mut calibration, generation) = idle_active_actor();
    let before = calibration.accounted_state_bytes();
    calibration.reduce(ActorEvent::ReaderMessage {
        generation,
        message: server_message(),
    });
    let server_delta = calibration.accounted_state_bytes() - before;
    assert!(server_delta > 0);
    for extra in [0usize, 1] {
        let (mut actor, generation) = idle_active_actor();
        let base = actor.accounted_state_bytes();
        actor.test_set_accounting_bias(
            MAX_ACTOR_STATE_BYTES
                .checked_sub(base + server_delta)
                .expect("server fixture fits")
                + extra,
        );
        let effects = actor.reduce(ActorEvent::ReaderMessage {
            generation,
            message: server_message(),
        });
        if extra == 0 {
            assert!(effects.is_empty());
            assert_eq!(actor.accounted_state_bytes(), MAX_ACTOR_STATE_BYTES);
        } else {
            assert!(matches!(
                io_effect(&effects),
                Some(ActorEffect::BeginCleanup {
                    cause: CleanupCause::ActorBudget,
                    ..
                })
            ));
            assert!(actor.is_terminal());
        }
    }

    let replacement = || Some(desired(1, 3, "var replacement = 3;"));
    let (mut calibration, _) = actor_with_change_flight();
    let before = calibration.accounted_state_bytes();
    calibration.reduce(ActorEvent::DesiredDocumentChanged {
        desired: replacement(),
    });
    let desired_delta = calibration.accounted_state_bytes() - before;
    assert!(desired_delta > 0);
    for extra in [0usize, 1] {
        let (mut actor, generation) = actor_with_change_flight();
        let base = actor.accounted_state_bytes();
        actor.test_set_accounting_bias(
            MAX_ACTOR_STATE_BYTES
                .checked_sub(base + desired_delta)
                .expect("desired fixture fits")
                + extra,
        );
        let effects = actor.reduce(ActorEvent::DesiredDocumentChanged {
            desired: replacement(),
        });
        if extra == 0 {
            assert!(effects.is_empty());
            assert_eq!(actor.accounted_state_bytes(), MAX_ACTOR_STATE_BYTES);
        } else {
            assert!(matches!(
                io_effect(&effects),
                Some(ActorEffect::BeginCleanup {
                    generation: cleaned,
                    cause: CleanupCause::ActorBudget,
                    ..
                }) if *cleaned == generation
            ));
            let snapshot_effects = actor.reduce(ActorEvent::Drive);
            assert_eq!(
                published_snapshot(&snapshot_effects).and_then(LanguageSnapshot::desired_stamp),
                Some(stamp(1, 2)),
                "failed replacement must leave durable desired unchanged"
            );
        }
    }
}

#[test]
fn empty_diagnostics_do_not_allocate_an_exhausted_item_counter() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_diagnostic_revision: 41,
        next_diagnostic_item: u64::MAX,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    open_current(&mut actor, generation);
    let fence = actor.written_fence().expect("written").clone();
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(
            &fence.uri,
            Some(fence.lsp_version.get()),
            json!([{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                "message":"last item"
            }]),
        ),
    });
    assert_eq!(actor.test_diagnostic_counters(), (Some(42), None));
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(&fence.uri, Some(fence.lsp_version.get()), json!([])),
    });
    assert!(!actor.is_terminal());
    assert_eq!(actor.test_diagnostic_counters(), (Some(43), None));
    let published = actor.reduce(ActorEvent::Drive);
    assert!(
        published_snapshot(&published)
            .and_then(LanguageSnapshot::diagnostics)
            .is_some_and(|diagnostics| diagnostics.items.is_empty())
    );
}

#[test]
fn diagnostic_set_and_item_allocation_is_atomic_at_the_last_item_id() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_diagnostic_revision: 41,
        next_diagnostic_item: u64::MAX,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    open_current(&mut actor, generation);
    let fence = actor.written_fence().expect("written").clone();
    let item = || {
        json!({
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
            "message":"item"
        })
    };
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(
            &fence.uri,
            Some(fence.lsp_version.get()),
            json!([item(), item()]),
        ),
    });
    assert!(actor.is_terminal());
    assert_eq!(actor.test_diagnostic_counters(), (Some(41), Some(u64::MAX)));
}

#[test]
fn request_and_write_counter_failures_do_not_partially_consume_the_other_counter() {
    let mut request_actor = LanguageActor::new(ActorSeeds {
        next_request: i32::MAX,
        next_write: 100,
        ..ActorSeeds::default()
    })
    .expect("actor");
    request_actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let generation = launch(&mut request_actor);
    make_ready(&mut request_actor, generation);
    open_current(&mut request_actor, generation);
    let before = request_actor.test_counter_state();
    let effects = request_actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::CounterExhausted,
            ..
        })
    ));
    assert_eq!(request_actor.test_counter_state().1, before.1);

    let mut write_actor = LanguageActor::new(ActorSeeds {
        next_request: 100,
        next_write: u64::MAX,
        ..ActorSeeds::default()
    })
    .expect("actor");
    let generation = launch(&mut write_actor);
    let (sequence, id) = initialize(&mut write_actor, generation);
    write_actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: valid_initialize(id.get()),
    });
    write_actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    let before = write_actor.test_counter_state();
    let effects = write_actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::CounterExhausted,
            ..
        })
    ));
    assert_eq!(write_actor.test_counter_state().0, before.0);
}

#[test]
fn ordinary_notice_uses_last_id_then_fails_without_partial_notice() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_notice: u64::MAX - 1,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let generation = launch(&mut actor);
    make_ready(&mut actor, generation);
    open_current(&mut actor, generation);
    let fence = actor.written_fence().expect("written").clone();
    let invalid = || {
        diagnostics(
            &fence.uri,
            Some(fence.lsp_version.get()),
            json!([{"message":"invalid"}]),
        )
    };
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: invalid(),
    });
    assert_eq!(actor.test_next_notice(), None);
    let first = actor.reduce(ActorEvent::Drive);
    assert_eq!(
        published_snapshot(&first)
            .expect("notice snapshot")
            .notices()
            .len(),
        1
    );
    let effects = actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: invalid(),
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::CounterExhausted,
            ..
        })
    ));
    assert!(actor.is_terminal());
}

#[test]
fn launch_timeout_cancellation_retries_but_shutdown_cancellation_does_not() {
    for outcome in [
        LaunchOutcome::FailedBeforeOwnership {
            cause: LaunchFailure::MissingSibling,
        },
        LaunchOutcome::Ready,
    ] {
        let mut actor = new_actor();
        let generation = launch(&mut actor);
        let cancelled = actor.reduce(ActorEvent::Tick {
            now: ManualTick::from_raw(LAUNCH_TIMEOUT_MS),
        });
        assert!(matches!(
            io_effect(&cancelled),
            Some(ActorEffect::CancelLaunch { generation: value }) if *value == generation
        ));
        assert!(
            actor
                .reduce(ActorEvent::Tick {
                    now: ManualTick::from_raw(LAUNCH_TIMEOUT_MS + 1),
                })
                .is_empty()
        );
        let finished = actor.reduce(ActorEvent::LaunchFinished {
            generation,
            outcome,
        });
        if outcome == LaunchOutcome::Ready {
            assert!(matches!(
                io_effect(&finished),
                Some(ActorEffect::BeginCleanup {
                    cause: CleanupCause::Launch,
                    ..
                })
            ));
            actor.reduce(ActorEvent::FinalizedGeneration {
                generation,
                stderr_tail: None,
            });
        }
        assert!(!actor.is_terminal());
        actor.reduce(ActorEvent::Tick {
            now: ManualTick::from_raw(LAUNCH_TIMEOUT_MS + 1 + BACKOFF_INITIAL_MS),
        });
        assert!(matches!(
            io_effect(&actor.reduce(ActorEvent::Drive)),
            Some(ActorEffect::LaunchGeneration { generation: next }) if *next > generation
        ));
    }

    let mut actor = new_actor();
    let generation = launch(&mut actor);
    assert!(matches!(
        io_effect(&actor.reduce(ActorEvent::ShutdownRequested)),
        Some(ActorEffect::CancelLaunch { generation: value }) if *value == generation
    ));
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::FailedBeforeOwnership {
            cause: LaunchFailure::MissingSibling,
        },
    });
    assert!(actor.is_terminal());
    assert!(
        actor
            .reduce(ActorEvent::Drive)
            .iter()
            .all(|effect| { matches!(effect, ActorEffect::PublishSnapshot { .. }) })
    );

    let mut shutdown_actor = new_actor();
    let generation = launch(&mut shutdown_actor);
    make_ready(&mut shutdown_actor, generation);
    assert!(
        shutdown_actor
            .reduce(ActorEvent::ShutdownRequested)
            .is_empty()
    );
    let effects = shutdown_actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::FailedBeforeOwnership {
            cause: LaunchFailure::Spawn,
        },
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            generation: cleaned,
            cause: CleanupCause::AdapterInvariant,
            ..
        }) if *cleaned == generation
    ));
    assert_eq!(shutdown_actor.cleanup_pending(), Some(generation));
    assert!(shutdown_actor.is_terminal());
}

#[test]
fn newer_definition_waits_while_the_old_same_kind_response_is_pending() {
    let (mut actor, generation, fence) = ready_with_open("var a = 1;\nprint a;");
    let ui_fence = UiDocumentFence {
        process_generation: generation,
        stamp: fence.stamp,
        lsp_version: Some(fence.lsp_version),
    };
    let first_caret = CaretGeneration::from_raw(1).expect("caret");
    actor.reduce(ActorEvent::CaretChanged {
        fence: ui_fence,
        caret_generation: first_caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: first_caret,
        navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
        primary_character: 17,
    });
    let (first_sequence, first_id) =
        request_from_effect(&actor.reduce(ActorEvent::Drive), RequestKind::Definition);
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: first_sequence,
        outcome: WriterOutcome::Flushed,
    });

    let second_caret = CaretGeneration::from_raw(2).expect("caret");
    actor.reduce(ActorEvent::CaretChanged {
        fence: ui_fence,
        caret_generation: second_caret,
    });
    actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: second_caret,
        navigation_generation: NavigationGeneration::from_raw(2).expect("navigation"),
        primary_character: 17,
    });
    let waiting = actor.reduce(ActorEvent::Drive);
    assert!(io_effect(&waiting).is_none());
    assert!(!actor.is_terminal());
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(first_id, Value::Null),
    });
    let (_, _) = request_from_effect(&actor.reduce(ActorEvent::Drive), RequestKind::Definition);
}

#[test]
fn newer_semantic_intent_waits_while_the_old_same_kind_response_is_pending() {
    let (mut actor, generation, _) = ready_with_open("var a = 1;");
    let (first_sequence, first_id) = request_from_effect(
        &actor.reduce(ActorEvent::Drive),
        RequestKind::SemanticTokens,
    );
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: first_sequence,
        outcome: WriterOutcome::Flushed,
    });
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 2, "var b = 2;")),
    });
    let (_, change_sequence, FrameKind::DidChange { .. }, _) =
        send(&actor.reduce(ActorEvent::Drive))
    else {
        panic!("expected change")
    };
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence: change_sequence,
        outcome: WriterOutcome::Flushed,
    });
    let waiting = actor.reduce(ActorEvent::Drive);
    assert!(io_effect(&waiting).is_none());
    assert!(!actor.is_terminal());
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(first_id, Value::Null),
    });
    let (_, _) = request_from_effect(
        &actor.reduce(ActorEvent::Drive),
        RequestKind::SemanticTokens,
    );
}

#[test]
fn actor_specific_snapshot_corruption_fails_before_publication() {
    let (mut definition_actor, generation, fence) = ready_with_open("var a = 1;\nprint a;");
    let ui_fence = UiDocumentFence {
        process_generation: generation,
        stamp: fence.stamp,
        lsp_version: Some(fence.lsp_version),
    };
    let caret = CaretGeneration::from_raw(1).expect("caret");
    definition_actor.reduce(ActorEvent::CaretChanged {
        fence: ui_fence,
        caret_generation: caret,
    });
    definition_actor.reduce(ActorEvent::DefinitionRequested {
        fence: ui_fence,
        caret_generation: caret,
        navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
        primary_character: 17,
    });
    let (sequence, id) = request_from_effect(
        &definition_actor.reduce(ActorEvent::Drive),
        RequestKind::Definition,
    );
    definition_actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    definition_actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: response(
            id,
            json!([{
                "uri":fence.uri,
                "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}}
            }]),
        ),
    });
    definition_actor.test_corrupt_definition_caret_fence();
    let effects = definition_actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            cause: CleanupCause::AdapterInvariant,
            ..
        })
    ));
    let snapshot = published_snapshot(&effects).expect("terminal snapshot");
    assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    assert!(snapshot.definition().is_none());

    let (mut desired_actor, generation, _) = ready_with_open("var a = 1;");
    desired_actor.test_corrupt_current_desired_source();
    let effects = desired_actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            generation: cleaned,
            cause: CleanupCause::AdapterInvariant,
            ..
        }) if *cleaned == generation
    ));
    let snapshot = published_snapshot(&effects).expect("terminal snapshot");
    assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    assert_ne!(snapshot.status(), LanguageStatus::Limited);
}

fn actor_with_visible_diagnostics() -> (LanguageActor, ProcessGeneration) {
    let (mut actor, generation, fence) = ready_with_open("var a = 1;");
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: diagnostics(
            &fence.uri,
            Some(fence.lsp_version.get()),
            json!([{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
                "severity":1,
                "message":"fence fixture"
            }]),
        ),
    });
    assert!(
        published_snapshot(&actor.reduce(ActorEvent::Drive))
            .and_then(LanguageSnapshot::diagnostics)
            .is_some()
    );
    (actor, generation)
}

fn assert_snapshot_corruption_is_contained(
    actor: &mut LanguageActor,
    generation: ProcessGeneration,
) {
    let effects = actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            generation: cleaned,
            cause: CleanupCause::AdapterInvariant,
            ..
        }) if *cleaned == generation
    ));
    let snapshot = published_snapshot(&effects).expect("terminal snapshot");
    assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    assert!(snapshot.diagnostics().is_none());
    assert_ne!(snapshot.status(), LanguageStatus::Limited);
}

#[test]
fn snapshot_fence_mutation_matrix_never_publishes_mismatched_state() {
    for corruption in [
        SnapshotFenceCorruption::ProcessGeneration,
        SnapshotFenceCorruption::DocumentStamp,
        SnapshotFenceCorruption::LspVersion,
    ] {
        let (mut actor, generation) = actor_with_visible_diagnostics();
        actor.test_corrupt_diagnostic_fence(corruption);
        assert_snapshot_corruption_is_contained(&mut actor, generation);
    }

    let mut writer_actor = new_actor();
    let generation = launch(&mut writer_actor);
    writer_actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    let _ = send(&writer_actor.reduce(ActorEvent::Drive));
    writer_actor.test_corrupt_writer_sequence();
    assert_snapshot_corruption_is_contained(&mut writer_actor, generation);

    let (mut cleanup_actor, generation) = actor_with_visible_diagnostics();
    cleanup_actor.test_stage_orphan_diagnostics();
    let cleanup = cleanup_actor.reduce(ActorEvent::ReaderFatal {
        generation,
        cause: ReaderFatalCause::Framing,
    });
    assert!(matches!(
        io_effect(&cleanup),
        Some(ActorEffect::BeginCleanup {
            generation: cleaned,
            ..
        }) if *cleaned == generation
    ));
    let cleanup_snapshot = cleanup_actor.reduce(ActorEvent::Drive);
    assert_eq!(
        published_snapshot(&cleanup_snapshot)
            .expect("cleanup containment snapshot")
            .status(),
        LanguageStatus::Disabled
    );

    let (mut backoff_actor, generation) = actor_with_visible_diagnostics();
    backoff_actor.test_stage_orphan_diagnostics();
    backoff_actor.reduce(ActorEvent::ReaderFatal {
        generation,
        cause: ReaderFatalCause::Framing,
    });
    backoff_actor.reduce(ActorEvent::FinalizedGeneration {
        generation,
        stderr_tail: None,
    });
    let backoff_snapshot = backoff_actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&backoff_snapshot).expect("backoff containment snapshot");
    assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    assert!(snapshot.diagnostics().is_none());
}

#[test]
fn final_stderr_tail_survives_exact_finalization_and_is_generation_fenced() {
    let (mut actor, generation, _) = ready_with_open("var a = 1;");
    actor.reduce(ActorEvent::ReaderFatal {
        generation,
        cause: ReaderFatalCause::Io,
    });
    let text: Arc<str> = format!("{}\n", "x".repeat(MAX_STDERR_BYTES - 1)).into();
    actor.reduce(ActorEvent::FinalizedGeneration {
        generation,
        stderr_tail: Some(BoundedStderrTail {
            text: text.clone(),
            line_count: 1,
            truncated: true,
        }),
    });
    let effects = actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&effects).expect("final stderr snapshot");
    assert_eq!(snapshot.process_generation(), Some(generation));
    assert_eq!(
        snapshot.stderr_tail().map(|tail| tail.text.as_ref()),
        Some(text.as_ref())
    );
    assert_eq!(snapshot.stderr_tail().map(|tail| tail.line_count), Some(1));
    assert_eq!(
        snapshot.stderr_tail().map(|tail| tail.truncated),
        Some(true)
    );
    assert!(snapshot.estimated_bytes() <= crate::language::snapshot::MAX_SNAPSHOT_BYTES);
    actor.assert_invariants().expect("retained tail invariants");

    actor.reduce(ActorEvent::Tick {
        now: ManualTick::from_raw(BACKOFF_INITIAL_MS),
    });
    let next_generation_effects = actor.reduce(ActorEvent::Drive);
    assert!(matches!(
        io_effect(&next_generation_effects),
        Some(ActorEffect::LaunchGeneration { generation: next }) if *next > generation
    ));
    assert!(
        published_snapshot(&next_generation_effects)
            .expect("new generation snapshot")
            .stderr_tail()
            .is_none()
    );
}

#[test]
fn cleanup_failure_publishes_its_final_stderr_tail_without_active_artifacts() {
    let (mut actor, generation, _) = ready_with_open("var a = 1;");
    actor.reduce(ActorEvent::ReaderFatal {
        generation,
        cause: ReaderFatalCause::Io,
    });
    actor.reduce(ActorEvent::CleanupFailed {
        generation,
        cause: CleanupFailure::Reap,
        stderr_tail: Some(BoundedStderrTail {
            text: Arc::from("worker failed\nlast detail"),
            line_count: 2,
            truncated: false,
        }),
    });
    let effects = actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&effects).expect("cleanup failure snapshot");
    assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    assert_eq!(snapshot.process_generation(), Some(generation));
    let tail = snapshot.stderr_tail().expect("truthful final stderr");
    assert_eq!(tail.text.as_ref(), "worker failed\nlast detail");
    assert_eq!(tail.line_count, 2);
    assert!(!tail.truncated);
    assert!(snapshot.diagnostics().is_none());
    assert!(snapshot.syntax().is_none());
    assert!(snapshot.definition().is_none());
    actor.assert_invariants().expect("cleanup tail invariants");
}

#[test]
fn malformed_final_stderr_tail_is_rejected_before_snapshot_publication() {
    let (mut actor, generation, _) = ready_with_open("var a = 1;");
    actor.reduce(ActorEvent::ReaderFatal {
        generation,
        cause: ReaderFatalCause::Io,
    });
    actor.reduce(ActorEvent::FinalizedGeneration {
        generation,
        stderr_tail: Some(BoundedStderrTail {
            text: Arc::from("one line"),
            line_count: 2,
            truncated: false,
        }),
    });
    let effects = actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&effects).expect("terminal snapshot");
    assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    assert!(snapshot.stderr_tail().is_none());
    assert_ne!(snapshot.status(), LanguageStatus::Limited);
}

#[test]
fn next_deadline_is_the_exact_minimum_without_polling() {
    let mut actor = new_actor();
    assert_eq!(actor.next_deadline(), None);
    actor.reduce(ActorEvent::Start {
        now: ManualTick::from_raw(0),
    });
    assert_eq!(actor.next_deadline(), None);
    let generation = launch(&mut actor);
    assert_eq!(
        actor.next_deadline(),
        Some(ManualTick::from_raw(LAUNCH_TIMEOUT_MS))
    );
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    assert_eq!(actor.next_deadline(), None);
    let (_, sequence, FrameKind::ClientRequest { kind, .. }, _) =
        send(&actor.reduce(ActorEvent::Drive))
    else {
        panic!("expected initialize frame")
    };
    assert_eq!(kind, RequestKind::Initialize);
    assert_eq!(
        actor.next_deadline(),
        Some(ManualTick::from_raw(WRITE_ACK_TIMEOUT_MS))
    );
    actor.reduce(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    assert_eq!(
        actor.next_deadline(),
        Some(ManualTick::from_raw(INITIALIZE_RESPONSE_TIMEOUT_MS))
    );
}

#[test]
fn future_finalization_transfers_the_real_active_generation_to_cleanup() {
    for writer_active in [false, true] {
        let mut actor = new_actor();
        let generation = launch(&mut actor);
        if writer_active {
            actor.reduce(ActorEvent::LaunchFinished {
                generation,
                outcome: LaunchOutcome::Ready,
            });
            let (_, _, _, _) = send(&actor.reduce(ActorEvent::Drive));
        }
        let future = ProcessGeneration::from_raw(generation.get() + 1).expect("future");
        let effects = actor.reduce(ActorEvent::FinalizedGeneration {
            generation: future,
            stderr_tail: None,
        });
        assert!(matches!(
            io_effect(&effects),
            Some(ActorEffect::BeginCleanup {
                generation: cleaned,
                cause: CleanupCause::AdapterInvariant,
                ..
            }) if *cleaned == generation
        ));
        assert_eq!(actor.cleanup_pending(), Some(generation));
        assert!(actor.is_terminal());
        let snapshot_effects = actor.reduce(ActorEvent::Drive);
        let snapshot = published_snapshot(&snapshot_effects).expect("disabled snapshot");
        assert_eq!(snapshot.process_generation(), Some(generation));
        assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    }
}

#[test]
fn duplicate_launch_completion_after_ownership_cleans_the_owned_generation() {
    let mut actor = new_actor();
    let generation = launch(&mut actor);
    actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    let effects = actor.reduce(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::FailedBeforeOwnership {
            cause: LaunchFailure::Spawn,
        },
    });
    assert!(matches!(
        io_effect(&effects),
        Some(ActorEffect::BeginCleanup {
            generation: cleaned,
            cause: CleanupCause::AdapterInvariant,
            ..
        }) if *cleaned == generation
    ));
    assert!(actor.is_terminal());
    assert_eq!(actor.cleanup_pending(), Some(generation));
    assert!(
        actor
            .reduce(ActorEvent::Drive)
            .iter()
            .all(|effect| { matches!(effect, ActorEffect::PublishSnapshot { .. }) })
    );
}

#[test]
fn notice_acknowledgment_requires_actual_publication_exact_id_and_revision() {
    let (mut actor, generation, fence) = ready_with_open("var a = 1;");
    let invalid = || {
        diagnostics(
            &fence.uri,
            Some(fence.lsp_version.get()),
            json!([{"message":"invalid"}]),
        )
    };
    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: invalid(),
    });
    let first_effects = actor.reduce(ActorEvent::Drive);
    let first_snapshot = published_snapshot(&first_effects).expect("first notice snapshot");
    let first_revision = first_snapshot.revision();
    let first_id = first_snapshot.notices()[0].id;
    let stale_revision = SnapshotRevision::from_raw(first_revision.get() - 1).expect("stale");
    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: stale_revision,
        definition: None,
        notices: Arc::from([first_id]),
    });
    let unknown_id = NoticeId::from_raw(first_id.get() + 1).expect("unknown notice");
    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: first_revision,
        definition: None,
        notices: Arc::from([unknown_id]),
    });

    actor.reduce(ActorEvent::ReaderMessage {
        generation,
        message: invalid(),
    });
    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: first_revision,
        definition: None,
        notices: Arc::from([unknown_id]),
    });
    let second_effects = actor.reduce(ActorEvent::Drive);
    let second_snapshot = published_snapshot(&second_effects).expect("two notices");
    assert_eq!(second_snapshot.notices().len(), 2);
    assert!(
        second_snapshot
            .notices()
            .iter()
            .any(|notice| notice.id == first_id)
    );
    assert!(
        second_snapshot
            .notices()
            .iter()
            .any(|notice| notice.id == unknown_id)
    );

    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: first_revision,
        definition: None,
        notices: Arc::from([first_id]),
    });
    let after_first_ack = actor.reduce(ActorEvent::Drive);
    let snapshot = published_snapshot(&after_first_ack).expect("first notice removed");
    assert_eq!(snapshot.notices().len(), 1);
    assert_eq!(snapshot.notices()[0].id, unknown_id);
    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: generation,
        observed_revision: second_snapshot.revision(),
        definition: None,
        notices: Arc::from([unknown_id]),
    });
    let after_second_ack = actor.reduce(ActorEvent::Drive);
    assert!(
        published_snapshot(&after_second_ack)
            .expect("all notices removed")
            .notices()
            .is_empty()
    );
}

#[test]
fn stale_process_acknowledgment_cannot_remove_reused_limit_notice() {
    let mut actor = LanguageActor::new(ActorSeeds {
        next_generation: 2,
        ..ActorSeeds::default()
    })
    .expect("actor");
    actor.reduce(ActorEvent::DesiredDocumentChanged {
        desired: Some(desired(1, 1, "var a = 1;")),
    });
    let current_generation = launch(&mut actor);
    assert_eq!(current_generation.get(), 2);
    make_ready(&mut actor, current_generation);
    open_current(&mut actor, current_generation);
    let fence = actor.written_fence().expect("written fence").clone();

    for _ in 0..MAX_NOTICES {
        actor.reduce(ActorEvent::ReaderMessage {
            generation: current_generation,
            message: diagnostics(
                &fence.uri,
                Some(fence.lsp_version.get()),
                json!([{"message":"invalid"}]),
            ),
        });
    }
    let first_publication = actor.reduce(ActorEvent::Drive);
    let first_snapshot =
        published_snapshot(&first_publication).expect("generation-two notice snapshot");
    assert!(
        first_snapshot
            .notices()
            .iter()
            .any(|notice| notice.id == NoticeId::limit())
    );
    let observed_revision = first_snapshot.revision();

    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: ProcessGeneration::from_raw(1).expect("stale process"),
        observed_revision,
        definition: None,
        notices: Arc::from([NoticeId::limit()]),
    });
    actor.reduce(ActorEvent::StderrTailChanged {
        generation: current_generation,
        tail: BoundedStderrTail {
            text: Arc::from("current generation"),
            line_count: 1,
            truncated: false,
        },
    });
    let after_stale_ack = actor.reduce(ActorEvent::Drive);
    assert!(
        published_snapshot(&after_stale_ack)
            .expect("snapshot after stale acknowledgement")
            .notices()
            .iter()
            .any(|notice| notice.id == NoticeId::limit())
    );

    actor.reduce(ActorEvent::SnapshotItemsAcknowledged {
        process_generation: current_generation,
        observed_revision,
        definition: None,
        notices: Arc::from([NoticeId::limit()]),
    });
    let after_current_ack = actor.reduce(ActorEvent::Drive);
    assert!(
        published_snapshot(&after_current_ack)
            .expect("snapshot after current acknowledgement")
            .notices()
            .iter()
            .all(|notice| notice.id != NoticeId::limit())
    );
}

fn trace_events(actor: &LanguageActor) -> Vec<ActorEvent> {
    let mut events = Vec::with_capacity(9);
    events.push(ActorEvent::Drive);

    if actor.protocol_phase() == Some(ProtocolPhase::Launching) {
        if let Some(generation) = actor.active_generation() {
            events.push(ActorEvent::LaunchFinished {
                generation,
                outcome: LaunchOutcome::Ready,
            });
        }
    } else if let Some((generation, request)) = actor.test_pending_initialize() {
        events.push(ActorEvent::ReaderMessage {
            generation,
            message: valid_initialize(request.get()),
        });
    }

    if let (Some(generation), Some(sequence)) = (actor.active_generation(), actor.writer_sequence())
    {
        events.push(ActorEvent::WriterFinished {
            generation,
            sequence,
            outcome: WriterOutcome::Flushed,
        });
    }

    events.push(ActorEvent::DesiredDocumentChanged {
        desired: Some(actor.test_next_trace_desired()),
    });
    if let Some(event) = actor.test_next_definition_event() {
        events.push(event);
    }
    events.push(ActorEvent::ShutdownRequested);
    if let Some(generation) = actor.active_generation() {
        events.push(ActorEvent::ReaderFatal {
            generation,
            cause: ReaderFatalCause::Framing,
        });
    }
    if let Some(generation) = actor
        .cleanup_pending()
        .or_else(|| actor.active_generation())
    {
        events.push(ActorEvent::FinalizedGeneration {
            generation,
            stderr_tail: None,
        });
    }
    events.push(ActorEvent::Tick {
        now: actor.test_next_trace_tick(),
    });
    events
}

fn assert_trace_frame_contract(kind: &FrameKind, bytes: &[u8]) {
    let message = decode_frame(bytes);
    let value = message.value();
    let canonical = encode_message(value).expect("actor emitted valid JSON-RPC");
    assert_eq!(
        bytes,
        canonical.as_slice(),
        "frame must be canonical and exact"
    );
    let body = serde_json::to_vec(value).expect("serialize emitted body");
    let prefix = format!("Content-Length: {}\r\n\r\n", body.len());
    assert!(bytes.starts_with(prefix.as_bytes()), "exact frame prefix");

    let expected_method = match kind {
        FrameKind::ClientRequest { id, kind } => {
            assert_eq!(value.get("id"), Some(&Value::from(id.get())));
            match kind {
                RequestKind::Initialize => "initialize",
                RequestKind::SemanticTokens => "textDocument/semanticTokens/full",
                RequestKind::Definition => "textDocument/definition",
                RequestKind::Shutdown => "shutdown",
            }
        }
        FrameKind::Initialized => "initialized",
        FrameKind::DidClose { uri } => {
            assert_eq!(
                value.pointer("/params/textDocument/uri"),
                Some(&Value::String(uri.to_string()))
            );
            "textDocument/didClose"
        }
        FrameKind::DidOpen { fence } => {
            assert_eq!(
                value.pointer("/params/textDocument/uri"),
                Some(&Value::String(fence.uri.to_string()))
            );
            assert_eq!(
                value.pointer("/params/textDocument/version"),
                Some(&Value::from(fence.lsp_version.get()))
            );
            "textDocument/didOpen"
        }
        FrameKind::DidChange { fence } => {
            assert_eq!(
                value.pointer("/params/textDocument/uri"),
                Some(&Value::String(fence.uri.to_string()))
            );
            assert_eq!(
                value.pointer("/params/textDocument/version"),
                Some(&Value::from(fence.lsp_version.get()))
            );
            "textDocument/didChange"
        }
        FrameKind::ServerResponse { id } => {
            match id {
                RpcId::Number(id) => assert_eq!(value.get("id"), Some(&Value::from(*id))),
                RpcId::String(id) => {
                    assert_eq!(value.get("id"), Some(&Value::String(id.clone())))
                }
            }
            assert_eq!(value.pointer("/error/code"), Some(&Value::from(-32601)));
            assert!(value.get("method").is_none());
            return;
        }
        FrameKind::Exit => "exit",
    };
    assert_eq!(
        value.get("method"),
        Some(&Value::String(expected_method.to_owned()))
    );
}

#[test]
fn deterministic_depth_seven_transition_exploration_preserves_actor_contract() {
    const MAX_DEPTH: usize = 7;
    const MAX_STATES: usize = 2_048;

    let mut initial = new_actor();
    assert!(
        initial
            .reduce(ActorEvent::Start {
                now: ManualTick::from_raw(0),
            })
            .is_empty()
    );
    initial.assert_invariants().expect("initial invariants");

    let mut seen = HashSet::new();
    seen.insert(initial.test_trace_key());
    let mut queue = VecDeque::from([(initial, 0usize)]);
    let mut deepest = 0usize;
    let mut explored_edges = 0usize;

    while let Some((actor, depth)) = queue.pop_front() {
        deepest = deepest.max(depth);
        if depth == MAX_DEPTH {
            continue;
        }
        for event in trace_events(&actor) {
            let mut next = actor.clone();
            let effects = next.reduce(event);
            next.assert_invariants().expect("trace invariants");
            assert!(
                effects
                    .iter()
                    .filter(|effect| !matches!(effect, ActorEffect::PublishSnapshot { .. }))
                    .count()
                    <= 1,
                "at most one lifecycle effect per reduction"
            );
            assert!(
                effects
                    .iter()
                    .filter(|effect| matches!(effect, ActorEffect::PublishSnapshot { .. }))
                    .count()
                    <= 1,
                "at most one snapshot effect per reduction"
            );
            for effect in &effects {
                if let ActorEffect::SendFrame { kind, bytes, .. } = effect {
                    assert_trace_frame_contract(kind, bytes);
                }
            }
            explored_edges = explored_edges.checked_add(1).expect("bounded edge count");

            let key = next.test_trace_key();
            if seen.insert(key) {
                assert!(seen.len() <= MAX_STATES, "normalized trace exceeded cap");
                queue.push_back((next, depth + 1));
            }
        }
    }

    assert_eq!(deepest, MAX_DEPTH);
    assert!(explored_edges > seen.len(), "trace exercised deduplication");
    assert!(seen.len() <= MAX_STATES);
}
