use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rlox::{PauseReason, RevisionId, SnapshotReason, SourceId};

use crate::protocol::{
    Command, CommandStreamValidator, DecodeError, EncodeError, Envelope, EventSequence, LineCodec,
    MAX_CONTROL_TEXT_BYTES, MAX_PAYLOAD_BYTES, MAX_RUN_OUTPUT_FRAME_BYTES, PROTOCOL_VERSION,
    RequestId, RunId, StreamValidationError, WireDocument, WorkerEvent, WorkerEventStreamValidator,
    WorkerSessionId,
};

const ACTOR_QUEUE_ITEMS: usize = 256;
const WRITER_QUEUE_ITEMS: usize = 32;
const STDERR_RETAIN_BYTES: usize = 64 * 1024;
const INBOX_NORMAL_BYTES: usize = 24 * 1024 * 1024;
const INBOX_TOTAL_BYTES: usize = 33 * 1024 * 1024;
const SUBMISSION_QUEUE_BYTES: usize = 2 * MAX_PAYLOAD_BYTES;
const ACTOR_TICK: Duration = Duration::from_millis(10);

const STATE_BOOTING: u8 = 0;
const STATE_AWAIT_LOAD: u8 = 1;
const STATE_ACTIVE: u8 = 2;
const STATE_PAUSED: u8 = 3;
const STATE_TERMINAL: u8 = 4;
const STATE_CLOSING: u8 = 5;
const STATE_CLOSED: u8 = 6;

static NEXT_WORKER_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    pub executable: PathBuf,
    pub handshake_timeout: Duration,
    pub stop_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl SupervisorConfig {
    pub fn current_executable() -> Result<Self, SupervisorStartError> {
        let executable =
            std::env::current_exe().map_err(|error| SupervisorStartError { kind: error.kind() })?;
        Ok(Self {
            executable,
            handshake_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(2),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupervisorStartError {
    pub kind: std::io::ErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SupervisorSubmissionId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorSource {
    pub display_name: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorRunMode {
    Run,
    Debug,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupervisorRun {
    pub worker_session_id: WorkerSessionId,
    pub run_id: RunId,
    pub source_id: SourceId,
    pub source_revision: RevisionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorCommand {
    Start {
        mode: SupervisorRunMode,
        source: SupervisorSource,
    },
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
    ProvideInput {
        in_reply_to: RequestId,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorCommandKind {
    Start,
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
    ProvideInput,
}

impl SupervisorCommand {
    fn kind(&self) -> SupervisorCommandKind {
        match self {
            Self::Start { .. } => SupervisorCommandKind::Start,
            Self::Pause => SupervisorCommandKind::Pause,
            Self::Continue => SupervisorCommandKind::Continue,
            Self::StepInto => SupervisorCommandKind::StepInto,
            Self::StepOver => SupervisorCommandKind::StepOver,
            Self::StepOut => SupervisorCommandKind::StepOut,
            Self::Stop => SupervisorCommandKind::Stop,
            Self::ProvideInput { .. } => SupervisorCommandKind::ProvideInput,
        }
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Start { source, .. } => source
                .text
                .len()
                .saturating_add(source.display_name.len())
                .saturating_add(256),
            Self::ProvideInput { text, .. } => text.len().saturating_add(128),
            _ => 64,
        }
    }

    fn is_valid(&self) -> bool {
        match self {
            Self::Start { source, .. } => WireDocument {
                source_id: SourceId(1),
                revision: RevisionId(1),
                display_name: source.display_name.clone(),
                text: source.text.clone(),
            }
            .validate()
            .is_ok(),
            Self::ProvideInput { in_reply_to, text } => {
                in_reply_to.0 != 0 && text.len() <= MAX_CONTROL_TEXT_BYTES
            }
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    InvalidSubmissionId,
    NotReady,
    InvalidState,
    Full,
    Terminal,
    Closed,
    Poisoned,
    InvalidCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorPollError {
    Poisoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct StderrSummary {
    pub retained: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerTerminationReason {
    HandshakeTimeout,
    EofBeforeHello,
    EofBeforeTerminal,
    StdoutDecode(DecodeError),
    StdoutProtocol(StreamValidationError),
    CausalityViolation,
    CommandWrite(EncodeError),
    WriterClosed,
    RequestExhausted,
    EventInboxExceeded,
    StopTimeout,
    UnexpectedExit(Option<i32>),
    Kill(std::io::ErrorKind),
    Wait(std::io::ErrorKind),
    SupervisorClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosureHealth {
    Clean,
    NonzeroExit,
    ProtocolAfterTerminal,
    ShutdownTimeout,
    IoFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorEvent {
    Worker(Box<Envelope<WorkerEvent>>),
    Started {
        submission_id: SupervisorSubmissionId,
        mode: SupervisorRunMode,
        run: SupervisorRun,
        request_id: RequestId,
        next_event_sequence: EventSequence,
    },
    SubmissionAccepted {
        submission_id: SupervisorSubmissionId,
        command: SupervisorCommandKind,
        run: SupervisorRun,
        request_id: RequestId,
        next_event_sequence: EventSequence,
    },
    CloseAccepted {
        run: SupervisorRun,
        request_id: RequestId,
        next_event_sequence: EventSequence,
    },
    SubmissionRejected {
        submission_id: SupervisorSubmissionId,
        command: SupervisorCommandKind,
        error: SubmitError,
    },
    WorkerTerminated {
        run: Option<SupervisorRun>,
        reason: WorkerTerminationReason,
        stderr: StderrSummary,
    },
    Closed {
        run: Option<SupervisorRun>,
        status: Option<i32>,
        stderr: StderrSummary,
        health: ClosureHealth,
    },
}

struct QueuedSupervisorEvent {
    event: SupervisorEvent,
    bytes: usize,
}

#[derive(Default)]
struct SupervisorInboxState {
    events: VecDeque<QueuedSupervisorEvent>,
    bytes: usize,
}

#[derive(Default)]
struct SupervisorInbox {
    state: Mutex<SupervisorInboxState>,
}

impl SupervisorInbox {
    fn push(&self, event: SupervisorEvent) -> Result<(), SupervisorPollError> {
        let bytes = supervisor_event_size(&event);
        let terminal = match &event {
            SupervisorEvent::Worker(envelope) => envelope.payload.is_terminal(),
            SupervisorEvent::WorkerTerminated { .. } | SupervisorEvent::Closed { .. } => true,
            SupervisorEvent::Started { .. }
            | SupervisorEvent::SubmissionAccepted { .. }
            | SupervisorEvent::CloseAccepted { .. }
            | SupervisorEvent::SubmissionRejected { .. } => false,
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| SupervisorPollError::Poisoned)?;
        let maximum = if terminal {
            INBOX_TOTAL_BYTES
        } else {
            INBOX_NORMAL_BYTES
        };
        if state
            .bytes
            .checked_add(bytes)
            .is_none_or(|total| total > maximum)
        {
            return Err(SupervisorPollError::Poisoned);
        }
        state.bytes += bytes;
        state
            .events
            .push_back(QueuedSupervisorEvent { event, bytes });
        Ok(())
    }

    fn pop(&self) -> Result<Option<SupervisorEvent>, SupervisorPollError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SupervisorPollError::Poisoned)?;
        let Some(value) = state.events.pop_front() else {
            return Ok(None);
        };
        state.bytes -= value.bytes;
        Ok(Some(value.event))
    }
}

fn supervisor_event_size(event: &SupervisorEvent) -> usize {
    match event {
        SupervisorEvent::Worker(envelope) => LineCodec::new()
            .worker_event_payload_len(envelope)
            .unwrap_or(INBOX_TOTAL_BYTES)
            .saturating_add(1),
        SupervisorEvent::Started { .. }
        | SupervisorEvent::SubmissionAccepted { .. }
        | SupervisorEvent::CloseAccepted { .. }
        | SupervisorEvent::SubmissionRejected { .. } => 128,
        SupervisorEvent::WorkerTerminated { stderr, .. }
        | SupervisorEvent::Closed { stderr, .. } => stderr.retained.len().saturating_add(256),
    }
}

#[derive(Default)]
struct SubmissionBudget {
    bytes: Mutex<usize>,
}

impl SubmissionBudget {
    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Result<SubmissionPermit, SubmitError> {
        if bytes > SUBMISSION_QUEUE_BYTES {
            return Err(SubmitError::Full);
        }
        let mut retained = self.bytes.lock().map_err(|_| SubmitError::Poisoned)?;
        if retained
            .checked_add(bytes)
            .is_none_or(|total| total > SUBMISSION_QUEUE_BYTES)
        {
            return Err(SubmitError::Full);
        }
        *retained += bytes;
        Ok(SubmissionPermit {
            budget: self.clone(),
            bytes,
        })
    }
}

struct SubmissionPermit {
    budget: Arc<SubmissionBudget>,
    bytes: usize,
}

impl Drop for SubmissionPermit {
    fn drop(&mut self) {
        if let Ok(mut retained) = self.budget.bytes.lock() {
            *retained = retained.saturating_sub(self.bytes);
        }
    }
}

struct QueuedSupervisorCommand {
    submission_id: SupervisorSubmissionId,
    command: SupervisorCommand,
    _permit: SubmissionPermit,
}

#[derive(Clone)]
pub struct WorkerCommandSender {
    actor: mpsc::SyncSender<ActorMessage>,
    state: Arc<AtomicU8>,
    close_requested: Arc<AtomicBool>,
    submission_budget: Arc<SubmissionBudget>,
    last_submission_id: Arc<AtomicU64>,
}

impl WorkerCommandSender {
    pub fn try_send(
        &self,
        submission_id: SupervisorSubmissionId,
        command: SupervisorCommand,
    ) -> Result<(), SubmitError> {
        claim_submission_id(&self.last_submission_id, submission_id)?;
        let state = self.state.load(Ordering::Acquire);
        if state == STATE_CLOSED || state == STATE_CLOSING {
            return Err(SubmitError::Closed);
        }
        if state == STATE_TERMINAL {
            return Err(SubmitError::Terminal);
        }
        if !command_allowed_locally(state, &command) {
            return Err(if state == STATE_BOOTING {
                SubmitError::NotReady
            } else {
                SubmitError::InvalidState
            });
        }
        if !command.is_valid() {
            return Err(SubmitError::InvalidCommand);
        }
        let permit = self
            .submission_budget
            .try_reserve(command.retained_bytes())?;
        self.actor
            .try_send(ActorMessage::Submit(QueuedSupervisorCommand {
                submission_id,
                command,
                _permit: permit,
            }))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => SubmitError::Full,
                mpsc::TrySendError::Disconnected(_) => SubmitError::Closed,
            })
    }
}

fn claim_submission_id(
    last_submission_id: &AtomicU64,
    submission_id: SupervisorSubmissionId,
) -> Result<(), SubmitError> {
    if submission_id.0 == 0 {
        return Err(SubmitError::InvalidSubmissionId);
    }
    let mut last = last_submission_id.load(Ordering::Acquire);
    loop {
        if submission_id.0 <= last {
            return Err(SubmitError::InvalidSubmissionId);
        }
        match last_submission_id.compare_exchange_weak(
            last,
            submission_id.0,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(()),
            Err(actual) => last = actual,
        }
    }
}

fn command_allowed_locally(state: u8, command: &SupervisorCommand) -> bool {
    match state {
        STATE_BOOTING | STATE_AWAIT_LOAD => matches!(command, SupervisorCommand::Start { .. }),
        STATE_ACTIVE => matches!(
            command,
            SupervisorCommand::Pause
                | SupervisorCommand::Stop
                | SupervisorCommand::ProvideInput { .. }
        ),
        STATE_PAUSED => matches!(
            command,
            SupervisorCommand::Continue
                | SupervisorCommand::StepInto
                | SupervisorCommand::StepOver
                | SupervisorCommand::StepOut
                | SupervisorCommand::Stop
                | SupervisorCommand::ProvideInput { .. }
        ),
        _ => false,
    }
}

pub struct WorkerSupervisor {
    sender: WorkerCommandSender,
    inbox: Arc<SupervisorInbox>,
    session: WorkerSessionId,
}

impl WorkerSupervisor {
    pub fn launch(config: SupervisorConfig) -> Result<Self, SupervisorStartError> {
        let now = Instant::now();
        if config.handshake_timeout.is_zero()
            || config.stop_timeout.is_zero()
            || config.shutdown_timeout.is_zero()
            || now.checked_add(config.handshake_timeout).is_none()
            || now.checked_add(config.stop_timeout).is_none()
            || now.checked_add(config.shutdown_timeout).is_none()
        {
            return Err(SupervisorStartError {
                kind: std::io::ErrorKind::InvalidInput,
            });
        }
        let session = allocate_worker_session()?;
        let mut child = ProcessCommand::new(&config.executable)
            .args(["--worker", "--worker-session", &session.0.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| SupervisorStartError { kind: error.kind() })?;

        let Some(stdin) = child.stdin.take() else {
            cleanup_failed_launch(&mut child);
            return Err(SupervisorStartError {
                kind: std::io::ErrorKind::BrokenPipe,
            });
        };
        let Some(stdout) = child.stdout.take() else {
            cleanup_failed_launch(&mut child);
            return Err(SupervisorStartError {
                kind: std::io::ErrorKind::BrokenPipe,
            });
        };
        let Some(stderr) = child.stderr.take() else {
            cleanup_failed_launch(&mut child);
            return Err(SupervisorStartError {
                kind: std::io::ErrorKind::BrokenPipe,
            });
        };

        let (actor_tx, actor_rx) = mpsc::sync_channel(ACTOR_QUEUE_ITEMS);
        let (writer_tx, writer_rx) = mpsc::sync_channel(WRITER_QUEUE_ITEMS);
        let inbox = Arc::new(SupervisorInbox::default());
        let state = Arc::new(AtomicU8::new(STATE_BOOTING));
        let close_requested = Arc::new(AtomicBool::new(false));
        let submission_budget = Arc::new(SubmissionBudget::default());
        let last_submission_id = Arc::new(AtomicU64::new(0));

        let writer_actor = actor_tx.clone();
        let writer = thread::spawn(move || writer_thread(stdin, session, writer_rx, writer_actor));
        let stdout_actor = actor_tx.clone();
        let stdout_reader = thread::spawn(move || stdout_thread(stdout, stdout_actor));
        let stderr_actor = actor_tx.clone();
        let stderr_reader = thread::spawn(move || stderr_thread(stderr, stderr_actor));

        let actor_inbox = inbox.clone();
        let actor_state = state.clone();
        let actor_close_requested = close_requested.clone();
        thread::spawn(move || {
            supervisor_actor(
                child,
                session,
                config,
                actor_rx,
                writer_tx,
                actor_inbox,
                actor_state,
                actor_close_requested,
                writer,
                stdout_reader,
                stderr_reader,
            );
        });

        Ok(Self {
            sender: WorkerCommandSender {
                actor: actor_tx,
                state,
                close_requested,
                submission_budget,
                last_submission_id,
            },
            inbox,
            session,
        })
    }

    pub fn command_sender(&self) -> WorkerCommandSender {
        self.sender.clone()
    }

    pub fn worker_session_id(&self) -> WorkerSessionId {
        self.session
    }

    pub fn try_recv(&self) -> Result<Option<SupervisorEvent>, SupervisorPollError> {
        self.inbox.pop()
    }

    pub fn close(&self) -> Result<(), SubmitError> {
        let previous = self.sender.state.swap(STATE_CLOSING, Ordering::AcqRel);
        if previous == STATE_CLOSED {
            return Err(SubmitError::Closed);
        }
        self.sender.close_requested.store(true, Ordering::Release);
        match self.sender.actor.try_send(ActorMessage::Close) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(SubmitError::Closed),
        }
    }
}

impl Drop for WorkerSupervisor {
    fn drop(&mut self) {
        if self.sender.state.load(Ordering::Acquire) != STATE_CLOSED {
            self.sender.state.store(STATE_CLOSING, Ordering::Release);
            self.sender.close_requested.store(true, Ordering::Release);
            let _ = self.sender.actor.try_send(ActorMessage::Close);
        }
    }
}

fn allocate_worker_session() -> Result<WorkerSessionId, SupervisorStartError> {
    let value = NEXT_WORKER_SESSION.fetch_add(1, Ordering::Relaxed);
    if value == 0 || value == u64::MAX {
        return Err(SupervisorStartError {
            kind: std::io::ErrorKind::Other,
        });
    }
    Ok(WorkerSessionId(value))
}

fn cleanup_failed_launch(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

enum ActorMessage {
    Submit(QueuedSupervisorCommand),
    Close,
    StdoutEvent(Box<Envelope<WorkerEvent>>),
    StdoutEof,
    StdoutError(DecodeError),
    Writer(WriterNotice),
    Stderr(StderrSummary),
}

enum WriterNotice {
    Closed,
    Failed(EncodeError),
}

struct WriteRequest {
    envelope: Envelope<Command>,
    close_after: bool,
}

struct PreparedCommand {
    envelope: Envelope<Command>,
    request: RequestId,
    next_request: Option<u64>,
    next_sequence: Option<u64>,
}

fn writer_thread(
    mut stdin: ChildStdin,
    session: WorkerSessionId,
    requests: mpsc::Receiver<WriteRequest>,
    actor: mpsc::SyncSender<ActorMessage>,
) {
    let mut codec = LineCodec::new();
    let mut validator = match CommandStreamValidator::new(session) {
        Ok(value) => value,
        Err(_) => {
            let _ = actor.send(ActorMessage::Writer(WriterNotice::Closed));
            return;
        }
    };
    while let Ok(request) = requests.recv() {
        if validator.validate(&request.envelope).is_err() {
            let _ = actor.send(ActorMessage::Writer(WriterNotice::Closed));
            return;
        }
        if let Err(error) = codec.write_command(&mut stdin, &request.envelope) {
            let _ = actor.send(ActorMessage::Writer(WriterNotice::Failed(error)));
            return;
        }
        if request.close_after {
            break;
        }
    }
    drop(stdin);
    let _ = actor.send(ActorMessage::Writer(WriterNotice::Closed));
}

fn stdout_thread(stdout: ChildStdout, actor: mpsc::SyncSender<ActorMessage>) {
    let mut stdout = BufReader::new(stdout);
    let mut codec = LineCodec::new();
    loop {
        match codec.read_worker_event(&mut stdout) {
            Ok(Some(event)) => {
                if actor
                    .send(ActorMessage::StdoutEvent(Box::new(event)))
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) => {
                let _ = actor.send(ActorMessage::StdoutEof);
                return;
            }
            Err(error) => {
                let _ = actor.send(ActorMessage::StdoutError(error));
                return;
            }
        }
    }
}

fn stderr_thread(mut stderr: ChildStderr, actor: mpsc::SyncSender<ActorMessage>) {
    let mut summary = StderrSummary::default();
    let mut buffer = [0u8; 8192];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                summary.total_bytes = summary.total_bytes.saturating_add(read as u64);
                let remaining = STDERR_RETAIN_BYTES.saturating_sub(summary.retained.len());
                summary
                    .retained
                    .extend_from_slice(&buffer[..read.min(remaining)]);
                summary.truncated |= read > remaining;
            }
            Err(_) => {
                summary.truncated = true;
                break;
            }
        }
    }
    let _ = actor.send(ActorMessage::Stderr(summary));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActorPhase {
    Booting,
    AwaitLoad,
    Active,
    Paused,
    Terminal,
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IssuedKind {
    LoadRun,
    LoadDebug,
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
    ProvideInput,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueCommandError {
    InvalidState,
    IdExhausted,
    WriterFull,
    WriterClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IssuedCommand {
    kind: IssuedKind,
    run_id: RunId,
    revision: RevisionId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveRun {
    run_id: RunId,
    revision: RevisionId,
    source_id: SourceId,
    driver: RequestId,
}

impl ActiveRun {
    fn public(self, worker_session_id: WorkerSessionId) -> SupervisorRun {
        SupervisorRun {
            worker_session_id,
            run_id: self.run_id,
            source_id: self.source_id,
            source_revision: self.revision,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandAdmission {
    command: SupervisorCommandKind,
    mode: Option<SupervisorRunMode>,
    run: SupervisorRun,
    request_id: RequestId,
}

struct ActorState {
    phase: ActorPhase,
    event_validator: WorkerEventStreamValidator,
    next_worker_event_sequence: Option<EventSequence>,
    next_request: Option<u64>,
    next_sequence: Option<u64>,
    issued: HashMap<RequestId, IssuedCommand>,
    active: Option<ActiveRun>,
    pending_pause: Option<RequestId>,
    pending_stop: Option<RequestId>,
    output_bytes: usize,
    output_truncated: bool,
    terminal_seen: bool,
    stdout_done: bool,
    writer_done: bool,
    child_status: Option<Option<i32>>,
    stderr: Option<StderrSummary>,
    failure: Option<WorkerTerminationReason>,
    closure_health: ClosureHealth,
    handshake_deadline: Instant,
    stop_deadline: Option<Instant>,
    shutdown_deadline: Option<Instant>,
    close_requested: bool,
    killed: bool,
}

impl ActorState {
    fn new(
        session: WorkerSessionId,
        config: &SupervisorConfig,
    ) -> Result<Self, StreamValidationError> {
        Ok(Self {
            phase: ActorPhase::Booting,
            event_validator: WorkerEventStreamValidator::new(session)?,
            next_worker_event_sequence: Some(EventSequence(1)),
            next_request: Some(1),
            next_sequence: Some(1),
            issued: HashMap::new(),
            active: None,
            pending_pause: None,
            pending_stop: None,
            output_bytes: 0,
            output_truncated: false,
            terminal_seen: false,
            stdout_done: false,
            writer_done: false,
            child_status: None,
            stderr: None,
            failure: None,
            closure_health: ClosureHealth::Clean,
            handshake_deadline: deadline_after(config.handshake_timeout),
            stop_deadline: None,
            shutdown_deadline: None,
            close_requested: false,
            killed: false,
        })
    }

    fn fail(&mut self, reason: WorkerTerminationReason) {
        if self.failure.is_none() && !self.terminal_seen {
            self.failure = Some(reason);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn supervisor_actor(
    mut child: Child,
    session: WorkerSessionId,
    config: SupervisorConfig,
    messages: mpsc::Receiver<ActorMessage>,
    writer_tx: mpsc::SyncSender<WriteRequest>,
    inbox: Arc<SupervisorInbox>,
    public_state: Arc<AtomicU8>,
    close_requested: Arc<AtomicBool>,
    writer: thread::JoinHandle<()>,
    stdout_reader: thread::JoinHandle<()>,
    stderr_reader: thread::JoinHandle<()>,
) {
    let Ok(mut state) = ActorState::new(session, &config) else {
        cleanup_failed_launch(&mut child);
        public_state.store(STATE_CLOSED, Ordering::Release);
        return;
    };
    let mut pending: VecDeque<QueuedSupervisorCommand> = VecDeque::new();

    loop {
        match messages.recv_timeout(ACTOR_TICK) {
            Ok(message) => handle_actor_message(
                message,
                session,
                &config,
                &mut state,
                &writer_tx,
                &inbox,
                &public_state,
                &mut pending,
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                state.close_requested = true;
                state.fail(WorkerTerminationReason::SupervisorClosed);
            }
        }

        if close_requested.load(Ordering::Acquire) && !state.close_requested {
            handle_actor_message(
                ActorMessage::Close,
                session,
                &config,
                &mut state,
                &writer_tx,
                &inbox,
                &public_state,
                &mut pending,
            );
        }

        check_deadlines(&config, &mut state);
        if state.failure.is_some() || state.killed {
            ensure_killed_and_reaped(&mut child, &mut state);
        } else if state.child_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    state.child_status = Some(status.code());
                    if !state.terminal_seen && !state.close_requested {
                        state.fail(WorkerTerminationReason::UnexpectedExit(status.code()));
                    }
                }
                Ok(None) => {}
                Err(error) => state.fail(WorkerTerminationReason::Wait(error.kind())),
            }
        }

        if state.child_status.is_some() && state.stdout_done && state.stderr.is_some() {
            break;
        }
    }

    drop(writer_tx);
    let _ = writer.join();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    public_state.store(STATE_CLOSED, Ordering::Release);
    finalize_supervisor(session, &state, &inbox);
}

#[allow(clippy::too_many_arguments)]
fn handle_actor_message(
    message: ActorMessage,
    session: WorkerSessionId,
    config: &SupervisorConfig,
    state: &mut ActorState,
    writer: &mpsc::SyncSender<WriteRequest>,
    inbox: &SupervisorInbox,
    public_state: &AtomicU8,
    pending: &mut VecDeque<QueuedSupervisorCommand>,
) {
    match message {
        ActorMessage::Submit(submission) => {
            let submission_id = submission.submission_id;
            let command = submission.command.kind();
            if state.close_requested {
                reject_submission(submission_id, command, SubmitError::Closed, state, inbox);
                return;
            }
            if state.phase == ActorPhase::Booting {
                pending.push_back(submission);
            } else {
                match queue_supervisor_command(submission.command, session, config, state, writer) {
                    Ok(admission) => {
                        publish_phase(state.phase, public_state);
                        acknowledge_submission(submission_id, admission, state, inbox);
                    }
                    Err(QueueCommandError::InvalidState) => {
                        reject_submission(
                            submission_id,
                            command,
                            SubmitError::InvalidState,
                            state,
                            inbox,
                        );
                    }
                    Err(QueueCommandError::WriterFull) => {
                        reject_submission(submission_id, command, SubmitError::Full, state, inbox);
                    }
                    Err(QueueCommandError::IdExhausted) => {
                        state.fail(WorkerTerminationReason::RequestExhausted);
                    }
                    Err(QueueCommandError::WriterClosed) => {
                        state.fail(WorkerTerminationReason::WriterClosed);
                    }
                }
            }
        }
        ActorMessage::Close => {
            state.close_requested = true;
            public_state.store(STATE_CLOSING, Ordering::Release);
            match state.phase {
                ActorPhase::Booting => {}
                ActorPhase::AwaitLoad => {
                    if queue_shutdown(session, state, writer, config).is_err() {
                        state.fail(WorkerTerminationReason::WriterClosed);
                    }
                }
                ActorPhase::Active | ActorPhase::Paused => {
                    if state.pending_stop.is_none() {
                        match queue_supervisor_command(
                            SupervisorCommand::Stop,
                            session,
                            config,
                            state,
                            writer,
                        ) {
                            Ok(admission) => acknowledge_close(admission, state, inbox),
                            Err(QueueCommandError::IdExhausted) => {
                                state.fail(WorkerTerminationReason::RequestExhausted);
                            }
                            Err(QueueCommandError::InvalidState) => {
                                state.fail(WorkerTerminationReason::CausalityViolation);
                            }
                            Err(QueueCommandError::WriterFull)
                            | Err(QueueCommandError::WriterClosed) => {
                                state.fail(WorkerTerminationReason::WriterClosed);
                            }
                        }
                    }
                }
                ActorPhase::Terminal => {
                    if state.shutdown_deadline.is_none()
                        && queue_shutdown(session, state, writer, config).is_err()
                    {
                        state.closure_health = ClosureHealth::IoFailure;
                        state.killed = true;
                    }
                }
                ActorPhase::Closing => {}
            }
        }
        ActorMessage::StdoutEvent(event) => {
            if let Err(error) =
                accept_worker_event(*event, session, config, state, writer, inbox, public_state)
            {
                if state.terminal_seen {
                    state.closure_health = ClosureHealth::ProtocolAfterTerminal;
                    state.killed = true;
                } else {
                    state.fail(error);
                }
            } else if state.phase == ActorPhase::AwaitLoad {
                if state.close_requested {
                    while let Some(submission) = pending.pop_front() {
                        reject_submission(
                            submission.submission_id,
                            submission.command.kind(),
                            SubmitError::Closed,
                            state,
                            inbox,
                        );
                    }
                    if queue_shutdown(session, state, writer, config).is_err() {
                        state.fail(WorkerTerminationReason::WriterClosed);
                    }
                    publish_phase(state.phase, public_state);
                } else {
                    while let Some(submission) = pending.pop_front() {
                        let submission_id = submission.submission_id;
                        let command = submission.command.kind();
                        match queue_supervisor_command(
                            submission.command,
                            session,
                            config,
                            state,
                            writer,
                        ) {
                            Ok(admission) => {
                                publish_phase(state.phase, public_state);
                                acknowledge_submission(submission_id, admission, state, inbox);
                            }
                            Err(QueueCommandError::InvalidState) => {
                                reject_submission(
                                    submission_id,
                                    command,
                                    SubmitError::InvalidState,
                                    state,
                                    inbox,
                                );
                            }
                            Err(QueueCommandError::WriterFull) => {
                                reject_submission(
                                    submission_id,
                                    command,
                                    SubmitError::Full,
                                    state,
                                    inbox,
                                );
                            }
                            Err(QueueCommandError::IdExhausted) => {
                                state.fail(WorkerTerminationReason::RequestExhausted);
                                break;
                            }
                            Err(QueueCommandError::WriterClosed) => {
                                state.fail(WorkerTerminationReason::WriterClosed);
                                break;
                            }
                        }
                    }
                }
            }
        }
        ActorMessage::StdoutEof => {
            state.stdout_done = true;
            if !state.terminal_seen && !state.close_requested {
                state.fail(if state.phase == ActorPhase::Booting {
                    WorkerTerminationReason::EofBeforeHello
                } else {
                    WorkerTerminationReason::EofBeforeTerminal
                });
            }
        }
        ActorMessage::StdoutError(error) => {
            state.stdout_done = true;
            if state.terminal_seen {
                state.closure_health = ClosureHealth::ProtocolAfterTerminal;
                state.killed = true;
            } else {
                state.fail(WorkerTerminationReason::StdoutDecode(error));
            }
        }
        ActorMessage::Writer(notice) => match notice {
            WriterNotice::Closed => state.writer_done = true,
            WriterNotice::Failed(error) => {
                state.writer_done = true;
                if state.terminal_seen {
                    state.closure_health = ClosureHealth::IoFailure;
                    state.killed = true;
                } else {
                    state.fail(WorkerTerminationReason::CommandWrite(error));
                }
            }
        },
        ActorMessage::Stderr(summary) => state.stderr = Some(summary),
    }
}

fn reject_submission(
    submission_id: SupervisorSubmissionId,
    command: SupervisorCommandKind,
    error: SubmitError,
    state: &mut ActorState,
    inbox: &SupervisorInbox,
) {
    if inbox
        .push(SupervisorEvent::SubmissionRejected {
            submission_id,
            command,
            error,
        })
        .is_err()
    {
        state.fail(WorkerTerminationReason::EventInboxExceeded);
    }
}

fn acknowledge_submission(
    submission_id: SupervisorSubmissionId,
    admission: CommandAdmission,
    state: &mut ActorState,
    inbox: &SupervisorInbox,
) {
    let Some(next_event_sequence) = state.next_worker_event_sequence else {
        state.fail(WorkerTerminationReason::CausalityViolation);
        return;
    };
    let event = if let Some(mode) = admission.mode {
        SupervisorEvent::Started {
            submission_id,
            mode,
            run: admission.run,
            request_id: admission.request_id,
            next_event_sequence,
        }
    } else {
        SupervisorEvent::SubmissionAccepted {
            submission_id,
            command: admission.command,
            run: admission.run,
            request_id: admission.request_id,
            next_event_sequence,
        }
    };
    if inbox.push(event).is_err() {
        state.fail(WorkerTerminationReason::EventInboxExceeded);
    }
}

fn acknowledge_close(admission: CommandAdmission, state: &mut ActorState, inbox: &SupervisorInbox) {
    let Some(next_event_sequence) = state.next_worker_event_sequence else {
        state.fail(WorkerTerminationReason::CausalityViolation);
        return;
    };
    if admission.command != SupervisorCommandKind::Stop || admission.mode.is_some() {
        state.fail(WorkerTerminationReason::CausalityViolation);
        return;
    }
    if inbox
        .push(SupervisorEvent::CloseAccepted {
            run: admission.run,
            request_id: admission.request_id,
            next_event_sequence,
        })
        .is_err()
    {
        state.fail(WorkerTerminationReason::EventInboxExceeded);
    }
}

fn queue_supervisor_command(
    command: SupervisorCommand,
    session: WorkerSessionId,
    config: &SupervisorConfig,
    state: &mut ActorState,
    writer: &mpsc::SyncSender<WriteRequest>,
) -> Result<CommandAdmission, QueueCommandError> {
    let public_command = command.kind();
    let legal = match &command {
        SupervisorCommand::Start { .. } => state.phase == ActorPhase::AwaitLoad,
        SupervisorCommand::Pause => {
            state.phase == ActorPhase::Active
                && state.pending_pause.is_none()
                && state.pending_stop.is_none()
        }
        SupervisorCommand::Continue
        | SupervisorCommand::StepInto
        | SupervisorCommand::StepOver
        | SupervisorCommand::StepOut => {
            state.phase == ActorPhase::Paused && state.pending_stop.is_none()
        }
        SupervisorCommand::Stop => {
            matches!(state.phase, ActorPhase::Active | ActorPhase::Paused)
                && state.pending_stop.is_none()
        }
        SupervisorCommand::ProvideInput { .. } => {
            matches!(state.phase, ActorPhase::Active | ActorPhase::Paused)
        }
    };
    if !legal {
        return Err(QueueCommandError::InvalidState);
    }
    let (payload, kind, run_id, revision, source_id, mode) = match command {
        SupervisorCommand::Start { mode, source } => {
            let document = WireDocument {
                source_id: SourceId(session.0),
                revision: RevisionId(1),
                display_name: source.display_name,
                text: source.text,
            };
            let (payload, kind) = match mode {
                SupervisorRunMode::Run => (Command::LoadAndRun { document }, IssuedKind::LoadRun),
                SupervisorRunMode::Debug => {
                    (Command::LoadAndDebug { document }, IssuedKind::LoadDebug)
                }
            };
            (
                payload,
                kind,
                RunId(1),
                RevisionId(1),
                Some(SourceId(session.0)),
                Some(mode),
            )
        }
        SupervisorCommand::Pause => current_command(state, Command::Pause, IssuedKind::Pause)?,
        SupervisorCommand::Continue => {
            current_command(state, Command::Continue, IssuedKind::Continue)?
        }
        SupervisorCommand::StepInto => {
            current_command(state, Command::StepInto, IssuedKind::StepInto)?
        }
        SupervisorCommand::StepOver => {
            current_command(state, Command::StepOver, IssuedKind::StepOver)?
        }
        SupervisorCommand::StepOut => {
            current_command(state, Command::StepOut, IssuedKind::StepOut)?
        }
        SupervisorCommand::Stop => current_command(state, Command::Stop, IssuedKind::Stop)?,
        SupervisorCommand::ProvideInput { in_reply_to, text } => current_command(
            state,
            Command::ProvideInput { in_reply_to, text },
            IssuedKind::ProvideInput,
        )?,
    };
    let prepared = prepare_command(session, run_id, revision, payload, state)?;
    enqueue_write(
        writer,
        WriteRequest {
            envelope: prepared.envelope,
            close_after: false,
        },
    )?;
    state.next_request = prepared.next_request;
    state.next_sequence = prepared.next_sequence;
    state.issued.insert(
        prepared.request,
        IssuedCommand {
            kind,
            run_id,
            revision,
        },
    );
    match kind {
        IssuedKind::LoadRun | IssuedKind::LoadDebug => {
            state.active = Some(ActiveRun {
                run_id,
                revision,
                source_id: source_id.ok_or(QueueCommandError::InvalidState)?,
                driver: prepared.request,
            });
            state.phase = ActorPhase::Active;
        }
        IssuedKind::Pause => state.pending_pause = Some(prepared.request),
        IssuedKind::Continue
        | IssuedKind::StepInto
        | IssuedKind::StepOver
        | IssuedKind::StepOut => {
            state.phase = ActorPhase::Active;
            state.pending_pause = None;
            state
                .active
                .as_mut()
                .ok_or(QueueCommandError::InvalidState)?
                .driver = prepared.request;
        }
        IssuedKind::Stop => {
            state.pending_stop = Some(prepared.request);
            state.stop_deadline = Some(deadline_after(config.stop_timeout));
        }
        IssuedKind::ProvideInput | IssuedKind::Shutdown => {}
    }
    let run = state
        .active
        .ok_or(QueueCommandError::InvalidState)?
        .public(session);
    Ok(CommandAdmission {
        command: public_command,
        mode,
        run,
        request_id: prepared.request,
    })
}

type CurrentCommand = (
    Command,
    IssuedKind,
    RunId,
    RevisionId,
    Option<SourceId>,
    Option<SupervisorRunMode>,
);

fn current_command(
    state: &ActorState,
    payload: Command,
    kind: IssuedKind,
) -> Result<CurrentCommand, QueueCommandError> {
    let active = state.active.ok_or(QueueCommandError::InvalidState)?;
    Ok((payload, kind, active.run_id, active.revision, None, None))
}

fn prepare_command(
    session: WorkerSessionId,
    run_id: RunId,
    revision: RevisionId,
    payload: Command,
    state: &ActorState,
) -> Result<PreparedCommand, QueueCommandError> {
    let request = state.next_request.ok_or(QueueCommandError::IdExhausted)?;
    let sequence = state.next_sequence.ok_or(QueueCommandError::IdExhausted)?;
    if request == u64::MAX && !matches!(payload, Command::Shutdown) {
        return Err(QueueCommandError::IdExhausted);
    }
    if sequence == u64::MAX && !matches!(payload, Command::Shutdown) {
        return Err(QueueCommandError::IdExhausted);
    }
    let request_id = RequestId(request);
    let envelope = Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: session,
        run_id,
        source_revision: revision,
        request_id,
        sequence: EventSequence(sequence),
        payload,
    };
    Ok(PreparedCommand {
        envelope,
        request: request_id,
        next_request: request.checked_add(1),
        next_sequence: sequence.checked_add(1),
    })
}

fn queue_shutdown(
    session: WorkerSessionId,
    state: &mut ActorState,
    writer: &mpsc::SyncSender<WriteRequest>,
    config: &SupervisorConfig,
) -> Result<(), QueueCommandError> {
    let (run_id, revision) = state
        .active
        .map(|active| (active.run_id, active.revision))
        .unwrap_or((RunId(0), RevisionId(0)));
    let prepared = prepare_command(session, run_id, revision, Command::Shutdown, state)?;
    enqueue_write(
        writer,
        WriteRequest {
            envelope: prepared.envelope,
            close_after: true,
        },
    )?;
    state.next_request = prepared.next_request;
    state.next_sequence = prepared.next_sequence;
    state.issued.insert(
        prepared.request,
        IssuedCommand {
            kind: IssuedKind::Shutdown,
            run_id,
            revision,
        },
    );
    state.phase = ActorPhase::Closing;
    state.shutdown_deadline = Some(deadline_after(config.shutdown_timeout));
    Ok(())
}

fn enqueue_write(
    writer: &mpsc::SyncSender<WriteRequest>,
    request: WriteRequest,
) -> Result<(), QueueCommandError> {
    writer.try_send(request).map_err(|error| match error {
        mpsc::TrySendError::Full(_) => QueueCommandError::WriterFull,
        mpsc::TrySendError::Disconnected(_) => QueueCommandError::WriterClosed,
    })
}

#[allow(clippy::too_many_arguments)]
fn accept_worker_event(
    envelope: Envelope<WorkerEvent>,
    session: WorkerSessionId,
    config: &SupervisorConfig,
    state: &mut ActorState,
    writer: &mpsc::SyncSender<WriteRequest>,
    inbox: &SupervisorInbox,
    public_state: &AtomicU8,
) -> Result<(), WorkerTerminationReason> {
    state
        .event_validator
        .validate(&envelope)
        .map_err(WorkerTerminationReason::StdoutProtocol)?;
    if envelope.worker_session_id != session {
        return Err(WorkerTerminationReason::CausalityViolation);
    }
    if matches!(envelope.payload, WorkerEvent::Hello) {
        if state.phase != ActorPhase::Booting {
            return Err(WorkerTerminationReason::CausalityViolation);
        }
        let next_event_sequence = envelope
            .sequence
            .0
            .checked_add(1)
            .map(EventSequence)
            .ok_or(WorkerTerminationReason::CausalityViolation)?;
        inbox
            .push(SupervisorEvent::Worker(Box::new(envelope)))
            .map_err(|_| WorkerTerminationReason::EventInboxExceeded)?;
        state.next_worker_event_sequence = Some(next_event_sequence);
        state.phase = ActorPhase::AwaitLoad;
        public_state.store(STATE_AWAIT_LOAD, Ordering::Release);
        return Ok(());
    }

    let active = state
        .active
        .ok_or(WorkerTerminationReason::CausalityViolation)?;
    let issued = state
        .issued
        .get(&envelope.request_id)
        .copied()
        .ok_or(WorkerTerminationReason::CausalityViolation)?;
    if issued.run_id != envelope.run_id || issued.revision != envelope.source_revision {
        return Err(WorkerTerminationReason::CausalityViolation);
    }
    if !matches!(envelope.payload, WorkerEvent::CommandRejected { .. })
        && (envelope.run_id != active.run_id || envelope.source_revision != active.revision)
    {
        return Err(WorkerTerminationReason::CausalityViolation);
    }
    validate_event_causality(&envelope, issued, active, state)?;
    publish_phase(state.phase, public_state);

    let terminal = envelope.payload.is_terminal();
    let next_event_sequence = if terminal {
        None
    } else {
        Some(
            envelope
                .sequence
                .0
                .checked_add(1)
                .map(EventSequence)
                .ok_or(WorkerTerminationReason::CausalityViolation)?,
        )
    };
    inbox
        .push(SupervisorEvent::Worker(Box::new(envelope)))
        .map_err(|_| WorkerTerminationReason::EventInboxExceeded)?;
    state.next_worker_event_sequence = next_event_sequence;
    if terminal {
        state.terminal_seen = true;
        state.phase = ActorPhase::Terminal;
        state.stop_deadline = None;
        public_state.store(STATE_TERMINAL, Ordering::Release);
        queue_shutdown(session, state, writer, config)
            .map_err(|_| WorkerTerminationReason::WriterClosed)?;
    }
    Ok(())
}

fn publish_phase(phase: ActorPhase, public_state: &AtomicU8) {
    let value = match phase {
        ActorPhase::Booting => STATE_BOOTING,
        ActorPhase::AwaitLoad => STATE_AWAIT_LOAD,
        ActorPhase::Active => STATE_ACTIVE,
        ActorPhase::Paused => STATE_PAUSED,
        ActorPhase::Terminal => STATE_TERMINAL,
        ActorPhase::Closing => STATE_CLOSING,
    };
    public_state.store(value, Ordering::Release);
}

fn validate_event_causality(
    envelope: &Envelope<WorkerEvent>,
    issued: IssuedCommand,
    active: ActiveRun,
    state: &mut ActorState,
) -> Result<(), WorkerTerminationReason> {
    match &envelope.payload {
        WorkerEvent::Hello | WorkerEvent::InputRequested { .. } => {
            return Err(WorkerTerminationReason::CausalityViolation);
        }
        WorkerEvent::CommandRejected { .. } => {}
        WorkerEvent::Output { .. }
        | WorkerEvent::Diagnostic { .. }
        | WorkerEvent::OutputTruncated => {
            if envelope.request_id != active.driver {
                return Err(WorkerTerminationReason::CausalityViolation);
            }
            if let WorkerEvent::Output { .. } = envelope.payload {
                if state.output_truncated {
                    return Err(WorkerTerminationReason::CausalityViolation);
                }
                let framed = LineCodec::new()
                    .worker_event_payload_len(envelope)
                    .map_err(|_| WorkerTerminationReason::CausalityViolation)?
                    .checked_add(1)
                    .ok_or(WorkerTerminationReason::CausalityViolation)?;
                state.output_bytes = state
                    .output_bytes
                    .checked_add(framed)
                    .ok_or(WorkerTerminationReason::CausalityViolation)?;
                if state.output_bytes > MAX_RUN_OUTPUT_FRAME_BYTES {
                    return Err(WorkerTerminationReason::CausalityViolation);
                }
            } else if matches!(envelope.payload, WorkerEvent::OutputTruncated) {
                if state.output_truncated {
                    return Err(WorkerTerminationReason::CausalityViolation);
                }
                state.output_truncated = true;
            }
            validate_event_source(&envelope.payload, active.source_id)?;
        }
        WorkerEvent::Paused { snapshot, .. } => {
            let correct = match snapshot.reason {
                SnapshotReason::Paused(PauseReason::DebugPoint) => {
                    issued.kind == IssuedKind::LoadDebug
                }
                SnapshotReason::Paused(PauseReason::Explicit) => issued.kind == IssuedKind::Pause,
                SnapshotReason::Paused(PauseReason::Step) => matches!(
                    issued.kind,
                    IssuedKind::StepInto | IssuedKind::StepOver | IssuedKind::StepOut
                ),
                _ => false,
            };
            if !correct {
                return Err(WorkerTerminationReason::CausalityViolation);
            }
            validate_event_source(&envelope.payload, active.source_id)?;
            state.phase = ActorPhase::Paused;
            state.pending_pause = None;
        }
        WorkerEvent::Completed | WorkerEvent::Faulted { .. } => {
            if envelope.request_id != active.driver {
                return Err(WorkerTerminationReason::CausalityViolation);
            }
            validate_event_source(&envelope.payload, active.source_id)?;
        }
        WorkerEvent::Cancelled { .. } => {
            if issued.kind != IssuedKind::Stop || state.pending_stop != Some(envelope.request_id) {
                return Err(WorkerTerminationReason::CausalityViolation);
            }
            validate_event_source(&envelope.payload, active.source_id)?;
        }
    }
    Ok(())
}

fn validate_event_source(
    event: &WorkerEvent,
    source_id: SourceId,
) -> Result<(), WorkerTerminationReason> {
    let matches = match event {
        WorkerEvent::Diagnostic { diagnostic } => diagnostic.span.source_id == source_id,
        WorkerEvent::Paused { location, snapshot } => {
            location.source_id == source_id && snapshot.current_span.source_id == source_id
        }
        WorkerEvent::Cancelled { snapshot } => snapshot.current_span.source_id == source_id,
        WorkerEvent::Faulted {
            diagnostic,
            snapshot,
        } => diagnostic.span.source_id == source_id && snapshot.current_span.source_id == source_id,
        _ => true,
    };
    if matches {
        Ok(())
    } else {
        Err(WorkerTerminationReason::CausalityViolation)
    }
}

fn check_deadlines(config: &SupervisorConfig, state: &mut ActorState) {
    let now = Instant::now();
    if state.phase == ActorPhase::Booting && now >= state.handshake_deadline {
        state.fail(WorkerTerminationReason::HandshakeTimeout);
    }
    if state.stop_deadline.is_some_and(|deadline| now >= deadline) && !state.terminal_seen {
        state.fail(WorkerTerminationReason::StopTimeout);
    }
    if state
        .shutdown_deadline
        .is_some_and(|deadline| now >= deadline)
        && state.child_status.is_none()
    {
        if state.terminal_seen {
            state.closure_health = ClosureHealth::ShutdownTimeout;
            state.killed = true;
        } else {
            state.fail(WorkerTerminationReason::SupervisorClosed);
        }
    }
    let _ = config;
}

fn deadline_after(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration).unwrap_or(now)
}

fn ensure_killed_and_reaped(child: &mut Child, state: &mut ActorState) {
    if state.child_status.is_some() {
        return;
    }
    if let Err(error) = child.kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        if state.terminal_seen {
            state.closure_health = ClosureHealth::IoFailure;
        } else {
            state.fail(WorkerTerminationReason::Kill(error.kind()));
        }
    }
    match child.wait() {
        Ok(status) => state.child_status = Some(status.code()),
        Err(error) => {
            state.child_status = Some(None);
            if state.terminal_seen {
                state.closure_health = ClosureHealth::IoFailure;
            } else {
                state.fail(WorkerTerminationReason::Wait(error.kind()));
            }
        }
    }
}

fn finalize_supervisor(session: WorkerSessionId, state: &ActorState, inbox: &SupervisorInbox) {
    let stderr = state.stderr.clone().unwrap_or_default();
    let run = state.active.map(|active| active.public(session));
    let event = if state.terminal_seen || state.close_requested && state.active.is_none() {
        let status = state.child_status.flatten();
        let health = if state.closure_health != ClosureHealth::Clean {
            state.closure_health
        } else if status == Some(0) {
            ClosureHealth::Clean
        } else {
            ClosureHealth::NonzeroExit
        };
        SupervisorEvent::Closed {
            run,
            status,
            stderr,
            health,
        }
    } else {
        SupervisorEvent::WorkerTerminated {
            run,
            reason: state
                .failure
                .unwrap_or(WorkerTerminationReason::UnexpectedExit(
                    state.child_status.flatten(),
                )),
            stderr,
        }
    };
    let _ = inbox.push(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SupervisorConfig {
        SupervisorConfig {
            executable: PathBuf::from("worker"),
            handshake_timeout: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
        }
    }

    fn active_state() -> ActorState {
        let mut state = ActorState::new(WorkerSessionId(1), &config()).unwrap();
        state.phase = ActorPhase::Active;
        state.active = Some(ActiveRun {
            run_id: RunId(1),
            revision: RevisionId(1),
            source_id: SourceId(1),
            driver: RequestId(1),
        });
        state.issued.insert(
            RequestId(1),
            IssuedCommand {
                kind: IssuedKind::LoadRun,
                run_id: RunId(1),
                revision: RevisionId(1),
            },
        );
        state
    }

    fn event(request: u64, payload: WorkerEvent) -> Envelope<WorkerEvent> {
        Envelope {
            version: PROTOCOL_VERSION,
            worker_session_id: WorkerSessionId(1),
            run_id: RunId(1),
            source_revision: RevisionId(1),
            request_id: RequestId(request),
            sequence: EventSequence(2),
            payload,
        }
    }

    #[test]
    fn stop_request_cannot_claim_natural_completion() {
        let mut state = active_state();
        state.pending_stop = Some(RequestId(2));
        let issued = IssuedCommand {
            kind: IssuedKind::Stop,
            run_id: RunId(1),
            revision: RevisionId(1),
        };
        assert_eq!(
            validate_event_causality(
                &event(2, WorkerEvent::Completed),
                issued,
                state.active.unwrap(),
                &mut state,
            ),
            Err(WorkerTerminationReason::CausalityViolation)
        );
    }

    #[test]
    fn output_after_truncation_is_a_causality_failure() {
        let mut state = active_state();
        state.output_truncated = true;
        let issued = state.issued[&RequestId(1)];
        assert_eq!(
            validate_event_causality(
                &event(
                    1,
                    WorkerEvent::Output {
                        text: "late".to_string(),
                    },
                ),
                issued,
                state.active.unwrap(),
                &mut state,
            ),
            Err(WorkerTerminationReason::CausalityViolation)
        );
    }

    #[test]
    fn accepted_terminal_is_never_replaced_by_local_termination() {
        let mut state = active_state();
        state.terminal_seen = true;
        state.child_status = Some(Some(9));
        state.stdout_done = true;
        state.stderr = Some(StderrSummary::default());
        state.failure = Some(WorkerTerminationReason::UnexpectedExit(Some(9)));
        let inbox = SupervisorInbox::default();
        finalize_supervisor(WorkerSessionId(1), &state, &inbox);
        assert!(matches!(
            inbox.pop().unwrap(),
            Some(SupervisorEvent::Closed {
                run: Some(SupervisorRun {
                    worker_session_id: WorkerSessionId(1),
                    run_id: RunId(1),
                    source_id: SourceId(1),
                    source_revision: RevisionId(1),
                }),
                health: ClosureHealth::NonzeroExit,
                ..
            })
        ));
    }

    #[test]
    fn preterminal_worker_termination_retains_the_active_run() {
        let mut state = active_state();
        state.child_status = Some(Some(9));
        state.stdout_done = true;
        state.stderr = Some(StderrSummary::default());
        state.failure = Some(WorkerTerminationReason::UnexpectedExit(Some(9)));
        let inbox = SupervisorInbox::default();
        finalize_supervisor(WorkerSessionId(1), &state, &inbox);
        assert!(matches!(
            inbox.pop().unwrap(),
            Some(SupervisorEvent::WorkerTerminated {
                run: Some(SupervisorRun {
                    worker_session_id: WorkerSessionId(1),
                    run_id: RunId(1),
                    source_id: SourceId(1),
                    source_revision: RevisionId(1),
                }),
                reason: WorkerTerminationReason::UnexpectedExit(Some(9)),
                ..
            })
        ));
    }

    #[test]
    fn submission_queue_is_bounded_by_retained_bytes() {
        let (actor, receiver) = mpsc::sync_channel(ACTOR_QUEUE_ITEMS);
        let state = Arc::new(AtomicU8::new(STATE_BOOTING));
        let close_requested = Arc::new(AtomicBool::new(false));
        let submission_budget = Arc::new(SubmissionBudget::default());
        let sender = WorkerCommandSender {
            actor,
            state,
            close_requested,
            submission_budget: submission_budget.clone(),
            last_submission_id: Arc::new(AtomicU64::new(0)),
        };
        let text = "a".repeat(MAX_PAYLOAD_BYTES - 128 * 1024);
        let command = || SupervisorCommand::Start {
            mode: SupervisorRunMode::Run,
            source: SupervisorSource {
                display_name: "bounded.lox".to_string(),
                text: text.clone(),
            },
        };

        sender
            .try_send(SupervisorSubmissionId(1), command())
            .expect("first large submission");
        sender
            .try_send(SupervisorSubmissionId(2), command())
            .expect("second large submission");
        assert_eq!(
            sender.try_send(SupervisorSubmissionId(3), command()),
            Err(SubmitError::Full)
        );

        drop(receiver);
        assert_eq!(*submission_budget.bytes.lock().unwrap(), 0);
    }

    #[test]
    fn invalid_submission_is_rejected_before_actor_admission() {
        let (actor, receiver) = mpsc::sync_channel(1);
        let submission_budget = Arc::new(SubmissionBudget::default());
        let sender = WorkerCommandSender {
            actor,
            state: Arc::new(AtomicU8::new(STATE_BOOTING)),
            close_requested: Arc::new(AtomicBool::new(false)),
            submission_budget: submission_budget.clone(),
            last_submission_id: Arc::new(AtomicU64::new(0)),
        };

        assert_eq!(
            sender.try_send(
                SupervisorSubmissionId(1),
                SupervisorCommand::Start {
                    mode: SupervisorRunMode::Run,
                    source: SupervisorSource {
                        display_name: "folder/invalid.lox".to_string(),
                        text: "print 1;\n".to_string(),
                    },
                },
            ),
            Err(SubmitError::InvalidCommand)
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(*submission_budget.bytes.lock().unwrap(), 0);
    }

    #[test]
    fn drop_sets_durable_close_signal_when_actor_queue_is_full() {
        let (actor, receiver) = mpsc::sync_channel(1);
        actor.try_send(ActorMessage::Close).unwrap();
        let close_requested = Arc::new(AtomicBool::new(false));
        let supervisor = WorkerSupervisor {
            sender: WorkerCommandSender {
                actor,
                state: Arc::new(AtomicU8::new(STATE_BOOTING)),
                close_requested: close_requested.clone(),
                submission_budget: Arc::new(SubmissionBudget::default()),
                last_submission_id: Arc::new(AtomicU64::new(0)),
            },
            inbox: Arc::new(SupervisorInbox::default()),
            session: WorkerSessionId(1),
        };

        drop(supervisor);

        assert!(close_requested.load(Ordering::Acquire));
        drop(receiver);
    }

    #[test]
    fn boot_close_discards_pending_load_and_queues_shutdown_after_hello() {
        let session = WorkerSessionId(1);
        let config = config();
        let mut state = ActorState::new(session, &config).unwrap();
        let (writer, written) = mpsc::sync_channel(4);
        let inbox = SupervisorInbox::default();
        let public_state = AtomicU8::new(STATE_BOOTING);
        let budget = Arc::new(SubmissionBudget::default());
        let mut pending = VecDeque::from([QueuedSupervisorCommand {
            submission_id: SupervisorSubmissionId(1),
            command: SupervisorCommand::Start {
                mode: SupervisorRunMode::Run,
                source: SupervisorSource {
                    display_name: "discarded.lox".to_string(),
                    text: "print 1;\n".to_string(),
                },
            },
            _permit: budget.try_reserve(64).unwrap(),
        }]);

        handle_actor_message(
            ActorMessage::Close,
            session,
            &config,
            &mut state,
            &writer,
            &inbox,
            &public_state,
            &mut pending,
        );
        assert_eq!(state.phase, ActorPhase::Booting);
        handle_actor_message(
            ActorMessage::StdoutEvent(Box::new(Envelope {
                version: PROTOCOL_VERSION,
                worker_session_id: session,
                run_id: RunId(0),
                source_revision: RevisionId(0),
                request_id: RequestId(0),
                sequence: EventSequence(1),
                payload: WorkerEvent::Hello,
            })),
            session,
            &config,
            &mut state,
            &writer,
            &inbox,
            &public_state,
            &mut pending,
        );

        assert!(pending.is_empty());
        assert!(state.active.is_none());
        assert_eq!(state.phase, ActorPhase::Closing);
        assert_eq!(public_state.load(Ordering::Acquire), STATE_CLOSING);
        assert!(matches!(
            written.try_recv().unwrap().envelope.payload,
            Command::Shutdown
        ));
        assert!(matches!(
            inbox.pop().unwrap(),
            Some(SupervisorEvent::Worker(envelope)) if envelope.payload == WorkerEvent::Hello
        ));
        assert_eq!(
            inbox.pop().unwrap(),
            Some(SupervisorEvent::SubmissionRejected {
                submission_id: SupervisorSubmissionId(1),
                command: SupervisorCommandKind::Start,
                error: SubmitError::Closed,
            })
        );
    }
}
