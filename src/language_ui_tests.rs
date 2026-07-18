use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::language::{
    AcknowledgementBatch, CaretCommand, CaretGeneration, DefinitionIntent, DefinitionResultId,
    DefinitionSnapshot, DesiredDocument, DocumentSyncSnapshot, LanguageClosed, LanguageNotice,
    LanguageSnapshot, LanguageSnapshotDraft, LanguageStatus, LspVersion, NoticeId, NoticeKind,
    ProcessGeneration, SnapshotRevision, WriterState,
};
use crate::language_ui::{
    AnalysisRevealBatch, AnalysisRevealLatch, AnalysisRevealScope, EditorCaret, LanguagePort,
    LanguageUiState, SelectionIdentity,
};
use crate::{
    AppModel, DocumentId, FileModelEvent, ModelEffect, ModelEvent, NavigationGeneration, UiAction,
    apply_event,
};

#[derive(Default)]
struct CapturedCommands {
    documents: Vec<Option<DesiredDocument>>,
    carets: Vec<CaretCommand>,
    definitions: Vec<DefinitionIntent>,
    acknowledgements: Vec<AcknowledgementBatch>,
    shutdowns: usize,
}

struct FakeLanguagePort {
    snapshot: Arc<Mutex<Arc<LanguageSnapshot>>>,
    commands: Arc<Mutex<CapturedCommands>>,
}

type SnapshotSlot = Arc<Mutex<Arc<LanguageSnapshot>>>;
type CommandCapture = Arc<Mutex<CapturedCommands>>;
type FakePortFixture = (FakeLanguagePort, SnapshotSlot, CommandCapture);

impl LanguagePort for FakeLanguagePort {
    fn load_snapshot(&self) -> Arc<LanguageSnapshot> {
        Arc::clone(&self.snapshot.lock().expect("snapshot"))
    }

    fn submit_document(&self, desired: Option<DesiredDocument>) -> Result<(), LanguageClosed> {
        self.commands
            .lock()
            .expect("commands")
            .documents
            .push(desired);
        Ok(())
    }

    fn submit_caret(&self, command: CaretCommand) -> Result<(), LanguageClosed> {
        self.commands.lock().expect("commands").carets.push(command);
        Ok(())
    }

    fn request_definition(&self, intent: DefinitionIntent) -> Result<(), LanguageClosed> {
        self.commands
            .lock()
            .expect("commands")
            .definitions
            .push(intent);
        Ok(())
    }

    fn acknowledge_items(&self, batch: AcknowledgementBatch) -> Result<(), LanguageClosed> {
        self.commands
            .lock()
            .expect("commands")
            .acknowledgements
            .push(batch);
        Ok(())
    }

    fn request_shutdown(&self) {
        self.commands.lock().expect("commands").shutdowns += 1;
    }
}

fn fake_port(snapshot: Arc<LanguageSnapshot>) -> FakePortFixture {
    let snapshot = Arc::new(Mutex::new(snapshot));
    let commands = Arc::new(Mutex::new(CapturedCommands::default()));
    (
        FakeLanguagePort {
            snapshot: Arc::clone(&snapshot),
            commands: Arc::clone(&commands),
        },
        snapshot,
        commands,
    )
}

fn generation_snapshot(generation: u64) -> Arc<LanguageSnapshot> {
    Arc::new(LanguageSnapshot::bounded(LanguageSnapshotDraft {
        revision: SnapshotRevision::from_raw(generation).expect("snapshot revision"),
        process_generation: Some(ProcessGeneration::from_raw(generation).expect("generation")),
        status: LanguageStatus::Ready,
        desired_document: None,
        written_document: None,
        diagnostics: None,
        syntax: None,
        definition: None,
        notices: Vec::new(),
        stderr_tail: None,
        writer: WriterState::Idle,
    }))
}

fn written_snapshot(generation: u64, stamp: crate::DocumentStamp) -> Arc<LanguageSnapshot> {
    let uri: Arc<str> = Arc::from(format!(
        "oxide-document://local/{}.ox",
        stamp.document_id.get()
    ));
    let document = DocumentSyncSnapshot {
        stamp,
        uri,
        lsp_version: Some(LspVersion::from_raw(1).expect("LSP version")),
    };
    Arc::new(LanguageSnapshot::bounded(LanguageSnapshotDraft {
        revision: SnapshotRevision::from_raw(generation).expect("snapshot revision"),
        process_generation: Some(ProcessGeneration::from_raw(generation).expect("generation")),
        status: LanguageStatus::Ready,
        desired_document: Some(document.clone()),
        written_document: Some(document),
        diagnostics: None,
        syntax: None,
        definition: None,
        notices: Vec::new(),
        stderr_tail: None,
        writer: WriterState::Idle,
    }))
}

fn snapshot_with_acknowledgeable_items(stamp: crate::DocumentStamp) -> Arc<LanguageSnapshot> {
    let uri: Arc<str> = Arc::from(format!(
        "oxide-document://local/{}.ox",
        stamp.document_id.get()
    ));
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    let version = LspVersion::from_raw(1).expect("LSP version");
    let document = DocumentSyncSnapshot {
        stamp,
        uri: Arc::clone(&uri),
        lsp_version: Some(version),
    };
    Arc::new(LanguageSnapshot::bounded(LanguageSnapshotDraft {
        revision: SnapshotRevision::from_raw(7).expect("snapshot revision"),
        process_generation: Some(generation),
        status: LanguageStatus::Ready,
        desired_document: Some(document.clone()),
        written_document: Some(document),
        diagnostics: None,
        syntax: None,
        definition: Some(DefinitionSnapshot {
            id: DefinitionResultId::from_raw(4).expect("definition"),
            process_generation: generation,
            uri,
            stamp,
            lsp_version: version,
            caret_generation: CaretGeneration::from_raw(1).expect("caret"),
            navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
            caret_character: 0,
            targets: Arc::from([]),
        }),
        notices: vec![
            LanguageNotice {
                id: NoticeId::from_raw(2).expect("notice"),
                kind: NoticeKind::Information,
                message: Arc::from("first"),
            },
            LanguageNotice {
                id: NoticeId::from_raw(3).expect("notice"),
                kind: NoticeKind::RequestError,
                message: Arc::from("second"),
            },
        ],
        stderr_tail: None,
        writer: WriterState::Idle,
    }))
}

fn notice_snapshot(generation: u64, revision: u64, ids: &[u64]) -> Arc<LanguageSnapshot> {
    let ids: Vec<NoticeId> = ids
        .iter()
        .map(|id| NoticeId::from_raw(*id).expect("notice"))
        .collect();
    notice_id_snapshot(generation, revision, &ids)
}

fn notice_id_snapshot(generation: u64, revision: u64, ids: &[NoticeId]) -> Arc<LanguageSnapshot> {
    Arc::new(LanguageSnapshot::bounded(LanguageSnapshotDraft {
        revision: SnapshotRevision::from_raw(revision).expect("snapshot revision"),
        process_generation: Some(ProcessGeneration::from_raw(generation).expect("generation")),
        status: LanguageStatus::Ready,
        desired_document: None,
        written_document: None,
        diagnostics: None,
        syntax: None,
        definition: None,
        notices: ids
            .iter()
            .map(|id| LanguageNotice {
                id: *id,
                kind: if *id == NoticeId::limit() {
                    NoticeKind::Limit
                } else {
                    NoticeKind::Information
                },
                message: Arc::from(format!("notice {}", id.get())),
            })
            .collect(),
        stderr_tail: None,
        writer: WriterState::Idle,
    }))
}

#[test]
fn document_reconciliation_reuses_the_model_arc_and_ignores_same_stamp() {
    let model = AppModel::new();
    let document = model.document().expect("initial document");
    let expected_text = document.shared_text();
    let (port, _, commands) = fake_port(crate::language_ui::unavailable_snapshot());
    let mut state = LanguageUiState::new(Box::new(port));

    assert_eq!(state.reconcile_document(Some(document)), Ok(true));
    assert_eq!(state.reconcile_document(Some(document)), Ok(false));

    let commands = commands.lock().expect("commands");
    assert_eq!(commands.documents.len(), 1);
    let submitted = commands.documents[0].as_ref().expect("open document");
    assert_eq!(submitted.stamp, document.stamp());
    assert!(Arc::ptr_eq(&submitted.text, &expected_text));
}

#[test]
fn thirty_two_edits_coalesce_to_the_latest_document_when_reconciled_once() {
    let mut model = AppModel::new();
    for edit in 0..32 {
        let stamp = model.document().expect("document").stamp();
        apply_event(
            &mut model,
            ModelEvent::Ui(UiAction::Edit {
                document: stamp,
                text: format!("var value = {edit};"),
            }),
        );
    }
    let document = model.document().expect("document");
    let expected_text = document.shared_text();
    let (port, _, commands) = fake_port(crate::language_ui::unavailable_snapshot());
    let mut state = LanguageUiState::new(Box::new(port));

    assert_eq!(state.reconcile_document(Some(document)), Ok(true));

    let commands = commands.lock().expect("commands");
    assert_eq!(commands.documents.len(), 1);
    let submitted = commands.documents[0].as_ref().expect("open document");
    assert_eq!(submitted.stamp, document.stamp());
    assert!(Arc::ptr_eq(&submitted.text, &expected_text));
}

#[test]
fn caret_generation_tracks_primary_selection_and_document_stamp() {
    let mut model = AppModel::new();
    let (port, _, commands) = fake_port(generation_snapshot(1));
    let mut state = LanguageUiState::new(Box::new(port));
    let document = model.document().expect("document");
    state
        .reconcile_document(Some(document))
        .expect("document submission");
    let stamp = document.stamp();

    let first = EditorCaret {
        stamp,
        primary_character: Some(3),
        selection: None,
    };
    assert_eq!(state.reconcile_caret(Some(first)), Ok(true));
    assert_eq!(state.reconcile_caret(Some(first)), Ok(false));
    assert_eq!(
        state.reconcile_caret(Some(EditorCaret {
            primary_character: Some(4),
            ..first
        })),
        Ok(true)
    );
    assert_eq!(
        state.reconcile_caret(Some(EditorCaret {
            primary_character: Some(4),
            selection: Some(SelectionIdentity {
                anchor_character: 1,
                focus_character: 4,
            }),
            ..first
        })),
        Ok(true)
    );

    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: "var changed = true;".to_owned(),
        }),
    );
    let edited = model.document().expect("edited document");
    state
        .reconcile_document(Some(edited))
        .expect("edited document submission");
    assert_eq!(
        state.reconcile_caret(Some(EditorCaret {
            stamp: edited.stamp(),
            primary_character: Some(4),
            selection: None,
        })),
        Ok(true)
    );

    let commands = commands.lock().expect("commands");
    let generations: Vec<u64> = commands
        .carets
        .iter()
        .map(|command| match command {
            CaretCommand::Current(intent) => intent.generation.get(),
            CaretCommand::Exhausted { .. } => panic!("counter should not exhaust"),
        })
        .collect();
    assert_eq!(generations, [1, 2, 3, 4]);
}

#[test]
fn caret_counter_exhaustion_resets_for_a_fresh_process_generation() {
    let model = AppModel::new();
    let document = model.document().expect("document");
    let stamp = document.stamp();
    let (port, snapshot_slot, commands) = fake_port(generation_snapshot(1));
    let mut state = LanguageUiState::new(Box::new(port));
    state
        .reconcile_document(Some(document))
        .expect("document submission");
    state.test_seed_caret_generation(
        CaretGeneration::from_raw(u64::MAX).expect("maximum caret generation"),
    );

    assert_eq!(
        state.reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: Some(1),
            selection: None,
        })),
        Ok(true)
    );
    assert!(state.caret_exhausted());
    assert_eq!(
        state.reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: Some(2),
            selection: None,
        })),
        Ok(false)
    );

    *snapshot_slot.lock().expect("snapshot") = generation_snapshot(2);
    state.refresh_snapshot();
    assert!(!state.caret_exhausted());
    assert_eq!(
        state.reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: Some(2),
            selection: None,
        })),
        Ok(true)
    );

    let commands = commands.lock().expect("commands");
    assert!(matches!(
        commands.carets.as_slice(),
        [CaretCommand::Exhausted { process_generation: exhausted_process, stamp: exhausted_stamp }, CaretCommand::Current(intent)]
            if *exhausted_process == ProcessGeneration::from_raw(1).expect("generation")
                && *exhausted_stamp == stamp
                && intent.process_generation == ProcessGeneration::from_raw(2).expect("generation")
                && intent.generation.get() == 1
    ));
}

#[test]
fn definition_submission_captures_and_revalidates_all_ui_fences() {
    let mut model = AppModel::new();
    let document = model.document().expect("document");
    let stamp = document.stamp();
    let (port, _, commands) = fake_port(written_snapshot(1, stamp));
    let mut state = LanguageUiState::new(Box::new(port));
    state
        .reconcile_document(Some(document))
        .expect("document submission");
    state
        .reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: Some(7),
            selection: None,
        }))
        .expect("caret submission");
    let navigation_generation = NavigationGeneration::from_raw(9).expect("navigation");
    let captured = state
        .capture_definition(navigation_generation)
        .expect("definition should be enabled");

    assert_eq!(state.submit_definition(captured.clone()), Ok(true));
    {
        let commands = commands.lock().expect("commands");
        assert_eq!(commands.definitions.len(), 1);
        assert_eq!(
            commands.definitions[0],
            DefinitionIntent {
                process_generation: ProcessGeneration::from_raw(1).expect("generation"),
                navigation_generation,
                stamp,
                caret_generation: CaretGeneration::from_raw(1).expect("caret"),
                caret_character: 7,
            }
        );
    }

    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: "var newer = 1;".to_owned(),
        }),
    );
    let edited = model.document().expect("edited");
    state
        .reconcile_document(Some(edited))
        .expect("document submission");
    state
        .reconcile_caret(Some(EditorCaret {
            stamp: edited.stamp(),
            primary_character: Some(7),
            selection: None,
        }))
        .expect("caret submission");

    assert_eq!(state.submit_definition(captured), Ok(false));
    assert_eq!(commands.lock().expect("commands").definitions.len(), 1);
}

#[test]
fn definition_is_disabled_without_a_primary_caret_or_after_exhaustion() {
    let model = AppModel::new();
    let document = model.document().expect("document");
    let stamp = document.stamp();
    let (port, _, _) = fake_port(written_snapshot(1, stamp));
    let mut state = LanguageUiState::new(Box::new(port));
    state
        .reconcile_document(Some(document))
        .expect("document submission");
    state
        .reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: None,
            selection: None,
        }))
        .expect("caret submission");
    assert!(
        state
            .capture_definition(NavigationGeneration::from_raw(1).expect("navigation"))
            .is_none()
    );

    state.test_seed_caret_generation(
        CaretGeneration::from_raw(u64::MAX).expect("maximum caret generation"),
    );
    state
        .reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: Some(1),
            selection: None,
        }))
        .expect("exhaustion submission");
    assert!(
        state
            .capture_definition(NavigationGeneration::from_raw(2).expect("navigation"))
            .is_none()
    );
}

#[test]
fn exact_definition_and_notice_acknowledgements_are_deduplicated_into_one_batch() {
    let model = AppModel::new();
    let stamp = model.document().expect("document").stamp();
    let (port, _, commands) = fake_port(snapshot_with_acknowledgeable_items(stamp));
    let mut state = LanguageUiState::new(Box::new(port));
    let definition = DefinitionResultId::from_raw(4).expect("definition");
    let first = NoticeId::from_raw(2).expect("notice");
    let second = NoticeId::from_raw(3).expect("notice");

    assert!(state.acknowledge_definition(definition));
    assert!(state.acknowledge_notice(first));
    assert!(state.acknowledge_notice(first));
    assert!(state.acknowledge_notice(second));
    assert!(!state.acknowledge_notice(NoticeId::from_raw(9).expect("unknown notice")));
    assert_eq!(state.flush_acknowledgements(), Ok(true));
    assert_eq!(state.flush_acknowledgements(), Ok(false));

    let commands = commands.lock().expect("commands");
    assert_eq!(commands.acknowledgements.len(), 1);
    assert_eq!(
        commands.acknowledgements[0],
        AcknowledgementBatch::new(
            ProcessGeneration::from_raw(1).expect("generation"),
            SnapshotRevision::from_raw(7).expect("revision"),
            Some(definition),
            Arc::from([first, second]),
        )
        .expect("bounded batch")
    );
}

#[test]
fn reused_limit_notice_is_acknowledged_only_in_its_exact_process_generation() {
    let limit = NoticeId::limit();
    let ordinary = NoticeId::from_raw(2).expect("notice");
    let (port, snapshot_slot, commands) = fake_port(notice_id_snapshot(1, 1, &[limit]));
    let mut state = LanguageUiState::new(Box::new(port));
    assert!(state.acknowledge_notice(limit));
    assert_eq!(state.flush_acknowledgements(), Ok(true));

    *snapshot_slot.lock().expect("snapshot") = notice_id_snapshot(2, 2, &[limit, ordinary]);
    state.refresh_snapshot();
    assert!(state.acknowledge_notice(ordinary));
    assert_eq!(state.flush_acknowledgements(), Ok(true));

    {
        let commands = commands.lock().expect("commands");
        assert_eq!(commands.acknowledgements.len(), 2);
        assert_eq!(
            commands.acknowledgements[0].process_generation,
            ProcessGeneration::from_raw(1).expect("generation")
        );
        assert_eq!(commands.acknowledgements[0].notices.as_ref(), [limit]);
        assert_eq!(
            commands.acknowledgements[1].process_generation,
            ProcessGeneration::from_raw(2).expect("generation")
        );
        assert_eq!(commands.acknowledgements[1].notices.as_ref(), [ordinary]);
    }

    assert!(state.acknowledge_notice(limit));
    assert_eq!(state.flush_acknowledgements(), Ok(true));
    let commands = commands.lock().expect("commands");
    assert_eq!(commands.acknowledgements.len(), 3);
    assert_eq!(
        commands.acknowledgements[2].process_generation,
        ProcessGeneration::from_raw(2).expect("generation")
    );
    assert_eq!(commands.acknowledgements[2].notices.as_ref(), [limit]);
}

#[test]
fn analysis_reveal_latch_ignores_stale_batches_and_resets_only_on_scoped_events() {
    let document = DocumentId::from_raw(1).expect("document");
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    let scope = AnalysisRevealScope {
        document,
        process_generation: generation,
    };
    let mut latch = AnalysisRevealLatch::default();
    let nonempty = |revision| AnalysisRevealBatch {
        scope,
        revision: crate::language::DiagnosticSetRevision::from_raw(revision)
            .expect("diagnostic revision"),
        item_count: 1,
    };

    assert!(latch.observe(Some(scope), Some(nonempty(1))));
    assert!(!latch.observe(Some(scope), None));
    assert!(!latch.observe(Some(scope), Some(nonempty(2))));

    let stale_scope = AnalysisRevealScope {
        document,
        process_generation: ProcessGeneration::from_raw(2).expect("generation"),
    };
    assert!(!latch.observe(
        Some(scope),
        Some(AnalysisRevealBatch {
            scope: stale_scope,
            revision:
                crate::language::DiagnosticSetRevision::from_raw(3).expect("diagnostic revision"),
            item_count: 0,
        })
    ));
    assert!(!latch.observe(Some(scope), Some(nonempty(4))));

    assert!(!latch.observe(
        Some(scope),
        Some(AnalysisRevealBatch {
            scope,
            revision:
                crate::language::DiagnosticSetRevision::from_raw(5).expect("diagnostic revision"),
            item_count: 0,
        })
    ));
    assert!(latch.observe(Some(scope), Some(nonempty(6))));
    assert!(!latch.observe(None, None));
    assert!(latch.observe(Some(scope), Some(nonempty(7))));
}

#[test]
fn analysis_reveal_latch_reveals_again_for_replacement_or_fresh_generation() {
    let mut latch = AnalysisRevealLatch::default();
    let first_scope = AnalysisRevealScope {
        document: DocumentId::from_raw(1).expect("document"),
        process_generation: ProcessGeneration::from_raw(1).expect("generation"),
    };
    let batch = |scope, revision| AnalysisRevealBatch {
        scope,
        revision: crate::language::DiagnosticSetRevision::from_raw(revision)
            .expect("diagnostic revision"),
        item_count: 2,
    };
    assert!(latch.observe(Some(first_scope), Some(batch(first_scope, 1))));

    let fresh_generation = AnalysisRevealScope {
        process_generation: ProcessGeneration::from_raw(2).expect("generation"),
        ..first_scope
    };
    assert!(latch.observe(Some(fresh_generation), Some(batch(fresh_generation, 1))));

    let replacement = AnalysisRevealScope {
        document: DocumentId::from_raw(2).expect("document"),
        ..fresh_generation
    };
    assert!(latch.observe(Some(replacement), Some(batch(replacement, 1))));
}

#[test]
fn unavailable_port_is_inert_and_keeps_one_stable_snapshot() {
    let model = AppModel::new();
    let mut state = LanguageUiState::unavailable();
    let initial = Arc::clone(state.snapshot());

    assert_eq!(initial.status(), LanguageStatus::Unavailable);
    assert_eq!(state.reconcile_document(model.document()), Ok(true));
    assert_eq!(state.reconcile_document(model.document()), Ok(false));
    assert!(Arc::ptr_eq(&initial, &state.refresh_snapshot()));
    assert!(!state.definition_available());
    assert_eq!(state.flush_acknowledgements(), Ok(false));
    state.request_shutdown();
    state.request_shutdown();
}

#[test]
fn language_shutdown_is_signal_only_and_idempotent_across_drop() {
    let (port, _, commands) = fake_port(crate::language_ui::unavailable_snapshot());
    {
        let mut state = LanguageUiState::new(Box::new(port));
        state.request_shutdown();
        state.request_shutdown();
    }

    assert_eq!(commands.lock().expect("commands").shutdowns, 1);
}

#[test]
fn native_coordinator_implements_the_object_safe_language_port() {
    fn assert_language_port<T: LanguagePort>() {}
    assert_language_port::<crate::language::LanguageCoordinator>();
}

#[test]
fn acknowledgement_accumulator_stays_bounded_and_prefers_latest_notice_ids() {
    let initial_ids: Vec<u64> = (1..=32).collect();
    let (port, snapshot_slot, commands) = fake_port(notice_snapshot(1, 1, &initial_ids));
    let mut state = LanguageUiState::new(Box::new(port));
    for id in &initial_ids {
        assert!(state.acknowledge_notice(NoticeId::from_raw(*id).expect("notice")));
    }

    *snapshot_slot.lock().expect("snapshot") = notice_snapshot(1, 2, &[33]);
    state.refresh_snapshot();
    assert!(state.acknowledge_notice(NoticeId::from_raw(33).expect("notice")));
    assert_eq!(state.flush_acknowledgements(), Ok(true));

    let commands = commands.lock().expect("commands");
    let notices = &commands.acknowledgements[0].notices;
    assert_eq!(notices.len(), 32);
    assert_eq!(notices[0].get(), 2);
    assert_eq!(notices[31].get(), 33);
}

#[test]
fn fresh_process_generation_discards_unsubmitted_old_acknowledgements() {
    let (port, snapshot_slot, commands) = fake_port(notice_snapshot(1, 1, &[1]));
    let mut state = LanguageUiState::new(Box::new(port));
    assert!(state.acknowledge_notice(NoticeId::from_raw(1).expect("notice")));

    *snapshot_slot.lock().expect("snapshot") = notice_snapshot(2, 2, &[2]);
    state.refresh_snapshot();
    assert!(state.acknowledge_notice(NoticeId::from_raw(2).expect("notice")));
    assert_eq!(state.flush_acknowledgements(), Ok(true));

    let commands = commands.lock().expect("commands");
    assert_eq!(commands.acknowledgements.len(), 1);
    assert_eq!(
        commands.acknowledgements[0].process_generation,
        ProcessGeneration::from_raw(2).expect("generation")
    );
    assert_eq!(
        commands.acknowledgements[0].notices.as_ref(),
        [NoticeId::from_raw(2).expect("notice")]
    );
}

#[test]
fn save_as_path_change_does_not_resubmit_an_unchanged_document_stamp() {
    let mut model = AppModel::new();
    let initial_stamp = model.document().expect("document").stamp();
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: initial_stamp,
            text: "print 1;".to_owned(),
        }),
    );
    let (port, _, commands) = fake_port(crate::language_ui::unavailable_snapshot());
    let mut state = LanguageUiState::new(Box::new(port));
    assert_eq!(state.reconcile_document(model.document()), Ok(true));
    let stamp_before_save = model.document().expect("document").stamp();

    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::SaveAs));
    let [ModelEffect::PickSaveAs { operation_id, .. }] = effects.as_slice() else {
        panic!("expected save picker");
    };
    let operation_id = *operation_id;
    let effects = apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::SavePicked {
            operation_id,
            path: Some(PathBuf::from("saved.ox")),
        }),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::WriteFile { .. }]
    ));
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::WriteFinished {
            operation_id,
            result: Ok(()),
        }),
    );

    assert_eq!(
        model.document().expect("document").stamp(),
        stamp_before_save
    );
    assert_eq!(state.reconcile_document(model.document()), Ok(false));
    assert_eq!(commands.lock().expect("commands").documents.len(), 1);
}

#[test]
fn replacement_and_close_each_submit_once() {
    let mut model = AppModel::new();
    let (port, _, commands) = fake_port(crate::language_ui::unavailable_snapshot());
    let mut state = LanguageUiState::new(Box::new(port));
    assert_eq!(state.reconcile_document(model.document()), Ok(true));

    apply_event(&mut model, ModelEvent::Ui(UiAction::New));
    assert_eq!(state.reconcile_document(model.document()), Ok(true));
    apply_event(&mut model, ModelEvent::Ui(UiAction::CloseDocument));
    assert_eq!(state.reconcile_document(model.document()), Ok(true));
    assert_eq!(state.reconcile_document(model.document()), Ok(false));

    let commands = commands.lock().expect("commands");
    assert_eq!(commands.documents.len(), 3);
    assert!(commands.documents[0].is_some());
    assert!(commands.documents[1].is_some());
    assert!(commands.documents[2].is_none());
    assert_ne!(
        commands.documents[0]
            .as_ref()
            .expect("first")
            .stamp
            .document_id,
        commands.documents[1]
            .as_ref()
            .expect("replacement")
            .stamp
            .document_id
    );
}

#[test]
fn definition_result_predicate_checks_current_snapshot_document_process_and_caret() {
    let model = AppModel::new();
    let document = model.document().expect("document");
    let stamp = document.stamp();
    let (port, _, _) = fake_port(snapshot_with_acknowledgeable_items(stamp));
    let mut state = LanguageUiState::new(Box::new(port));
    state
        .reconcile_document(Some(document))
        .expect("document submission");
    state
        .reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: Some(0),
            selection: None,
        }))
        .expect("caret submission");
    let result = state.snapshot().definition().expect("definition").clone();

    assert!(state.definition_result_current(&result));
    let mut wrong_process = result.clone();
    wrong_process.process_generation = ProcessGeneration::from_raw(2).expect("generation");
    assert!(!state.definition_result_current(&wrong_process));
    let mut wrong_caret = result.clone();
    wrong_caret.caret_character = 1;
    assert!(!state.definition_result_current(&wrong_caret));

    state
        .reconcile_caret(Some(EditorCaret {
            stamp,
            primary_character: Some(0),
            selection: Some(SelectionIdentity {
                anchor_character: 0,
                focus_character: 1,
            }),
        }))
        .expect("selection submission");
    assert!(!state.definition_result_current(&result));
}

#[test]
fn caret_waits_for_an_explicit_process_scope_then_starts_at_one() {
    let model = AppModel::new();
    let document = model.document().expect("document");
    let caret = EditorCaret {
        stamp: document.stamp(),
        primary_character: Some(0),
        selection: None,
    };
    let (port, snapshot_slot, commands) = fake_port(crate::language_ui::unavailable_snapshot());
    let mut state = LanguageUiState::new(Box::new(port));
    state
        .reconcile_document(Some(document))
        .expect("document submission");

    assert_eq!(state.reconcile_caret(Some(caret)), Ok(false));
    assert!(commands.lock().expect("commands").carets.is_empty());

    *snapshot_slot.lock().expect("snapshot") = generation_snapshot(1);
    state.refresh_snapshot();
    assert_eq!(state.reconcile_caret(Some(caret)), Ok(true));
    let commands = commands.lock().expect("commands");
    assert!(matches!(
        commands.carets.as_slice(),
        [CaretCommand::Current(intent)]
            if intent.process_generation == ProcessGeneration::from_raw(1).expect("generation")
                && intent.generation == CaretGeneration::from_raw(1).expect("caret")
    ));
}

#[test]
fn captured_definition_cannot_cross_a_process_generation_boundary() {
    let model = AppModel::new();
    let document = model.document().expect("document");
    let stamp = document.stamp();
    let (port, snapshot_slot, commands) = fake_port(written_snapshot(1, stamp));
    let mut state = LanguageUiState::new(Box::new(port));
    state
        .reconcile_document(Some(document))
        .expect("document submission");
    let caret = EditorCaret {
        stamp,
        primary_character: Some(2),
        selection: None,
    };
    state
        .reconcile_caret(Some(caret))
        .expect("caret submission");
    let stale = state
        .capture_definition(NavigationGeneration::from_raw(1).expect("navigation"))
        .expect("definition");

    *snapshot_slot.lock().expect("snapshot") = written_snapshot(2, stamp);
    state.refresh_snapshot();
    assert_eq!(state.submit_definition(stale), Ok(false));
    state
        .reconcile_caret(Some(caret))
        .expect("fresh caret submission");
    let current = state
        .capture_definition(NavigationGeneration::from_raw(2).expect("navigation"))
        .expect("definition");
    assert_eq!(state.submit_definition(current), Ok(true));

    let commands = commands.lock().expect("commands");
    assert_eq!(commands.definitions.len(), 1);
    assert_eq!(
        commands.definitions[0].process_generation,
        ProcessGeneration::from_raw(2).expect("generation")
    );
    assert_eq!(commands.definitions[0].caret_generation.get(), 1);
}
