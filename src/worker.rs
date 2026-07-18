use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;

use rlox::{
    Diagnostic, ExecutionControl, InterpreterSession, PauseReason, ResumeMode, RevisionId,
    RunOutcome, RuntimeHost, SourceId,
};

use crate::protocol::{
    Command, CommandStreamValidator, DecodeError, EncodeError, Envelope, EventSequence, LineCodec,
    MAX_OUTPUT_CHUNK_TEXT_BYTES, MAX_RUN_OUTPUT_FRAME_BYTES, PROTOCOL_VERSION, RequestId, RunId,
    WireDiagnostic, WireDocument, WorkerEvent, WorkerSessionId,
};

const EVENT_QUEUE_ITEMS: usize = 256;
const OUTPUT_QUEUE_ITEMS: usize = 240;
const EVENT_QUEUE_BYTES: usize = 16 * 1024 * 1024 + 1;
const NORMAL_EVENT_QUEUE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerError {
    ZeroSession,
    Decode(DecodeError),
    InvalidCommandStream,
    Encode(EncodeError),
    Io(std::io::ErrorKind),
    ChannelClosed,
    StateCorrupted,
    ThreadPanicked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CausalRequest {
    run_id: RunId,
    revision: RevisionId,
    request_id: RequestId,
}

impl CausalRequest {
    fn from_command(envelope: &Envelope<Command>) -> Self {
        Self {
            run_id: envelope.run_id,
            revision: envelope.source_revision,
            request_id: envelope.request_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveRun {
    run_id: RunId,
    revision: RevisionId,
    source_id: SourceId,
}

impl ActiveRun {
    fn matches(&self, envelope: &Envelope<Command>) -> bool {
        self.run_id == envelope.run_id && self.revision == envelope.source_revision
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerPhase {
    AwaitLoad,
    Active,
    Paused,
    Terminal,
    Closed,
}

struct ControlState {
    phase: WorkerPhase,
    active: Option<ActiveRun>,
    control: Option<ExecutionControl>,
    pending_pause: Option<CausalRequest>,
    pending_stop: Option<CausalRequest>,
    fatal: bool,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            phase: WorkerPhase::AwaitLoad,
            active: None,
            control: None,
            pending_pause: None,
            pending_stop: None,
            fatal: false,
        }
    }
}

#[derive(Default)]
struct ControlPlane {
    state: Mutex<ControlState>,
}

enum OwnerAction {
    Load {
        document: WireDocument,
        debugging: bool,
        driver: CausalRequest,
    },
    Resume {
        mode: ResumeMode,
        driver: CausalRequest,
    },
    CancelPaused,
    Shutdown,
    EndOfInput,
    Fatal,
    WriterFailed,
}

enum PauseCommit {
    Emit(CausalRequest),
    StopWon,
}

impl ControlPlane {
    fn abort(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.fatal = true;
            if let Some(control) = &state.control {
                control.request_cancel();
            }
        }
    }

    fn is_fatal(&self) -> Result<bool, WorkerError> {
        self.state
            .lock()
            .map(|state| state.fatal)
            .map_err(|_| WorkerError::StateCorrupted)
    }

    fn install_control(&self, control: ExecutionControl) -> Result<(), WorkerError> {
        let mut state = self.state.lock().map_err(|_| WorkerError::StateCorrupted)?;
        if state.phase != WorkerPhase::Active || state.control.is_some() {
            return Err(WorkerError::StateCorrupted);
        }
        if state.pending_pause.is_some() {
            control.request_pause();
        }
        if state.pending_stop.is_some() {
            control.request_cancel();
        }
        state.control = Some(control);
        Ok(())
    }

    fn commit_pause(
        &self,
        reason: PauseReason,
        driver: CausalRequest,
    ) -> Result<PauseCommit, WorkerError> {
        let mut state = self.state.lock().map_err(|_| WorkerError::StateCorrupted)?;
        if state.phase != WorkerPhase::Active {
            return Err(WorkerError::StateCorrupted);
        }
        if state.pending_stop.is_some() {
            state.pending_pause = None;
            return Ok(PauseCommit::StopWon);
        }
        let causal = if reason == PauseReason::Explicit {
            state
                .pending_pause
                .take()
                .ok_or(WorkerError::StateCorrupted)?
        } else {
            state.pending_pause = None;
            driver
        };
        state.phase = WorkerPhase::Paused;
        Ok(PauseCommit::Emit(causal))
    }

    fn commit_terminal(
        &self,
        cancelled: bool,
        driver: CausalRequest,
    ) -> Result<CausalRequest, WorkerError> {
        let mut state = self.state.lock().map_err(|_| WorkerError::StateCorrupted)?;
        if !matches!(state.phase, WorkerPhase::Active | WorkerPhase::Paused) {
            return Err(WorkerError::StateCorrupted);
        }
        let causal = if cancelled {
            state
                .pending_stop
                .take()
                .ok_or(WorkerError::StateCorrupted)?
        } else {
            driver
        };
        state.pending_pause = None;
        state.pending_stop = None;
        state.control = None;
        state.phase = WorkerPhase::Terminal;
        Ok(causal)
    }
}

struct QueuedEvent {
    envelope: Envelope<WorkerEvent>,
    framed_bytes: usize,
}

struct EventQueueState {
    queue: VecDeque<QueuedEvent>,
    queued_bytes: usize,
    next_sequence: Option<u64>,
    output_frame_bytes: usize,
    output_truncated: bool,
    terminal_admitted: bool,
    closed: bool,
}

struct EventQueue {
    state: Mutex<EventQueueState>,
    available: Condvar,
    space: Condvar,
}

#[derive(Clone)]
struct EventSink {
    session: WorkerSessionId,
    queue: Arc<EventQueue>,
}

impl EventSink {
    fn new(session: WorkerSessionId) -> Self {
        Self {
            session,
            queue: Arc::new(EventQueue {
                state: Mutex::new(EventQueueState {
                    queue: VecDeque::new(),
                    queued_bytes: 0,
                    next_sequence: Some(2),
                    output_frame_bytes: 0,
                    output_truncated: false,
                    terminal_admitted: false,
                    closed: false,
                }),
                available: Condvar::new(),
                space: Condvar::new(),
            }),
        }
    }

    fn output(&self, causal: CausalRequest, text: String) -> Result<(), WorkerError> {
        for chunk in split_output(&text) {
            if !self.try_output_chunk(causal, chunk.to_string())? {
                self.admit_output_truncated(causal)?;
                return Ok(());
            }
        }
        Ok(())
    }

    fn try_output_chunk(&self, causal: CausalRequest, text: String) -> Result<bool, WorkerError> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| WorkerError::StateCorrupted)?;
        if state.closed || state.terminal_admitted || state.output_truncated {
            return Ok(false);
        }
        let envelope = self.next_envelope(&state, causal, WorkerEvent::Output { text })?;
        let framed_bytes = framed_event_len(&envelope)?;
        let within_run_budget = state
            .output_frame_bytes
            .checked_add(framed_bytes)
            .is_some_and(|total| total <= MAX_RUN_OUTPUT_FRAME_BYTES);
        let within_queue = state.queue.len() < OUTPUT_QUEUE_ITEMS
            && state
                .queued_bytes
                .checked_add(framed_bytes)
                .is_some_and(|total| total <= NORMAL_EVENT_QUEUE_BYTES);
        if !within_run_budget || !within_queue {
            return Ok(false);
        }
        state.output_frame_bytes += framed_bytes;
        self.commit_envelope(&mut state, envelope, framed_bytes, false)?;
        Ok(true)
    }

    fn admit_output_truncated(&self, causal: CausalRequest) -> Result<(), WorkerError> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| WorkerError::StateCorrupted)?;
        if state.output_truncated || state.terminal_admitted || state.closed {
            return Ok(());
        }
        let envelope = self.next_envelope(&state, causal, WorkerEvent::OutputTruncated)?;
        let framed_bytes = framed_event_len(&envelope)?;
        if state.queue.len() >= EVENT_QUEUE_ITEMS - 1
            || state
                .queued_bytes
                .checked_add(framed_bytes)
                .is_none_or(|total| total > NORMAL_EVENT_QUEUE_BYTES)
        {
            return Err(WorkerError::StateCorrupted);
        }
        state.output_truncated = true;
        self.commit_envelope(&mut state, envelope, framed_bytes, false)
    }

    fn event(&self, causal: CausalRequest, event: WorkerEvent) -> Result<(), WorkerError> {
        let terminal = event.is_terminal();
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| WorkerError::StateCorrupted)?;
        if state.closed || state.terminal_admitted {
            return Err(WorkerError::StateCorrupted);
        }
        let envelope = self.next_envelope(&state, causal, event)?;
        let framed_bytes = framed_event_len(&envelope)?;
        if terminal {
            if state.queue.len() >= EVENT_QUEUE_ITEMS
                || state
                    .queued_bytes
                    .checked_add(framed_bytes)
                    .is_none_or(|total| total > EVENT_QUEUE_BYTES)
            {
                return Err(WorkerError::StateCorrupted);
            }
        } else {
            while state.queue.len() >= EVENT_QUEUE_ITEMS - 1
                || state
                    .queued_bytes
                    .checked_add(framed_bytes)
                    .is_none_or(|total| total > NORMAL_EVENT_QUEUE_BYTES)
            {
                state = self
                    .queue
                    .space
                    .wait(state)
                    .map_err(|_| WorkerError::StateCorrupted)?;
                if state.closed || state.terminal_admitted {
                    return Err(WorkerError::StateCorrupted);
                }
            }
        }
        self.commit_envelope(&mut state, envelope, framed_bytes, terminal)
    }

    fn rejection(
        &self,
        causal: CausalRequest,
        code: &'static str,
        message: &'static str,
    ) -> Result<(), WorkerError> {
        self.event(
            causal,
            WorkerEvent::CommandRejected {
                code: code.to_string(),
                message: message.to_string(),
            },
        )
    }

    fn next_envelope(
        &self,
        state: &EventQueueState,
        causal: CausalRequest,
        payload: WorkerEvent,
    ) -> Result<Envelope<WorkerEvent>, WorkerError> {
        let sequence = state.next_sequence.ok_or(WorkerError::StateCorrupted)?;
        if sequence == u64::MAX && !payload.is_terminal() {
            return Err(WorkerError::StateCorrupted);
        }
        Ok(Envelope {
            version: PROTOCOL_VERSION,
            worker_session_id: self.session,
            run_id: causal.run_id,
            source_revision: causal.revision,
            request_id: causal.request_id,
            sequence: EventSequence(sequence),
            payload,
        })
    }

    fn commit_envelope(
        &self,
        state: &mut EventQueueState,
        envelope: Envelope<WorkerEvent>,
        framed_bytes: usize,
        terminal: bool,
    ) -> Result<(), WorkerError> {
        let next = envelope.sequence.0.checked_add(1);
        if !terminal && next.is_none() {
            return Err(WorkerError::StateCorrupted);
        }
        state.next_sequence = if terminal { None } else { next };
        state.queued_bytes = state
            .queued_bytes
            .checked_add(framed_bytes)
            .ok_or(WorkerError::StateCorrupted)?;
        state.queue.push_back(QueuedEvent {
            envelope,
            framed_bytes,
        });
        state.terminal_admitted |= terminal;
        self.queue.available.notify_one();
        Ok(())
    }

    fn close(&self) -> Result<(), WorkerError> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| WorkerError::StateCorrupted)?;
        state.closed = true;
        self.queue.available.notify_all();
        self.queue.space.notify_all();
        Ok(())
    }

    fn fail(&self) {
        if let Ok(mut state) = self.queue.state.lock() {
            state.closed = true;
            state.queue.clear();
            state.queued_bytes = 0;
            self.queue.available.notify_all();
            self.queue.space.notify_all();
        }
    }
}

struct WorkerHost {
    sink: EventSink,
    driver: Arc<Mutex<CausalRequest>>,
    failed: Arc<Mutex<Option<WorkerError>>>,
}

impl WorkerHost {
    fn record(&self, result: Result<(), WorkerError>) {
        if let Err(error) = result
            && let Ok(mut failed) = self.failed.lock()
            && failed.is_none()
        {
            *failed = Some(error);
        }
    }

    fn causal(&self) -> Result<CausalRequest, WorkerError> {
        self.driver
            .lock()
            .map(|value| *value)
            .map_err(|_| WorkerError::StateCorrupted)
    }
}

impl RuntimeHost for WorkerHost {
    fn output(&mut self, mut text: String) {
        text.push('\n');
        let result = self
            .causal()
            .and_then(|causal| self.sink.output(causal, text));
        self.record(result);
    }

    fn diagnostic(&mut self, value: Diagnostic) {
        let result = self.causal().and_then(|causal| {
            self.sink.event(
                causal,
                WorkerEvent::Diagnostic {
                    diagnostic: WireDiagnostic::from(value),
                },
            )
        });
        self.record(result);
    }
}

pub fn run_worker<R, W>(
    input: R,
    mut output: W,
    worker_session_id: WorkerSessionId,
) -> Result<(), WorkerError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    if worker_session_id.0 == 0 {
        return Err(WorkerError::ZeroSession);
    }

    let hello = Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id,
        run_id: RunId(0),
        source_revision: RevisionId(0),
        request_id: RequestId(0),
        sequence: EventSequence(1),
        payload: WorkerEvent::Hello,
    };
    LineCodec::new()
        .write_worker_event(&mut output, &hello)
        .map_err(WorkerError::Encode)?;
    output
        .flush()
        .map_err(|error| WorkerError::Io(error.kind()))?;

    let sink = EventSink::new(worker_session_id);
    let control = Arc::new(ControlPlane::default());
    let (action_tx, action_rx) = mpsc::sync_channel(16);

    let writer_sink = sink.clone();
    let writer_control = control.clone();
    let writer_actions = action_tx.clone();
    let writer = thread::spawn(move || {
        let result = writer_loop(output, &writer_sink);
        if result.is_err() {
            writer_control.abort();
            writer_sink.fail();
            let _ = writer_actions.send(OwnerAction::WriterFailed);
        }
        result
    });

    let reader_sink = sink.clone();
    let reader_control = control.clone();
    let reader_actions = action_tx.clone();
    let reader = thread::spawn(move || {
        reader_loop(
            input,
            worker_session_id,
            &reader_control,
            &reader_sink,
            &reader_actions,
        )
    });
    drop(action_tx);

    let owner_result = owner_loop(&action_rx, &control, &sink);
    let _ = sink.close();
    let writer_result = writer.join().map_err(|_| WorkerError::ThreadPanicked)?;

    if reader.is_finished() {
        let reader_result = reader.join().map_err(|_| WorkerError::ThreadPanicked)?;
        reader_result?;
    }
    writer_result?;
    owner_result
}

fn owner_loop(
    actions: &mpsc::Receiver<OwnerAction>,
    control: &ControlPlane,
    sink: &EventSink,
) -> Result<(), WorkerError> {
    let first = actions.recv().map_err(|_| WorkerError::ChannelClosed)?;
    let OwnerAction::Load {
        document,
        debugging,
        driver,
    } = first
    else {
        return match first {
            OwnerAction::Shutdown | OwnerAction::EndOfInput => Ok(()),
            _ => Err(WorkerError::ChannelClosed),
        };
    };

    let source = document
        .into_source_document()
        .map_err(|_| WorkerError::InvalidCommandStream)?;
    let driver_slot = Arc::new(Mutex::new(driver));
    let host_failure = Arc::new(Mutex::new(None));
    let host = WorkerHost {
        sink: sink.clone(),
        driver: driver_slot.clone(),
        failed: host_failure.clone(),
    };
    let mut session = InterpreterSession::new(source, host);
    control.install_control(session.control())?;
    let mut current_driver = driver;
    let mut outcome = if debugging {
        session.start_debugging()
    } else {
        session.run_all()
    };

    loop {
        if let Some(error) = *host_failure
            .lock()
            .map_err(|_| WorkerError::StateCorrupted)?
        {
            return Err(error);
        }
        if control.is_fatal()? {
            return Err(WorkerError::InvalidCommandStream);
        }
        match outcome {
            RunOutcome::Paused(reason) => {
                match control.commit_pause(reason, current_driver)? {
                    PauseCommit::StopWon => {
                        outcome = session.resume(ResumeMode::Continue);
                        continue;
                    }
                    PauseCommit::Emit(causal) => {
                        let location = *session
                            .pause_location()
                            .ok_or(WorkerError::StateCorrupted)?;
                        let snapshot = session
                            .snapshot()
                            .cloned()
                            .ok_or(WorkerError::StateCorrupted)?;
                        sink.event(causal, WorkerEvent::Paused { location, snapshot })?;
                    }
                }

                match actions.recv().map_err(|_| WorkerError::ChannelClosed)? {
                    OwnerAction::Resume { mode, driver } => {
                        current_driver = driver;
                        *driver_slot
                            .lock()
                            .map_err(|_| WorkerError::StateCorrupted)? = driver;
                        outcome = session.resume(mode);
                    }
                    OwnerAction::CancelPaused => {
                        outcome = session.resume(ResumeMode::Continue);
                    }
                    OwnerAction::WriterFailed | OwnerAction::Fatal => {
                        return Err(WorkerError::ChannelClosed);
                    }
                    OwnerAction::EndOfInput => return Ok(()),
                    _ => return Err(WorkerError::StateCorrupted),
                }
            }
            RunOutcome::Completed => {
                let causal = control.commit_terminal(false, current_driver)?;
                sink.event(causal, WorkerEvent::Completed)?;
                return wait_for_shutdown(actions);
            }
            RunOutcome::Faulted(ref diagnostic) => {
                let causal = control.commit_terminal(false, current_driver)?;
                let snapshot = session
                    .snapshot()
                    .cloned()
                    .ok_or(WorkerError::StateCorrupted)?;
                sink.event(
                    causal,
                    WorkerEvent::Faulted {
                        diagnostic: WireDiagnostic::from(diagnostic),
                        snapshot,
                    },
                )?;
                return wait_for_shutdown(actions);
            }
            RunOutcome::Cancelled => {
                let causal = control.commit_terminal(true, current_driver)?;
                let snapshot = session
                    .snapshot()
                    .cloned()
                    .ok_or(WorkerError::StateCorrupted)?;
                sink.event(causal, WorkerEvent::Cancelled { snapshot })?;
                return wait_for_shutdown(actions);
            }
            RunOutcome::Rejected(_) => return Err(WorkerError::StateCorrupted),
        }
    }
}

fn wait_for_shutdown(actions: &mpsc::Receiver<OwnerAction>) -> Result<(), WorkerError> {
    match actions.recv().map_err(|_| WorkerError::ChannelClosed)? {
        OwnerAction::Shutdown | OwnerAction::EndOfInput => Ok(()),
        OwnerAction::WriterFailed | OwnerAction::Fatal => Err(WorkerError::ChannelClosed),
        _ => Err(WorkerError::StateCorrupted),
    }
}

fn reader_loop<R: Read>(
    input: R,
    worker_session_id: WorkerSessionId,
    control: &ControlPlane,
    sink: &EventSink,
    actions: &mpsc::SyncSender<OwnerAction>,
) -> Result<(), WorkerError> {
    let mut input = BufReader::new(input);
    let mut codec = LineCodec::new();
    let mut validator = CommandStreamValidator::new(worker_session_id)
        .map_err(|_| WorkerError::InvalidCommandStream)?;
    loop {
        let envelope = match codec.read_command(&mut input) {
            Ok(Some(envelope)) => envelope,
            Ok(None) => {
                let active = control
                    .state
                    .lock()
                    .map(|state| matches!(state.phase, WorkerPhase::Active | WorkerPhase::Paused))
                    .unwrap_or(true);
                if active {
                    control.abort();
                }
                let _ = actions.send(OwnerAction::EndOfInput);
                return Ok(());
            }
            Err(error) => {
                control.abort();
                let _ = actions.send(OwnerAction::Fatal);
                return Err(WorkerError::Decode(error));
            }
        };
        if validator.validate(&envelope).is_err() {
            control.abort();
            let _ = actions.send(OwnerAction::Fatal);
            return Err(WorkerError::InvalidCommandStream);
        }
        if !admit_command(envelope, control, sink, actions)? {
            return Ok(());
        }
    }
}

fn admit_command(
    envelope: Envelope<Command>,
    control: &ControlPlane,
    sink: &EventSink,
    actions: &mpsc::SyncSender<OwnerAction>,
) -> Result<bool, WorkerError> {
    let causal = CausalRequest::from_command(&envelope);
    let mut rejection = None;
    let mut action = None;
    let mut keep_reading = true;
    {
        let mut state = control
            .state
            .lock()
            .map_err(|_| WorkerError::StateCorrupted)?;
        match state.phase {
            WorkerPhase::AwaitLoad => match envelope.payload {
                Command::LoadAndRun { document } => {
                    state.active = Some(ActiveRun {
                        run_id: envelope.run_id,
                        revision: envelope.source_revision,
                        source_id: document.source_id,
                    });
                    state.phase = WorkerPhase::Active;
                    action = Some(OwnerAction::Load {
                        document,
                        debugging: false,
                        driver: causal,
                    });
                }
                Command::LoadAndDebug { document } => {
                    state.active = Some(ActiveRun {
                        run_id: envelope.run_id,
                        revision: envelope.source_revision,
                        source_id: document.source_id,
                    });
                    state.phase = WorkerPhase::Active;
                    action = Some(OwnerAction::Load {
                        document,
                        debugging: true,
                        driver: causal,
                    });
                }
                Command::Shutdown => {
                    if envelope.run_id.0 != 0 || envelope.source_revision.0 != 0 {
                        action = Some(OwnerAction::Fatal);
                    } else {
                        state.phase = WorkerPhase::Closed;
                        action = Some(OwnerAction::Shutdown);
                    }
                    keep_reading = false;
                }
                _ => {
                    if envelope.run_id.0 != 0 || envelope.source_revision.0 != 0 {
                        action = Some(OwnerAction::Fatal);
                        keep_reading = false;
                    } else {
                        rejection = Some((
                            "command.invalid_state",
                            "Load a program before using execution controls.",
                        ));
                    }
                }
            },
            WorkerPhase::Active => {
                if matches!(
                    envelope.payload,
                    Command::LoadAndRun { .. } | Command::LoadAndDebug { .. }
                ) {
                    rejection = Some((
                        "command.run_already_loaded",
                        "This worker already owns a run.",
                    ));
                } else {
                    let active = state.active.ok_or(WorkerError::StateCorrupted)?;
                    if !active.matches(&envelope) {
                        action = Some(OwnerAction::Fatal);
                        keep_reading = false;
                    } else {
                        match envelope.payload {
                            Command::Pause => {
                                if state.pending_pause.is_some() || state.pending_stop.is_some() {
                                    rejection = Some((
                                        "command.control_pending",
                                        "A pause or stop request is already pending.",
                                    ));
                                } else {
                                    state.pending_pause = Some(causal);
                                    if let Some(value) = &state.control {
                                        value.request_pause();
                                    }
                                }
                            }
                            Command::Stop => {
                                if state.pending_stop.is_some() {
                                    rejection = Some((
                                        "command.stop_pending",
                                        "A stop request is already pending.",
                                    ));
                                } else {
                                    state.pending_stop = Some(causal);
                                    state.pending_pause = None;
                                    if let Some(value) = &state.control {
                                        value.request_cancel();
                                    }
                                }
                            }
                            Command::Continue
                            | Command::StepInto
                            | Command::StepOver
                            | Command::StepOut => {
                                rejection = Some((
                                    "command.not_paused",
                                    "Resume and step controls require a paused run.",
                                ));
                            }
                            Command::ProvideInput { .. } => {
                                rejection = Some((
                                    "command.input_unsupported",
                                    "Program input is not supported by this runtime.",
                                ));
                            }
                            Command::Shutdown => {
                                action = Some(OwnerAction::Fatal);
                                keep_reading = false;
                            }
                            Command::LoadAndRun { .. } | Command::LoadAndDebug { .. } => {
                                unreachable!()
                            }
                        }
                    }
                }
            }
            WorkerPhase::Paused => {
                if matches!(
                    envelope.payload,
                    Command::LoadAndRun { .. } | Command::LoadAndDebug { .. }
                ) {
                    rejection = Some((
                        "command.run_already_loaded",
                        "This worker already owns a run.",
                    ));
                } else {
                    let active = state.active.ok_or(WorkerError::StateCorrupted)?;
                    if !active.matches(&envelope) {
                        action = Some(OwnerAction::Fatal);
                        keep_reading = false;
                    } else {
                        match envelope.payload {
                            Command::Continue
                            | Command::StepInto
                            | Command::StepOver
                            | Command::StepOut => {
                                if state.pending_stop.is_some() {
                                    rejection = Some((
                                        "command.stop_pending",
                                        "A stop request is already pending.",
                                    ));
                                } else {
                                    let mode = match envelope.payload {
                                        Command::Continue => ResumeMode::Continue,
                                        Command::StepInto => ResumeMode::StepInto,
                                        Command::StepOver => ResumeMode::StepOver,
                                        Command::StepOut => ResumeMode::StepOut,
                                        _ => unreachable!(),
                                    };
                                    state.phase = WorkerPhase::Active;
                                    action = Some(OwnerAction::Resume {
                                        mode,
                                        driver: causal,
                                    });
                                }
                            }
                            Command::Stop => {
                                if state.pending_stop.is_some() {
                                    rejection = Some((
                                        "command.stop_pending",
                                        "A stop request is already pending.",
                                    ));
                                } else {
                                    state.pending_stop = Some(causal);
                                    state.pending_pause = None;
                                    state
                                        .control
                                        .as_ref()
                                        .ok_or(WorkerError::StateCorrupted)?
                                        .request_cancel();
                                    state.phase = WorkerPhase::Active;
                                    action = Some(OwnerAction::CancelPaused);
                                }
                            }
                            Command::Pause => {
                                rejection =
                                    Some(("command.already_paused", "The run is already paused."));
                            }
                            Command::ProvideInput { .. } => {
                                rejection = Some((
                                    "command.input_unsupported",
                                    "Program input is not supported by this runtime.",
                                ));
                            }
                            Command::Shutdown => {
                                action = Some(OwnerAction::Fatal);
                                keep_reading = false;
                            }
                            Command::LoadAndRun { .. } | Command::LoadAndDebug { .. } => {
                                unreachable!()
                            }
                        }
                    }
                }
            }
            WorkerPhase::Terminal => {
                let active = state.active.ok_or(WorkerError::StateCorrupted)?;
                if matches!(envelope.payload, Command::Shutdown) {
                    if active.matches(&envelope) {
                        state.phase = WorkerPhase::Closed;
                        action = Some(OwnerAction::Shutdown);
                    } else {
                        action = Some(OwnerAction::Fatal);
                    }
                    keep_reading = false;
                } else {
                    keep_reading = true;
                }
            }
            WorkerPhase::Closed => return Err(WorkerError::StateCorrupted),
        }
    }

    if let Some((code, message)) = rejection {
        sink.rejection(causal, code, message)?;
    }
    if let Some(value) = action {
        if matches!(&value, OwnerAction::Fatal) {
            control.abort();
        }
        actions
            .send(value)
            .map_err(|_| WorkerError::ChannelClosed)?;
    }
    Ok(keep_reading)
}

fn writer_loop<W: Write>(mut output: W, sink: &EventSink) -> Result<(), WorkerError> {
    let mut codec = LineCodec::new();
    loop {
        let queued = {
            let mut state = sink
                .queue
                .state
                .lock()
                .map_err(|_| WorkerError::StateCorrupted)?;
            while state.queue.is_empty() && !state.closed {
                state = sink
                    .queue
                    .available
                    .wait(state)
                    .map_err(|_| WorkerError::StateCorrupted)?;
            }
            let Some(queued) = state.queue.pop_front() else {
                break;
            };
            state.queued_bytes -= queued.framed_bytes;
            sink.queue.space.notify_all();
            queued
        };
        codec
            .write_worker_event(&mut output, &queued.envelope)
            .map_err(WorkerError::Encode)?;
        output
            .flush()
            .map_err(|error| WorkerError::Io(error.kind()))?;
    }
    output
        .flush()
        .map_err(|error| WorkerError::Io(error.kind()))
}

fn framed_event_len(envelope: &Envelope<WorkerEvent>) -> Result<usize, WorkerError> {
    LineCodec::new()
        .worker_event_payload_len(envelope)
        .map_err(WorkerError::Encode)?
        .checked_add(1)
        .ok_or(WorkerError::StateCorrupted)
}

fn split_output(value: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= value.len() {
            return None;
        }
        let mut end = (start + MAX_OUTPUT_CHUNK_TEXT_BYTES).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &value[start..end];
        start = end;
        Some(chunk)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_chunks_respect_utf8_boundary_and_limit() {
        let text = format!("{}🙂tail", "x".repeat(MAX_OUTPUT_CHUNK_TEXT_BYTES - 1));
        let chunks: Vec<_> = split_output(&text).collect();
        assert_eq!(chunks.concat(), text);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MAX_OUTPUT_CHUNK_TEXT_BYTES)
        );
        assert_eq!(chunks.len(), 2);
    }
}
