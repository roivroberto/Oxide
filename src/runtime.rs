use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use super::{
    ClientSubmission, CommandIntent, ExecutionCommand, ExecutionCommandKind, ModelEffect,
    ModelEvent, RunBinding, RunMode, StartIntent, SubmitError, SupervisorCommand,
    SupervisorCommandKind, SupervisorConfig, SupervisorEvent, SupervisorModelEvent,
    SupervisorPollError, SupervisorRun, SupervisorRunMode, SupervisorSource, SupervisorStartError,
    SupervisorSubmissionId, WorkerEvent, WorkerSessionId, WorkerSupervisor, WorkerTarget,
};

const MAX_REQUESTS_PER_TURN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeCoordinatorConfig {
    pub request_capacity: usize,
    pub event_capacity: usize,
    pub poll_interval: Duration,
}

impl Default for RuntimeCoordinatorConfig {
    fn default() -> Self {
        Self {
            request_capacity: 32,
            event_capacity: 128,
            poll_interval: Duration::from_millis(2),
        }
    }
}

pub trait RuntimeWake: Send + Sync + 'static {
    fn wake(&self);
}

impl<F> RuntimeWake for F
where
    F: Fn() + Send + Sync + 'static,
{
    fn wake(&self) {
        self();
    }
}

pub trait SupervisorDriver: Send + 'static {
    fn worker_session_id(&self) -> WorkerSessionId;

    fn try_send(
        &self,
        submission_id: SupervisorSubmissionId,
        command: SupervisorCommand,
    ) -> Result<(), SubmitError>;

    fn try_recv(&self) -> Result<Option<SupervisorEvent>, SupervisorPollError>;

    fn close(&self) -> Result<(), SubmitError>;
}

impl SupervisorDriver for WorkerSupervisor {
    fn worker_session_id(&self) -> WorkerSessionId {
        WorkerSupervisor::worker_session_id(self)
    }

    fn try_send(
        &self,
        submission_id: SupervisorSubmissionId,
        command: SupervisorCommand,
    ) -> Result<(), SubmitError> {
        self.command_sender().try_send(submission_id, command)
    }

    fn try_recv(&self) -> Result<Option<SupervisorEvent>, SupervisorPollError> {
        WorkerSupervisor::try_recv(self)
    }

    fn close(&self) -> Result<(), SubmitError> {
        WorkerSupervisor::close(self)
    }
}

pub trait SupervisorFactory: Send + 'static {
    type Supervisor: SupervisorDriver;

    fn launch(&mut self) -> Result<Self::Supervisor, SupervisorStartError>;
}

struct NativeSupervisorFactory {
    config: SupervisorConfig,
}

impl SupervisorFactory for NativeSupervisorFactory {
    type Supervisor = WorkerSupervisor;

    fn launch(&mut self) -> Result<Self::Supervisor, SupervisorStartError> {
        WorkerSupervisor::launch(self.config.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeStartError {
    pub kind: io::ErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeDispatchError {
    Unsupported(ModelEffect),
    Full(ModelEffect),
    Closed(ModelEffect),
}

impl RuntimeDispatchError {
    pub fn into_effect(self) -> ModelEffect {
        match self {
            Self::Unsupported(effect) | Self::Full(effect) | Self::Closed(effect) => effect,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeReceiveError {
    Disconnected,
}

pub struct RuntimeCoordinator {
    requests: mpsc::SyncSender<RuntimeRequest>,
    events: mpsc::Receiver<ModelEvent>,
    shutdown: Arc<AtomicBool>,
}

impl RuntimeCoordinator {
    pub fn spawn<W>(supervisor: SupervisorConfig, wake: W) -> Result<Self, RuntimeStartError>
    where
        W: RuntimeWake,
    {
        Self::spawn_with_factory(
            NativeSupervisorFactory { config: supervisor },
            wake,
            RuntimeCoordinatorConfig::default(),
        )
    }

    pub fn spawn_with_factory<F, W>(
        factory: F,
        wake: W,
        config: RuntimeCoordinatorConfig,
    ) -> Result<Self, RuntimeStartError>
    where
        F: SupervisorFactory,
        W: RuntimeWake,
    {
        if config.request_capacity == 0
            || config.event_capacity == 0
            || config.poll_interval.is_zero()
        {
            return Err(RuntimeStartError {
                kind: io::ErrorKind::InvalidInput,
            });
        }
        let (request_tx, request_rx) = mpsc::sync_channel(config.request_capacity);
        let (event_tx, event_rx) = mpsc::sync_channel(config.event_capacity);
        let shutdown = Arc::new(AtomicBool::new(false));
        let actor_shutdown = shutdown.clone();
        thread::Builder::new()
            .name("oxide-runtime-coordinator".to_owned())
            .spawn(move || {
                runtime_actor(
                    factory,
                    request_rx,
                    event_tx,
                    Arc::new(wake),
                    actor_shutdown,
                    config.poll_interval,
                );
            })
            .map_err(|error| RuntimeStartError { kind: error.kind() })?;
        Ok(Self {
            requests: request_tx,
            events: event_rx,
            shutdown,
        })
    }

    pub fn try_dispatch(&self, effect: ModelEffect) -> Result<(), RuntimeDispatchError> {
        let request = match effect {
            ModelEffect::Start(intent) => RuntimeRequest::Start(intent),
            ModelEffect::SubmitCommand(intent) => RuntimeRequest::Command(intent),
            ModelEffect::CloseWorker { target } => RuntimeRequest::Close(target),
            other => return Err(RuntimeDispatchError::Unsupported(other)),
        };
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(request) => {
                    RuntimeDispatchError::Full(request.into_effect())
                }
                mpsc::TrySendError::Disconnected(request) => {
                    RuntimeDispatchError::Closed(request.into_effect())
                }
            })
    }

    pub fn try_recv(&self) -> Result<Option<ModelEvent>, RuntimeReceiveError> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(RuntimeReceiveError::Disconnected),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.requests.try_send(RuntimeRequest::Shutdown);
    }
}

impl Drop for RuntimeCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum RuntimeRequest {
    Start(StartIntent),
    Command(CommandIntent),
    Close(WorkerTarget),
    Shutdown,
}

impl RuntimeRequest {
    fn into_effect(self) -> ModelEffect {
        match self {
            Self::Start(intent) => ModelEffect::Start(intent),
            Self::Command(intent) => ModelEffect::SubmitCommand(intent),
            Self::Close(target) => ModelEffect::CloseWorker { target },
            Self::Shutdown => unreachable!("shutdown is not a model effect"),
        }
    }
}

#[derive(Clone, Copy)]
enum PendingSubmission {
    Start {
        client: super::ClientStartId,
        mode: RunMode,
    },
    Command {
        client: super::ClientCommandId,
        command: ExecutionCommandKind,
        run: RunBinding,
    },
}

struct SessionSlot<S> {
    client_start_id: super::ClientStartId,
    supervisor: S,
    run: Option<RunBinding>,
    close_target: Option<WorkerTarget>,
    next_submission_id: Option<u64>,
    pending: HashMap<SupervisorSubmissionId, PendingSubmission>,
}

impl<S: SupervisorDriver> SessionSlot<S> {
    fn new(client_start_id: super::ClientStartId, supervisor: S) -> Self {
        Self {
            client_start_id,
            supervisor,
            run: None,
            close_target: None,
            next_submission_id: Some(1),
            pending: HashMap::new(),
        }
    }

    fn allocate_submission(&mut self) -> Result<SupervisorSubmissionId, SubmitError> {
        let value = self
            .next_submission_id
            .ok_or(SubmitError::InvalidSubmissionId)?;
        self.next_submission_id = value.checked_add(1);
        Ok(SupervisorSubmissionId(value))
    }
}

fn runtime_actor<F, W>(
    mut factory: F,
    requests: mpsc::Receiver<RuntimeRequest>,
    events: mpsc::SyncSender<ModelEvent>,
    wake: Arc<W>,
    shutdown: Arc<AtomicBool>,
    poll_interval: Duration,
) where
    F: SupervisorFactory,
    W: RuntimeWake,
{
    let mut slot: Option<SessionSlot<F::Supervisor>> = None;
    let mut outbox = VecDeque::new();
    let mut retire_after_delivery = false;
    loop {
        if deliver_outbox(&events, wake.as_ref(), &mut outbox).is_err() {
            shutdown.store(true, Ordering::Release);
        }
        if retire_after_delivery && outbox.is_empty() {
            slot = None;
            retire_after_delivery = false;
        }
        if shutdown.load(Ordering::Acquire) {
            if let Some(slot) = slot.take() {
                let _ = slot.supervisor.close();
            }
            break;
        }

        let mut handled_request = false;
        for _ in 0..MAX_REQUESTS_PER_TURN {
            match requests.try_recv() {
                Ok(request) => {
                    handled_request = true;
                    if matches!(request, RuntimeRequest::Shutdown) {
                        shutdown.store(true, Ordering::Release);
                        break;
                    }
                    handle_request(request, &mut factory, &mut slot, &mut outbox);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    shutdown.store(true, Ordering::Release);
                    break;
                }
            }
        }
        if shutdown.load(Ordering::Acquire) {
            continue;
        }

        if outbox.is_empty()
            && let Some(current) = slot.as_mut()
        {
            match current.supervisor.try_recv() {
                Ok(Some(event)) => {
                    retire_after_delivery |= translate_event(current, event, &mut outbox);
                }
                Ok(None) => {}
                Err(_) => {
                    let _ = current.supervisor.close();
                    outbox.push_back(ModelEvent::Supervisor(
                        SupervisorModelEvent::WorkerTerminated {
                            client_start_id: Some(current.client_start_id),
                            worker_session_id: current.supervisor.worker_session_id(),
                            run: current.run,
                            reason: super::WorkerTerminationReason::SupervisorClosed,
                        },
                    ));
                    retire_after_delivery = true;
                }
            }
        }
        if deliver_outbox(&events, wake.as_ref(), &mut outbox).is_err() {
            shutdown.store(true, Ordering::Release);
            continue;
        }
        if retire_after_delivery && outbox.is_empty() {
            slot = None;
            retire_after_delivery = false;
        }
        if handled_request {
            thread::yield_now();
            continue;
        }

        match requests.recv_timeout(poll_interval) {
            Ok(RuntimeRequest::Shutdown) => shutdown.store(true, Ordering::Release),
            Ok(request) => handle_request(request, &mut factory, &mut slot, &mut outbox),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => shutdown.store(true, Ordering::Release),
        }
    }
}

fn deliver_outbox<W: RuntimeWake>(
    events: &mpsc::SyncSender<ModelEvent>,
    wake: &W,
    outbox: &mut VecDeque<ModelEvent>,
) -> Result<(), ()> {
    while let Some(event) = outbox.pop_front() {
        match events.try_send(event) {
            Ok(()) => wake.wake(),
            Err(mpsc::TrySendError::Full(event)) => {
                outbox.push_front(event);
                break;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return Err(()),
        }
    }
    Ok(())
}

fn handle_request<F: SupervisorFactory>(
    request: RuntimeRequest,
    factory: &mut F,
    slot: &mut Option<SessionSlot<F::Supervisor>>,
    outbox: &mut VecDeque<ModelEvent>,
) {
    match request {
        RuntimeRequest::Start(intent) => handle_start(intent, factory, slot, outbox),
        RuntimeRequest::Command(intent) => handle_command(intent, slot, outbox),
        RuntimeRequest::Close(target) => handle_close(target, slot),
        RuntimeRequest::Shutdown => {}
    }
}

fn handle_close<S: SupervisorDriver>(target: WorkerTarget, slot: &mut Option<SessionSlot<S>>) {
    let Some(current) = slot.as_mut() else {
        return;
    };
    let matching = match target {
        WorkerTarget::PendingStart(client) => client == current.client_start_id,
        WorkerTarget::Run(run) => current.run == Some(run),
    };
    if !matching || current.close_target.is_some() {
        return;
    }
    current.close_target = Some(target);
    let _ = current.supervisor.close();
}

fn handle_start<F: SupervisorFactory>(
    intent: StartIntent,
    factory: &mut F,
    slot: &mut Option<SessionSlot<F::Supervisor>>,
    outbox: &mut VecDeque<ModelEvent>,
) {
    if slot.is_some() {
        outbox.push_back(ModelEvent::Supervisor(SupervisorModelEvent::StartFailed {
            client_start_id: intent.client_start_id,
            kind: io::ErrorKind::WouldBlock,
        }));
        return;
    }
    let supervisor = match factory.launch() {
        Ok(supervisor) => supervisor,
        Err(error) => {
            outbox.push_back(ModelEvent::Supervisor(SupervisorModelEvent::StartFailed {
                client_start_id: intent.client_start_id,
                kind: error.kind,
            }));
            return;
        }
    };
    let mut current = SessionSlot::new(intent.client_start_id, supervisor);
    let submission = match current.allocate_submission() {
        Ok(submission) => submission,
        Err(error) => {
            outbox.push_back(submission_rejected(
                ClientSubmission::Start(intent.client_start_id),
                error,
            ));
            *slot = Some(current);
            return;
        }
    };
    let mode = intent.mode;
    let command = SupervisorCommand::Start {
        mode: supervisor_mode(mode),
        source: SupervisorSource {
            display_name: intent.display_name,
            text: intent.normalized_source.to_string(),
        },
    };
    match current.supervisor.try_send(submission, command) {
        Ok(()) => {
            current.pending.insert(
                submission,
                PendingSubmission::Start {
                    client: intent.client_start_id,
                    mode,
                },
            );
        }
        Err(error) => outbox.push_back(submission_rejected(
            ClientSubmission::Start(intent.client_start_id),
            error,
        )),
    }
    *slot = Some(current);
}

fn handle_command<S: SupervisorDriver>(
    intent: CommandIntent,
    slot: &mut Option<SessionSlot<S>>,
    outbox: &mut VecDeque<ModelEvent>,
) {
    let Some(current) = slot.as_mut() else {
        outbox.push_back(submission_rejected(
            ClientSubmission::Command(intent.client_command_id),
            SubmitError::Closed,
        ));
        return;
    };
    if current.run != Some(intent.run) {
        outbox.push_back(submission_rejected(
            ClientSubmission::Command(intent.client_command_id),
            SubmitError::InvalidState,
        ));
        return;
    }
    let submission = match current.allocate_submission() {
        Ok(submission) => submission,
        Err(error) => {
            outbox.push_back(submission_rejected(
                ClientSubmission::Command(intent.client_command_id),
                error,
            ));
            return;
        }
    };
    let command_kind = command_kind(&intent.command);
    let command = supervisor_command(intent.command);
    match current.supervisor.try_send(submission, command) {
        Ok(()) => {
            current.pending.insert(
                submission,
                PendingSubmission::Command {
                    client: intent.client_command_id,
                    command: command_kind,
                    run: intent.run,
                },
            );
        }
        Err(error) => outbox.push_back(submission_rejected(
            ClientSubmission::Command(intent.client_command_id),
            error,
        )),
    }
}

fn translate_event<S: SupervisorDriver>(
    slot: &mut SessionSlot<S>,
    event: SupervisorEvent,
    outbox: &mut VecDeque<ModelEvent>,
) -> bool {
    match event {
        SupervisorEvent::Started {
            submission_id,
            mode,
            run,
            request_id,
            next_event_sequence,
        } => {
            let Some(PendingSubmission::Start {
                client,
                mode: expected_mode,
            }) = slot.pending.remove(&submission_id)
            else {
                return correlation_failure(slot, outbox);
            };
            if supervisor_mode(expected_mode) != mode
                || run.worker_session_id != slot.supervisor.worker_session_id()
            {
                return correlation_failure(slot, outbox);
            }
            let run = run_binding(run);
            slot.run = Some(run);
            outbox.push_back(ModelEvent::Supervisor(SupervisorModelEvent::Started {
                client_start_id: client,
                mode: expected_mode,
                run,
                request_id,
                next_event_sequence,
            }));
            false
        }
        SupervisorEvent::SubmissionAccepted {
            submission_id,
            command,
            run,
            request_id,
            next_event_sequence,
        } => {
            let Some(PendingSubmission::Command {
                client,
                command: expected_command,
                run: expected_run,
            }) = slot.pending.remove(&submission_id)
            else {
                return correlation_failure(slot, outbox);
            };
            if expected_command.supervisor_kind() != command || run_binding(run) != expected_run {
                return correlation_failure(slot, outbox);
            }
            outbox.push_back(ModelEvent::Supervisor(
                SupervisorModelEvent::CommandAdmitted {
                    client_command_id: client,
                    command: expected_command,
                    run: expected_run,
                    request_id,
                    next_event_sequence,
                },
            ));
            false
        }
        SupervisorEvent::CloseAccepted {
            run,
            request_id,
            next_event_sequence,
        } => {
            let run = run_binding(run);
            let matching_target = match slot.close_target {
                Some(WorkerTarget::PendingStart(client)) => client == slot.client_start_id,
                Some(WorkerTarget::Run(expected)) => expected == run,
                None => false,
            };
            if !matching_target || slot.run != Some(run) {
                return correlation_failure(slot, outbox);
            }
            outbox.push_back(ModelEvent::Supervisor(
                SupervisorModelEvent::CloseAdmitted {
                    run,
                    request_id,
                    next_event_sequence,
                },
            ));
            false
        }
        SupervisorEvent::Worker(envelope) => {
            if matches!(&envelope.payload, WorkerEvent::Hello) {
                let valid_startup_hello = slot.run.is_none()
                    && envelope.version == super::PROTOCOL_VERSION
                    && envelope.worker_session_id == slot.supervisor.worker_session_id()
                    && envelope.run_id.0 == 0
                    && envelope.source_revision.0 == 0
                    && envelope.request_id.0 == 0
                    && envelope.sequence.0 == 1;
                return if valid_startup_hello {
                    false
                } else {
                    correlation_failure(slot, outbox)
                };
            }
            let Some(run) = slot.run else {
                return correlation_failure(slot, outbox);
            };
            if envelope.worker_session_id != run.worker_session_id
                || envelope.run_id != run.run_id
                || envelope.source_revision != run.source_revision
            {
                return correlation_failure(slot, outbox);
            }
            outbox.push_back(ModelEvent::Supervisor(SupervisorModelEvent::Worker(
                envelope,
            )));
            false
        }
        SupervisorEvent::SubmissionRejected {
            submission_id,
            command,
            error,
        } => {
            let Some(pending) = slot.pending.remove(&submission_id) else {
                return correlation_failure(slot, outbox);
            };
            let submission = match pending {
                PendingSubmission::Start { client, .. }
                    if command == SupervisorCommandKind::Start =>
                {
                    ClientSubmission::Start(client)
                }
                PendingSubmission::Command {
                    client,
                    command: expected,
                    ..
                } if command == expected.supervisor_kind() => ClientSubmission::Command(client),
                _ => return correlation_failure(slot, outbox),
            };
            outbox.push_back(submission_rejected(submission, error));
            false
        }
        SupervisorEvent::WorkerTerminated { run, reason, .. } => {
            let run = run.map(run_binding);
            if run != slot.run {
                return correlation_failure(slot, outbox);
            }
            outbox.push_back(ModelEvent::Supervisor(
                SupervisorModelEvent::WorkerTerminated {
                    client_start_id: Some(slot.client_start_id),
                    worker_session_id: slot.supervisor.worker_session_id(),
                    run,
                    reason,
                },
            ));
            true
        }
        SupervisorEvent::Closed { run, health, .. } => {
            let run = run.map(run_binding);
            if run != slot.run {
                return correlation_failure(slot, outbox);
            }
            outbox.push_back(ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: Some(slot.client_start_id),
                worker_session_id: slot.supervisor.worker_session_id(),
                run,
                health,
            }));
            true
        }
    }
}

fn correlation_failure<S: SupervisorDriver>(
    slot: &SessionSlot<S>,
    outbox: &mut VecDeque<ModelEvent>,
) -> bool {
    let _ = slot.supervisor.close();
    outbox.push_back(ModelEvent::Supervisor(
        SupervisorModelEvent::WorkerTerminated {
            client_start_id: Some(slot.client_start_id),
            worker_session_id: slot.supervisor.worker_session_id(),
            run: slot.run,
            reason: super::WorkerTerminationReason::CausalityViolation,
        },
    ));
    true
}

fn submission_rejected(submission: ClientSubmission, error: SubmitError) -> ModelEvent {
    ModelEvent::Supervisor(SupervisorModelEvent::SubmissionRejected { submission, error })
}

fn supervisor_mode(mode: RunMode) -> SupervisorRunMode {
    match mode {
        RunMode::Run => SupervisorRunMode::Run,
        RunMode::Debug => SupervisorRunMode::Debug,
    }
}

fn command_kind(command: &ExecutionCommand) -> ExecutionCommandKind {
    match command {
        ExecutionCommand::Pause => ExecutionCommandKind::Pause,
        ExecutionCommand::Continue => ExecutionCommandKind::Continue,
        ExecutionCommand::StepInto => ExecutionCommandKind::StepInto,
        ExecutionCommand::StepOver => ExecutionCommandKind::StepOver,
        ExecutionCommand::StepOut => ExecutionCommandKind::StepOut,
        ExecutionCommand::Stop => ExecutionCommandKind::Stop,
        ExecutionCommand::ProvideInput { .. } => ExecutionCommandKind::ProvideInput,
    }
}

fn supervisor_command(command: ExecutionCommand) -> SupervisorCommand {
    match command {
        ExecutionCommand::Pause => SupervisorCommand::Pause,
        ExecutionCommand::Continue => SupervisorCommand::Continue,
        ExecutionCommand::StepInto => SupervisorCommand::StepInto,
        ExecutionCommand::StepOver => SupervisorCommand::StepOver,
        ExecutionCommand::StepOut => SupervisorCommand::StepOut,
        ExecutionCommand::Stop => SupervisorCommand::Stop,
        ExecutionCommand::ProvideInput { in_reply_to, text } => {
            SupervisorCommand::ProvideInput { in_reply_to, text }
        }
    }
}

fn run_binding(run: SupervisorRun) -> RunBinding {
    RunBinding {
        worker_session_id: run.worker_session_id,
        run_id: run.run_id,
        source_id: run.source_id,
        source_revision: run.source_revision,
    }
}
