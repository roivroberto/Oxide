use oxide_ide::{
    ClientCommandId, ClientStartId, ClientSubmission, ClosureHealth, CommandIntent, Envelope,
    EventSequence, ExecutionCommand, ExecutionCommandKind, ModelEffect, ModelEvent,
    PROTOCOL_VERSION, RequestId, RunBinding, RunId, RunMode, RuntimeCoordinator,
    RuntimeCoordinatorConfig, RuntimeDispatchError, RuntimeWake, StartIntent, StderrSummary,
    SubmitError, SupervisorCommand, SupervisorCommandKind, SupervisorDriver, SupervisorEvent,
    SupervisorFactory, SupervisorModelEvent, SupervisorPollError, SupervisorRun, SupervisorRunMode,
    SupervisorStartError, SupervisorSubmissionId, WorkerEvent, WorkerSessionId, WorkerTarget,
    WorkerTerminationReason,
};
use rlox::{RevisionId, SourceId};

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
struct WakeCounter(Arc<Mutex<usize>>);

impl RuntimeWake for WakeCounter {
    fn wake(&self) {
        *self.0.lock().expect("wake counter") += 1;
    }
}

#[derive(Clone)]
struct FakeSupervisorControl {
    session: WorkerSessionId,
    events: Arc<Mutex<VecDeque<Result<SupervisorEvent, SupervisorPollError>>>>,
    submissions: Arc<Mutex<Vec<(SupervisorSubmissionId, SupervisorCommand)>>>,
    submission_results: Arc<Mutex<VecDeque<Result<(), SubmitError>>>>,
    close_count: Arc<Mutex<usize>>,
    close_gate: Option<CloseGate>,
}

impl FakeSupervisorControl {
    fn new(session: u64) -> Self {
        Self {
            session: WorkerSessionId(session),
            events: Arc::default(),
            submissions: Arc::default(),
            submission_results: Arc::default(),
            close_count: Arc::default(),
            close_gate: None,
        }
    }

    fn with_close_gate(session: u64, close_gate: CloseGate) -> Self {
        Self {
            close_gate: Some(close_gate),
            ..Self::new(session)
        }
    }

    fn push_event(&self, event: SupervisorEvent) {
        self.events.lock().expect("events").push_back(Ok(event));
    }

    fn push_poll_error(&self, error: SupervisorPollError) {
        self.events.lock().expect("events").push_back(Err(error));
    }

    fn submissions(&self) -> Vec<(SupervisorSubmissionId, SupervisorCommand)> {
        self.submissions.lock().expect("submissions").clone()
    }

    fn close_count(&self) -> usize {
        *self.close_count.lock().expect("close count")
    }
}

struct FakeSupervisor(FakeSupervisorControl);

impl SupervisorDriver for FakeSupervisor {
    fn worker_session_id(&self) -> WorkerSessionId {
        self.0.session
    }

    fn try_send(
        &self,
        submission_id: SupervisorSubmissionId,
        command: SupervisorCommand,
    ) -> Result<(), SubmitError> {
        self.0
            .submissions
            .lock()
            .expect("submissions")
            .push((submission_id, command));
        self.0
            .submission_results
            .lock()
            .expect("submission results")
            .pop_front()
            .unwrap_or(Ok(()))
    }

    fn try_recv(&self) -> Result<Option<SupervisorEvent>, SupervisorPollError> {
        self.0
            .events
            .lock()
            .expect("events")
            .pop_front()
            .transpose()
    }

    fn close(&self) -> Result<(), SubmitError> {
        if let Some(close_gate) = &self.0.close_gate {
            close_gate.wait_for_release();
        }
        *self.0.close_count.lock().expect("close count") += 1;
        Ok(())
    }
}

#[derive(Clone, Default)]
struct CloseGate(Arc<(Mutex<CloseGateState>, Condvar)>);

#[derive(Default)]
struct CloseGateState {
    entered: bool,
    released: bool,
}

impl CloseGate {
    fn wait_for_release(&self) {
        let (state, changed) = &*self.0;
        let mut state = state.lock().expect("close gate");
        state.entered = true;
        changed.notify_all();
        while !state.released {
            state = changed.wait(state).expect("close gate");
        }
    }

    fn wait_until_entered(&self) {
        let (state, changed) = &*self.0;
        let mut state = state.lock().expect("close gate");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !state.entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (next, timeout) = changed.wait_timeout(state, remaining).expect("close gate");
            state = next;
            assert!(
                !timeout.timed_out() || state.entered,
                "timed out waiting for close gate"
            );
        }
    }

    fn release(&self) {
        let (state, changed) = &*self.0;
        let mut state = state.lock().expect("close gate");
        state.released = true;
        changed.notify_all();
    }
}

struct FakeFactory {
    supervisors: VecDeque<Result<FakeSupervisor, SupervisorStartError>>,
}

impl SupervisorFactory for FakeFactory {
    type Supervisor = FakeSupervisor;

    fn launch(&mut self) -> Result<Self::Supervisor, SupervisorStartError> {
        self.supervisors
            .pop_front()
            .expect("a configured supervisor")
    }
}

fn coordinator(
    controls: impl IntoIterator<Item = FakeSupervisorControl>,
    wake: WakeCounter,
) -> RuntimeCoordinator {
    coordinator_with_config(
        controls,
        wake,
        RuntimeCoordinatorConfig {
            request_capacity: 8,
            event_capacity: 8,
            poll_interval: Duration::from_millis(1),
        },
    )
}

fn coordinator_with_config(
    controls: impl IntoIterator<Item = FakeSupervisorControl>,
    wake: WakeCounter,
    config: RuntimeCoordinatorConfig,
) -> RuntimeCoordinator {
    let supervisors = controls
        .into_iter()
        .map(|control| Ok(FakeSupervisor(control)))
        .collect();
    RuntimeCoordinator::spawn_with_factory(FakeFactory { supervisors }, wake, config)
        .expect("coordinator starts")
}

fn receive(coordinator: &RuntimeCoordinator) -> ModelEvent {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(event) = coordinator.try_recv().expect("runtime remains connected") {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for runtime event"
        );
        std::thread::yield_now();
    }
}

fn run() -> SupervisorRun {
    SupervisorRun {
        worker_session_id: WorkerSessionId(11),
        run_id: RunId(1),
        source_id: SourceId(11),
        source_revision: RevisionId(1),
    }
}

fn binding() -> RunBinding {
    let run = run();
    RunBinding {
        worker_session_id: run.worker_session_id,
        run_id: run.run_id,
        source_id: run.source_id,
        source_revision: run.source_revision,
    }
}

#[test]
fn independent_submission_ids_preserve_start_and_command_tickets() {
    let control = FakeSupervisorControl::new(11);
    let wake = WakeCounter::default();
    let coordinator = coordinator([control.clone()], wake.clone());
    let start_id = ClientStartId::from_raw(1).expect("nonzero start");

    coordinator
        .try_dispatch(ModelEffect::Start(StartIntent {
            client_start_id: start_id,
            mode: RunMode::Debug,
            display_name: "main.ox".to_owned(),
            normalized_source: Arc::from("print 1;\n"),
        }))
        .expect("start queued");

    wait_for_submission_count(&control, 1);
    assert_eq!(control.submissions()[0].0, SupervisorSubmissionId(1));
    control.push_event(SupervisorEvent::Started {
        submission_id: SupervisorSubmissionId(1),
        mode: SupervisorRunMode::Debug,
        run: run(),
        request_id: RequestId(1),
        next_event_sequence: EventSequence(2),
    });
    assert!(matches!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::Started {
            client_start_id,
            run: admitted,
            ..
        }) if client_start_id == start_id && admitted == binding()
    ));

    let command_id = ClientCommandId::from_raw(1).expect("nonzero command");
    coordinator
        .try_dispatch(ModelEffect::SubmitCommand(CommandIntent {
            client_command_id: command_id,
            run: binding(),
            command: ExecutionCommand::Pause,
        }))
        .expect("command queued");

    wait_for_submission_count(&control, 2);
    assert_eq!(control.submissions()[1].0, SupervisorSubmissionId(2));
    control.push_event(SupervisorEvent::SubmissionAccepted {
        submission_id: SupervisorSubmissionId(2),
        command: SupervisorCommandKind::Pause,
        run: run(),
        request_id: RequestId(2),
        next_event_sequence: EventSequence(2),
    });
    assert!(matches!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::CommandAdmitted {
            client_command_id,
            command: ExecutionCommandKind::Pause,
            run: admitted,
            ..
        }) if client_command_id == command_id && admitted == binding()
    ));
    assert_eq!(*wake.0.lock().expect("wake counter"), 2);
}

#[test]
fn startup_hello_is_consumed_before_start_admission() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    let start_id = ClientStartId::from_raw(1).expect("nonzero start");

    coordinator
        .try_dispatch(ModelEffect::Start(StartIntent {
            client_start_id: start_id,
            mode: RunMode::Debug,
            display_name: "main.ox".to_owned(),
            normalized_source: Arc::from("print 1;\n"),
        }))
        .expect("start queued");
    wait_for_submission_count(&control, 1);

    control.push_event(SupervisorEvent::Worker(Box::new(Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: WorkerSessionId(11),
        run_id: RunId(0),
        source_revision: RevisionId(0),
        request_id: RequestId(0),
        sequence: EventSequence(1),
        payload: WorkerEvent::Hello,
    })));
    control.push_event(SupervisorEvent::Started {
        submission_id: SupervisorSubmissionId(1),
        mode: SupervisorRunMode::Debug,
        run: run(),
        request_id: RequestId(1),
        next_event_sequence: EventSequence(2),
    });

    assert!(matches!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::Started {
            client_start_id,
            run: admitted,
            next_event_sequence: EventSequence(2),
            ..
        }) if client_start_id == start_id && admitted == binding()
    ));
    assert_eq!(control.close_count(), 0);
}

#[test]
fn matching_run_close_maps_the_internal_close_admission() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    let start_id = ClientStartId::from_raw(1).expect("nonzero start");
    start_and_admit(&coordinator, &control, start_id);

    coordinator
        .try_dispatch(ModelEffect::CloseWorker {
            target: WorkerTarget::Run(binding()),
        })
        .expect("close queued");
    wait_until(|| control.close_count() == 1, "matching supervisor close");

    control.push_event(SupervisorEvent::CloseAccepted {
        run: run(),
        request_id: RequestId(2),
        next_event_sequence: EventSequence(2),
    });
    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::CloseAdmitted {
            run: binding(),
            request_id: RequestId(2),
            next_event_sequence: EventSequence(2),
        })
    );
}

#[test]
fn closed_retires_the_generation_before_a_fresh_worker_starts() {
    let first = FakeSupervisorControl::new(11);
    let second = FakeSupervisorControl::new(22);
    let coordinator = coordinator([first.clone(), second.clone()], WakeCounter::default());
    let first_start = ClientStartId::from_raw(1).expect("nonzero start");
    start_and_admit(&coordinator, &first, first_start);

    first.push_event(SupervisorEvent::Closed {
        run: Some(run()),
        status: Some(0),
        stderr: StderrSummary::default(),
        health: ClosureHealth::Clean,
    });
    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::Closed {
            client_start_id: Some(first_start),
            worker_session_id: WorkerSessionId(11),
            run: Some(binding()),
            health: ClosureHealth::Clean,
        })
    );

    let second_start = ClientStartId::from_raw(2).expect("nonzero start");
    coordinator
        .try_dispatch(ModelEffect::Start(StartIntent {
            client_start_id: second_start,
            mode: RunMode::Run,
            display_name: "next.ox".to_owned(),
            normalized_source: Arc::from("print 2;\n"),
        }))
        .expect("next start queued");
    wait_for_submission_count(&second, 1);
    let submissions = second.submissions();
    let [(submission, SupervisorCommand::Start { mode, .. })] = submissions.as_slice() else {
        panic!("expected one fresh start submission");
    };
    assert_eq!(*submission, SupervisorSubmissionId(1));
    assert_eq!(*mode, SupervisorRunMode::Run);
}

#[test]
fn worker_envelopes_are_forwarded_unchanged_and_in_order() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    start_and_admit(
        &coordinator,
        &control,
        ClientStartId::from_raw(1).expect("nonzero start"),
    );
    let first = Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: WorkerSessionId(11),
        run_id: RunId(1),
        source_revision: RevisionId(1),
        request_id: RequestId(1),
        sequence: EventSequence(2),
        payload: WorkerEvent::Output {
            text: "one".to_owned(),
        },
    };
    let second = Envelope {
        sequence: EventSequence(3),
        payload: WorkerEvent::Output {
            text: "two".to_owned(),
        },
        ..first.clone()
    };
    control.push_event(SupervisorEvent::Worker(Box::new(first.clone())));
    control.push_event(SupervisorEvent::Worker(Box::new(second.clone())));

    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::Worker(Box::new(first)))
    );
    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::Worker(Box::new(second)))
    );
}

#[test]
fn supervisor_rejection_returns_to_the_exact_client_command_ticket() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    start_and_admit(
        &coordinator,
        &control,
        ClientStartId::from_raw(1).expect("nonzero start"),
    );
    let command_id = ClientCommandId::from_raw(1).expect("nonzero command");
    coordinator
        .try_dispatch(ModelEffect::SubmitCommand(CommandIntent {
            client_command_id: command_id,
            run: binding(),
            command: ExecutionCommand::Pause,
        }))
        .expect("pause queued");
    wait_for_submission_count(&control, 2);
    control.push_event(SupervisorEvent::SubmissionRejected {
        submission_id: SupervisorSubmissionId(2),
        command: SupervisorCommandKind::Pause,
        error: SubmitError::Full,
    });

    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::SubmissionRejected {
            submission: ClientSubmission::Command(command_id),
            error: SubmitError::Full,
        })
    );
}

#[test]
fn poll_failure_terminates_and_closes_the_exact_generation() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    let start_id = ClientStartId::from_raw(1).expect("nonzero start");
    start_and_admit(&coordinator, &control, start_id);
    control.push_poll_error(SupervisorPollError::Poisoned);

    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::WorkerTerminated {
            client_start_id: Some(start_id),
            worker_session_id: WorkerSessionId(11),
            run: Some(binding()),
            reason: WorkerTerminationReason::SupervisorClosed,
        })
    );
    wait_until(|| control.close_count() == 1, "failed supervisor close");
}

#[test]
fn unknown_admission_id_is_a_correlated_causality_failure() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    let start_id = ClientStartId::from_raw(1).expect("nonzero start");
    coordinator
        .try_dispatch(ModelEffect::Start(StartIntent {
            client_start_id: start_id,
            mode: RunMode::Debug,
            display_name: "main.ox".to_owned(),
            normalized_source: Arc::from("print 1;\n"),
        }))
        .expect("start queued");
    wait_for_submission_count(&control, 1);
    control.push_event(SupervisorEvent::Started {
        submission_id: SupervisorSubmissionId(99),
        mode: SupervisorRunMode::Debug,
        run: run(),
        request_id: RequestId(1),
        next_event_sequence: EventSequence(2),
    });

    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::WorkerTerminated {
            client_start_id: Some(start_id),
            worker_session_id: WorkerSessionId(11),
            run: None,
            reason: WorkerTerminationReason::CausalityViolation,
        })
    );
    wait_until(
        || control.close_count() == 1,
        "desynchronized supervisor close",
    );
}

#[test]
fn launch_and_immediate_submit_failures_are_typed() {
    let launch_coordinator = RuntimeCoordinator::spawn_with_factory(
        FakeFactory {
            supervisors: VecDeque::from([Err(SupervisorStartError {
                kind: std::io::ErrorKind::PermissionDenied,
            })]),
        },
        WakeCounter::default(),
        RuntimeCoordinatorConfig::default(),
    )
    .expect("coordinator starts");
    let launch_id = ClientStartId::from_raw(1).expect("nonzero start");
    launch_coordinator
        .try_dispatch(start_effect(launch_id))
        .expect("start queued");
    assert_eq!(
        receive(&launch_coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::StartFailed {
            client_start_id: launch_id,
            kind: std::io::ErrorKind::PermissionDenied,
        })
    );

    let control = FakeSupervisorControl::new(11);
    control
        .submission_results
        .lock()
        .expect("submission results")
        .push_back(Err(SubmitError::Full));
    let coordinator = coordinator([control], WakeCounter::default());
    let rejected_id = ClientStartId::from_raw(2).expect("nonzero start");
    coordinator
        .try_dispatch(start_effect(rejected_id))
        .expect("start queued");
    assert_eq!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::SubmissionRejected {
            submission: ClientSubmission::Start(rejected_id),
            error: SubmitError::Full,
        })
    );
}

#[test]
fn pending_start_close_is_exact_and_idempotent() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    let start_id = ClientStartId::from_raw(1).expect("nonzero start");
    coordinator
        .try_dispatch(start_effect(start_id))
        .expect("start queued");
    wait_for_submission_count(&control, 1);

    coordinator
        .try_dispatch(ModelEffect::CloseWorker {
            target: WorkerTarget::PendingStart(
                ClientStartId::from_raw(2).expect("nonzero stale start"),
            ),
        })
        .expect("stale close queued");
    std::thread::yield_now();
    assert_eq!(control.close_count(), 0);

    for _ in 0..2 {
        coordinator
            .try_dispatch(ModelEffect::CloseWorker {
                target: WorkerTarget::PendingStart(start_id),
            })
            .expect("matching close queued");
    }
    wait_until(|| control.close_count() == 1, "one pending-start close");
}

#[test]
fn full_gui_event_lane_does_not_starve_close_or_emit_early_wakes() {
    let control = FakeSupervisorControl::new(11);
    let wake = WakeCounter::default();
    let coordinator = coordinator_with_config(
        [control.clone()],
        wake.clone(),
        RuntimeCoordinatorConfig {
            request_capacity: 8,
            event_capacity: 1,
            poll_interval: Duration::from_millis(1),
        },
    );
    let start_id = ClientStartId::from_raw(1).expect("nonzero start");
    coordinator
        .try_dispatch(start_effect(start_id))
        .expect("start queued");
    wait_for_submission_count(&control, 1);
    control.push_event(SupervisorEvent::Started {
        submission_id: SupervisorSubmissionId(1),
        mode: SupervisorRunMode::Debug,
        run: run(),
        request_id: RequestId(1),
        next_event_sequence: EventSequence(2),
    });
    wait_until(
        || *wake.0.lock().expect("wake counter") == 1,
        "first queued event wake",
    );
    control.push_event(SupervisorEvent::Worker(Box::new(Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: WorkerSessionId(11),
        run_id: RunId(1),
        source_revision: RevisionId(1),
        request_id: RequestId(1),
        sequence: EventSequence(2),
        payload: WorkerEvent::Output {
            text: "queued".to_owned(),
        },
    })));
    coordinator
        .try_dispatch(ModelEffect::CloseWorker {
            target: WorkerTarget::Run(binding()),
        })
        .expect("close queued");
    wait_until(
        || control.close_count() == 1,
        "close behind full event lane",
    );
    assert_eq!(*wake.0.lock().expect("wake counter"), 1);

    let _ = receive(&coordinator);
    wait_until(
        || *wake.0.lock().expect("wake counter") == 2,
        "second event enqueue wake",
    );
    assert!(matches!(
        receive(&coordinator),
        ModelEvent::Supervisor(SupervisorModelEvent::Worker(_))
    ));
}

#[test]
fn shutdown_joins_the_actor_after_closing_the_owned_generation() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    coordinator
        .try_dispatch(start_effect(
            ClientStartId::from_raw(1).expect("nonzero start"),
        ))
        .expect("start queued");
    wait_for_submission_count(&control, 1);

    coordinator.shutdown();
    assert_eq!(control.close_count(), 1);
    assert!(matches!(
        coordinator.try_dispatch(start_effect(
            ClientStartId::from_raw(2).expect("nonzero start")
        )),
        Err(RuntimeDispatchError::Closed(_))
    ));

    drop(coordinator);
    assert_eq!(control.close_count(), 1);
}

#[test]
fn drop_joins_the_actor_after_closing_the_owned_generation() {
    let control = FakeSupervisorControl::new(11);
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    coordinator
        .try_dispatch(start_effect(
            ClientStartId::from_raw(1).expect("nonzero start"),
        ))
        .expect("start queued");
    wait_for_submission_count(&control, 1);

    drop(coordinator);

    assert_eq!(control.close_count(), 1);
}

#[test]
fn shutdown_wait_is_bounded_and_a_later_shutdown_joins_the_actor() {
    let gate = CloseGate::default();
    let control = FakeSupervisorControl::with_close_gate(11, gate.clone());
    let coordinator = coordinator([control.clone()], WakeCounter::default());
    coordinator
        .try_dispatch(start_effect(
            ClientStartId::from_raw(1).expect("nonzero start"),
        ))
        .expect("start queued");
    wait_for_submission_count(&control, 1);

    let started = Instant::now();
    coordinator.shutdown();
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "shutdown must not wait indefinitely for an uncooperative driver"
    );
    gate.wait_until_entered();
    assert_eq!(control.close_count(), 0);

    gate.release();
    coordinator.shutdown();
    assert_eq!(control.close_count(), 1);
}

fn start_effect(client_start_id: ClientStartId) -> ModelEffect {
    ModelEffect::Start(StartIntent {
        client_start_id,
        mode: RunMode::Debug,
        display_name: "main.ox".to_owned(),
        normalized_source: Arc::from("print 1;\n"),
    })
}

fn start_and_admit(
    coordinator: &RuntimeCoordinator,
    control: &FakeSupervisorControl,
    start_id: ClientStartId,
) {
    coordinator
        .try_dispatch(ModelEffect::Start(StartIntent {
            client_start_id: start_id,
            mode: RunMode::Debug,
            display_name: "main.ox".to_owned(),
            normalized_source: Arc::from("print 1;\n"),
        }))
        .expect("start queued");
    wait_for_submission_count(control, 1);
    control.push_event(SupervisorEvent::Started {
        submission_id: SupervisorSubmissionId(1),
        mode: SupervisorRunMode::Debug,
        run: run(),
        request_id: RequestId(1),
        next_event_sequence: EventSequence(2),
    });
    let _ = receive(coordinator);
}

fn wait_until(mut predicate: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if predicate() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::yield_now();
    }
}

fn wait_for_submission_count(control: &FakeSupervisorControl, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if control.submissions().len() >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for submission"
        );
        std::thread::yield_now();
    }
}
