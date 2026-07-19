use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::contained_child::{
    CachedIoError, ContainedChild, ContainedPipedChild, ContainedSpawnAttempt,
    PartialCleanupFailure, PartialContainedChild,
};

use super::actor::{
    AccountedJsonRpcMessage, ActorEffect, ActorEvent, BoundedChildExit, BoundedStderrTail,
    CleanupFailure, LaunchFailure, LaunchOutcome, MAX_STDERR_BYTES, MAX_STDERR_LINES,
    READER_INBOX_BODY_BYTES, READER_INBOX_ITEMS, READER_READ_CHUNK_BYTES, ReaderFatalCause,
    WRITER_INPUT_ITEMS, WriterFatalCause, WriterOutcome,
};
use super::framing::{FrameDecoder, JsonRpcMessage};
use super::snapshot::{ProcessGeneration, WriteSequence};

#[derive(Clone, Default)]
struct AdvisoryWake {
    sender: Option<mpsc::SyncSender<()>>,
}

impl AdvisoryWake {
    fn from_sender(sender: mpsc::SyncSender<()>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    fn notify(&self) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(());
        }
    }
}

pub(crate) struct CompletionCell<T> {
    value: Mutex<Option<T>>,
    ready: Condvar,
}

impl<T> CompletionCell<T> {
    pub(crate) fn new() -> Self {
        Self {
            value: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    pub(crate) fn publish(&self, value: T) {
        let mut slot = self
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(slot.is_none(), "completion cell is single-assignment");
        *slot = Some(value);
        self.ready.notify_all();
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn cloned(&self) -> Option<T>
    where
        T: Clone,
    {
        self.value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn wait_until(&self, deadline: Instant) -> bool {
        let mut slot = self
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while slot.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, timeout) = self
                .ready
                .wait_timeout(slot, deadline.saturating_duration_since(now))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot = next;
            if timeout.timed_out() && slot.is_none() {
                return false;
            }
        }
        true
    }
}

impl<T> Default for CompletionCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadJoinError {
    Panicked,
}

pub(crate) fn join_if_completed<T>(
    completion: &CompletionCell<T>,
    handle: &mut Option<JoinHandle<T>>,
) -> Result<Option<T>, ThreadJoinError> {
    let Some(join) = handle.as_ref() else {
        return Ok(None);
    };
    if !completion.is_ready() || !join.is_finished() {
        return Ok(None);
    }
    handle
        .take()
        .expect("checked join owner")
        .join()
        .map(Some)
        .map_err(|_| ThreadJoinError::Panicked)
}

#[derive(Clone)]
pub(crate) struct InboxBudget {
    retained: Arc<AtomicUsize>,
    limit: usize,
}

impl InboxBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            retained: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }

    pub(crate) fn try_reserve(&self, bytes: usize) -> Option<InboxPermit> {
        let mut retained = self.retained.load(Ordering::Acquire);
        loop {
            let next = retained
                .checked_add(bytes)
                .filter(|next| *next <= self.limit)?;
            match self.retained.compare_exchange_weak(
                retained,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(InboxPermit {
                        retained: Arc::clone(&self.retained),
                        bytes,
                    });
                }
                Err(actual) => retained = actual,
            }
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained.load(Ordering::Acquire)
    }
}

pub(crate) struct InboxPermit {
    retained: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for InboxPermit {
    fn drop(&mut self) {
        let previous = self.retained.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

pub(crate) struct WriteFrame {
    pub(crate) generation: ProcessGeneration,
    pub(crate) sequence: WriteSequence,
    pub(crate) bytes: Arc<[u8]>,
}

pub(crate) struct WriterPort {
    sender: mpsc::SyncSender<WriteFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriterHandoffError {
    Full,
    Disconnected,
}

impl WriterPort {
    pub(crate) fn try_send(
        &self,
        generation: ProcessGeneration,
        sequence: WriteSequence,
        bytes: Arc<[u8]>,
    ) -> Result<(), WriterHandoffError> {
        match self.sender.try_send(WriteFrame {
            generation,
            sequence,
            bytes,
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(WriterHandoffError::Full),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(WriterHandoffError::Disconnected),
        }
    }
}

pub(crate) fn writer_input() -> (WriterPort, mpsc::Receiver<WriteFrame>) {
    let (sender, receiver) = mpsc::sync_channel(WRITER_INPUT_ITEMS);
    (WriterPort { sender }, receiver)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdapterFatal {
    Reader {
        generation: ProcessGeneration,
        cause: ReaderFatalCause,
    },
    Writer {
        generation: ProcessGeneration,
        cause: WriterFatalCause,
    },
}

#[derive(Clone)]
pub(crate) struct FatalSideband {
    value: Arc<Mutex<Option<AdapterFatal>>>,
    wake: AdvisoryWake,
}

impl FatalSideband {
    pub(crate) fn new() -> Self {
        Self {
            value: Arc::new(Mutex::new(None)),
            wake: AdvisoryWake::default(),
        }
    }

    pub(crate) fn with_wake(wake: mpsc::SyncSender<()>) -> Self {
        Self {
            value: Arc::new(Mutex::new(None)),
            wake: AdvisoryWake::from_sender(wake),
        }
    }

    pub(crate) fn store_first(&self, value: AdapterFatal) -> bool {
        let mut slot = self
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return false;
        }
        *slot = Some(value);
        drop(slot);
        self.wake.notify();
        true
    }

    pub(crate) fn take(&self) -> Option<AdapterFatal> {
        self.value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl Default for FatalSideband {
    fn default() -> Self {
        Self::new()
    }
}

struct ReaderItem {
    message: AccountedJsonRpcMessage,
    _permit: InboxPermit,
}

pub(crate) struct ReaderSender {
    sender: mpsc::SyncSender<ReaderItem>,
    budget: InboxBudget,
    wake: AdvisoryWake,
}

pub(crate) struct ReaderInbox {
    receiver: mpsc::Receiver<ReaderItem>,
}

impl ReaderInbox {
    pub(crate) fn try_recv(&self) -> Option<AccountedJsonRpcMessage> {
        match self.receiver.try_recv() {
            Ok(item) => Some(item.message),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderSendError {
    Budget,
    Full,
    Disconnected,
}

impl ReaderSender {
    fn try_send(&self, message: JsonRpcMessage) -> Result<(), ReaderSendError> {
        let Some(permit) = self.budget.try_reserve(message.body_bytes()) else {
            return Err(ReaderSendError::Budget);
        };
        let item = ReaderItem {
            message: AccountedJsonRpcMessage::from_message(message),
            _permit: permit,
        };
        match self.sender.try_send(item) {
            Ok(()) => {
                self.wake.notify();
                Ok(())
            }
            Err(mpsc::TrySendError::Full(_)) => Err(ReaderSendError::Full),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(ReaderSendError::Disconnected),
        }
    }
}

pub(crate) fn reader_lane(budget: InboxBudget) -> (ReaderSender, ReaderInbox) {
    reader_lane_with_optional_wake(budget, AdvisoryWake::default())
}

pub(crate) fn reader_lane_with_wake(
    budget: InboxBudget,
    wake: mpsc::SyncSender<()>,
) -> (ReaderSender, ReaderInbox) {
    reader_lane_with_optional_wake(budget, AdvisoryWake::from_sender(wake))
}

fn reader_lane_with_optional_wake(
    budget: InboxBudget,
    wake: AdvisoryWake,
) -> (ReaderSender, ReaderInbox) {
    let (sender, receiver) = mpsc::sync_channel(READER_INBOX_ITEMS);
    (
        ReaderSender {
            sender,
            budget,
            wake,
        },
        ReaderInbox { receiver },
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReaderThreadOutcome {
    CleanEof,
    FramingFailed,
    IoFailed,
    InboxRejected,
}

pub(crate) fn run_reader(
    mut reader: impl Read,
    generation: ProcessGeneration,
    sender: ReaderSender,
    fatal: &FatalSideband,
) -> ReaderThreadOutcome {
    let mut decoder = FrameDecoder::new();
    let mut chunk = [0_u8; READER_READ_CHUNK_BYTES];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(_) => {
                fatal.store_first(AdapterFatal::Reader {
                    generation,
                    cause: ReaderFatalCause::Io,
                });
                return ReaderThreadOutcome::IoFailed;
            }
        };
        if read == 0 {
            return match decoder.finish() {
                Ok(()) => ReaderThreadOutcome::CleanEof,
                Err(_) => {
                    fatal.store_first(AdapterFatal::Reader {
                        generation,
                        cause: ReaderFatalCause::Framing,
                    });
                    ReaderThreadOutcome::FramingFailed
                }
            };
        }
        if decoder.feed(&chunk[..read]).is_err() {
            fatal.store_first(AdapterFatal::Reader {
                generation,
                cause: ReaderFatalCause::Framing,
            });
            return ReaderThreadOutcome::FramingFailed;
        }
        loop {
            match decoder.next_message() {
                Ok(Some(message)) => {
                    if sender.try_send(message).is_err() {
                        fatal.store_first(AdapterFatal::Reader {
                            generation,
                            cause: ReaderFatalCause::InboxOverflow,
                        });
                        return ReaderThreadOutcome::InboxRejected;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    fatal.store_first(AdapterFatal::Reader {
                        generation,
                        cause: ReaderFatalCause::Framing,
                    });
                    return ReaderThreadOutcome::FramingFailed;
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct IoStop {
    requested: Arc<AtomicBool>,
}

impl IoStop {
    pub(crate) fn new() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

impl Default for IoStop {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriterCompletion {
    pub(crate) generation: ProcessGeneration,
    pub(crate) sequence: WriteSequence,
    pub(crate) outcome: WriterOutcome,
}

#[derive(Clone)]
pub(crate) struct WriterOutcomeSlot {
    value: Arc<Mutex<Option<WriterCompletion>>>,
    wake: AdvisoryWake,
}

impl WriterOutcomeSlot {
    pub(crate) fn new() -> Self {
        Self {
            value: Arc::new(Mutex::new(None)),
            wake: AdvisoryWake::default(),
        }
    }

    pub(crate) fn with_wake(wake: mpsc::SyncSender<()>) -> Self {
        Self {
            value: Arc::new(Mutex::new(None)),
            wake: AdvisoryWake::from_sender(wake),
        }
    }

    pub(crate) fn publish(&self, outcome: WriterCompletion) -> bool {
        let mut slot = self
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return false;
        }
        *slot = Some(outcome);
        drop(slot);
        self.wake.notify();
        true
    }

    pub(crate) fn take(&self) -> Option<WriterCompletion> {
        self.value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl Default for WriterOutcomeSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriterThreadOutcome {
    InputClosed,
    Stopped,
    WriteFailed,
    ResultOverflow,
    GenerationMismatch,
}

pub(crate) fn run_writer(
    mut writer: impl Write,
    generation: ProcessGeneration,
    receiver: mpsc::Receiver<WriteFrame>,
    stop: IoStop,
    outcomes: &WriterOutcomeSlot,
    fatal: &FatalSideband,
) -> WriterThreadOutcome {
    loop {
        if stop.is_requested() {
            return WriterThreadOutcome::Stopped;
        }
        let frame = match receiver.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(frame) => frame,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return WriterThreadOutcome::InputClosed;
            }
        };
        if frame.generation != generation {
            fatal.store_first(AdapterFatal::Writer {
                generation,
                cause: WriterFatalCause::AdapterInvariant,
            });
            return WriterThreadOutcome::GenerationMismatch;
        }
        let outcome = if writer
            .write_all(&frame.bytes)
            .and_then(|()| writer.flush())
            .is_ok()
        {
            WriterOutcome::Flushed
        } else {
            WriterOutcome::WriteFailed
        };
        if !outcomes.publish(WriterCompletion {
            generation,
            sequence: frame.sequence,
            outcome,
        }) {
            fatal.store_first(AdapterFatal::Writer {
                generation,
                cause: WriterFatalCause::ResultOverflow,
            });
            return WriterThreadOutcome::ResultOverflow;
        }
        if outcome == WriterOutcome::WriteFailed {
            return WriterThreadOutcome::WriteFailed;
        }
    }
}

const PROCESS_TICK: Duration = Duration::from_millis(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestIoLanePanic {
    Writer,
    Reader,
}

#[derive(Clone, Debug)]
pub(crate) struct LanguageProcessConfig {
    executable: PathBuf,
    arguments: Vec<OsString>,
    process_tick: Duration,
    cleanup_timeout: Duration,
    #[cfg(test)]
    io_thread_fail_after: Option<usize>,
    #[cfg(test)]
    launch_gate: Option<Arc<TestLaunchGate>>,
    #[cfg(test)]
    ready_gate: Option<Arc<TestLaunchGate>>,
    #[cfg(test)]
    finalization_gate: Option<Arc<TestLaunchGate>>,
    #[cfg(test)]
    io_lane_panic: Option<TestIoLanePanic>,
}

impl LanguageProcessConfig {
    pub(crate) fn sibling() -> Result<Self, SiblingResolutionError> {
        let current = std::env::current_exe()
            .map_err(|error| SiblingResolutionError::Metadata(error.kind()))?;
        let command = resolve_language_command_from(&current, |candidate| {
            std::fs::metadata(candidate).map(|metadata| metadata.is_file())
        })?;
        Ok(Self {
            executable: command.executable,
            arguments: command.arguments,
            process_tick: PROCESS_TICK,
            cleanup_timeout: CLEANUP_TIMEOUT,
            #[cfg(test)]
            io_thread_fail_after: None,
            #[cfg(test)]
            launch_gate: None,
            #[cfg(test)]
            ready_gate: None,
            #[cfg(test)]
            finalization_gate: None,
            #[cfg(test)]
            io_lane_panic: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_command(executable: PathBuf, arguments: Vec<OsString>) -> Self {
        Self {
            executable,
            arguments,
            process_tick: PROCESS_TICK,
            cleanup_timeout: CLEANUP_TIMEOUT,
            io_thread_fail_after: None,
            launch_gate: None,
            ready_gate: None,
            finalization_gate: None,
            io_lane_panic: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_io_thread_fail_after(mut self, started: usize) -> Self {
        self.io_thread_fail_after = Some(started);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_launch_gate(mut self, gate: Arc<TestLaunchGate>) -> Self {
        self.launch_gate = Some(gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_ready_gate(mut self, gate: Arc<TestLaunchGate>) -> Self {
        self.ready_gate = Some(gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_finalization_gate(mut self, gate: Arc<TestLaunchGate>) -> Self {
        self.finalization_gate = Some(gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_io_lane_panic(mut self, lane: TestIoLanePanic) -> Self {
        self.io_lane_panic = Some(lane);
        self
    }

    fn wait_at_launch_gate(&self, generation: ProcessGeneration) {
        #[cfg(test)]
        if let Some(gate) = &self.launch_gate {
            gate.block_if(generation);
        }
        #[cfg(not(test))]
        let _ = generation;
    }

    fn wait_at_ready_gate(&self, generation: ProcessGeneration) {
        #[cfg(test)]
        if let Some(gate) = &self.ready_gate {
            gate.block_if(generation);
        }
        #[cfg(not(test))]
        let _ = generation;
    }

    fn command(&self) -> ProcessCommand {
        let mut command = ProcessCommand::new(&self.executable);
        command.args(&self.arguments);
        command
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestLaunchGate {
    generation: ProcessGeneration,
    state: Mutex<TestLaunchGateState>,
    changed: Condvar,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct TestLaunchGateState {
    entered: bool,
    released: bool,
}

#[cfg(test)]
impl TestLaunchGate {
    pub(crate) fn new(generation: ProcessGeneration) -> Self {
        Self {
            generation,
            state: Mutex::new(TestLaunchGateState::default()),
            changed: Condvar::new(),
        }
    }

    fn block_if(&self, generation: ProcessGeneration) {
        if generation != self.generation {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.entered = true;
        self.changed.notify_all();
        while !state.released {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(crate) fn wait_until_entered(&self, deadline: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.entered {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, timeout) = self
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timeout.timed_out() && !state.entered {
                return false;
            }
        }
        true
    }

    pub(crate) fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.released = true;
        self.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdapterStartError {
    WorkerThread(io::ErrorKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdapterControlError {
    GenerationBusy,
    GenerationMismatch,
    NoActiveLanes,
    UnsupportedEffect,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerPhase {
    Idle,
    Launching(ProcessGeneration),
    Active(ProcessGeneration),
    Cleaning(ProcessGeneration),
    Publishing(ProcessGeneration),
    Quarantined(ProcessGeneration),
    Closed,
}

struct ControlState {
    phase: WorkerPhase,
    launch: Option<ProcessGeneration>,
    cancel: Option<ProcessGeneration>,
    cleanup: Option<ProcessGeneration>,
    acknowledgement: Option<ProcessGeneration>,
    last_retired: Option<ProcessGeneration>,
    shutdown: bool,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            phase: WorkerPhase::Idle,
            launch: None,
            cancel: None,
            cleanup: None,
            acknowledgement: None,
            last_retired: None,
            shutdown: false,
        }
    }
}

struct LaunchReport {
    generation: ProcessGeneration,
    outcome: LaunchOutcome,
    lanes: Option<ActiveClientLanes>,
    release_phase_on_delivery: bool,
}

#[derive(Clone, Copy)]
struct ChildExitReport {
    generation: ProcessGeneration,
    status: BoundedChildExit,
}

#[derive(Clone, Copy)]
enum FinalizationOutcome {
    Finalized,
    Quarantined(CleanupFailure),
}

struct FinalizationReport {
    generation: ProcessGeneration,
    outcome: FinalizationOutcome,
    stderr_tail: Option<BoundedStderrTail>,
    delivered: bool,
}

struct SharedProcessState {
    control: Mutex<ControlState>,
    control_ready: Condvar,
    launch: Mutex<Option<LaunchReport>>,
    child_exit: Mutex<Option<ChildExitReport>>,
    finalization: Mutex<Option<FinalizationReport>>,
    fatal: FatalSideband,
    wake: mpsc::SyncSender<()>,
    coordinator_gone: AtomicBool,
    #[cfg(test)]
    finalization_gate: Option<Arc<TestLaunchGate>>,
}

impl SharedProcessState {
    fn wake(&self) {
        let _ = self.wake.try_send(());
    }

    fn publish_launch(&self, report: LaunchReport) {
        let mut slot = self
            .launch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(slot.is_none(), "launch outcome slot is single-value");
        *slot = Some(report);
        drop(slot);
        self.wake();
    }

    fn publish_child_exit(&self, report: ChildExitReport) {
        let mut slot = self
            .child_exit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(report);
            drop(slot);
            self.wake();
        }
    }

    fn publish_finalization(&self, report: FinalizationReport) {
        #[cfg(test)]
        let generation = report.generation;
        let mut slot = self
            .finalization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(slot.is_none(), "finalization slot is single-value");
        *slot = Some(report);
        drop(slot);
        #[cfg(test)]
        if let Some(gate) = &self.finalization_gate {
            gate.block_if(generation);
        }
        self.wake();
    }
}

struct ActiveClientLanes {
    generation: ProcessGeneration,
    writer: WriterPort,
    reader: ReaderInbox,
    writer_outcomes: WriterOutcomeSlot,
    stop: IoStop,
}

pub(crate) struct LanguageProcessAdapter {
    shared: Arc<SharedProcessState>,
    wake: Option<mpsc::Receiver<()>>,
    lanes: Option<ActiveClientLanes>,
    _worker: JoinHandle<()>,
}

impl LanguageProcessAdapter {
    pub(crate) fn start(config: LanguageProcessConfig) -> Result<Self, AdapterStartError> {
        let (wake_sender, wake) = mpsc::sync_channel(1);
        Self::start_inner(config, wake_sender, Some(wake))
    }

    pub(crate) fn start_with_wake(
        config: LanguageProcessConfig,
        wake_sender: mpsc::SyncSender<()>,
    ) -> Result<Self, AdapterStartError> {
        Self::start_inner(config, wake_sender, None)
    }

    fn start_inner(
        config: LanguageProcessConfig,
        wake_sender: mpsc::SyncSender<()>,
        wake: Option<mpsc::Receiver<()>>,
    ) -> Result<Self, AdapterStartError> {
        #[cfg(test)]
        let finalization_gate = config.finalization_gate.clone();
        let shared = Arc::new(SharedProcessState {
            control: Mutex::new(ControlState::default()),
            control_ready: Condvar::new(),
            launch: Mutex::new(None),
            child_exit: Mutex::new(None),
            finalization: Mutex::new(None),
            fatal: FatalSideband::with_wake(wake_sender.clone()),
            wake: wake_sender,
            coordinator_gone: AtomicBool::new(false),
            #[cfg(test)]
            finalization_gate,
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("oxide-language-process".to_string())
            .spawn(move || process_worker(worker_shared, config))
            .map_err(|error| AdapterStartError::WorkerThread(error.kind()))?;
        Ok(Self {
            shared,
            wake,
            lanes: None,
            _worker: worker,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_all_pending_events_for_test(generation: ProcessGeneration) -> Self {
        let (wake_sender, wake) = mpsc::sync_channel(1);
        let shared = Arc::new(SharedProcessState {
            control: Mutex::new(ControlState {
                phase: WorkerPhase::Publishing(generation),
                ..ControlState::default()
            }),
            control_ready: Condvar::new(),
            launch: Mutex::new(None),
            child_exit: Mutex::new(None),
            finalization: Mutex::new(None),
            fatal: FatalSideband::with_wake(wake_sender.clone()),
            wake: wake_sender,
            coordinator_gone: AtomicBool::new(false),
            finalization_gate: None,
        });
        let outcomes = WriterOutcomeSlot::with_wake(shared.wake.clone());
        let (writer, _writer_receiver) = writer_input();
        let (reader_sender, reader) = reader_lane_with_wake(
            InboxBudget::new(READER_INBOX_BODY_BYTES),
            shared.wake.clone(),
        );
        let body = br#"{"jsonrpc":"2.0","method":"queued"}"#;
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body);
        let mut decoder = FrameDecoder::new();
        decoder.feed(&frame).expect("valid queued test frame");
        reader_sender
            .try_send(
                decoder
                    .next_message()
                    .expect("decode queued test frame")
                    .expect("queued test message"),
            )
            .expect("queue reader event");
        outcomes.publish(WriterCompletion {
            generation,
            sequence: WriteSequence::from_raw(1).expect("test write sequence"),
            outcome: WriterOutcome::Flushed,
        });
        shared.publish_launch(LaunchReport {
            generation,
            outcome: LaunchOutcome::Ready,
            lanes: None,
            release_phase_on_delivery: false,
        });
        shared.publish_child_exit(ChildExitReport {
            generation,
            status: BoundedChildExit::Success,
        });
        shared.fatal.store_first(AdapterFatal::Reader {
            generation,
            cause: ReaderFatalCause::Io,
        });
        shared.publish_finalization(FinalizationReport {
            generation,
            outcome: FinalizationOutcome::Finalized,
            stderr_tail: None,
            delivered: false,
        });
        Self {
            shared,
            wake: Some(wake),
            lanes: Some(ActiveClientLanes {
                generation,
                writer,
                reader,
                writer_outcomes: outcomes,
                stop: IoStop::new(),
            }),
            _worker: thread::spawn(|| {}),
        }
    }

    #[cfg(test)]
    pub(crate) fn move_test_lanes_into_pending_launch(&mut self) {
        let lanes = self.lanes.take().expect("test fixture has active lanes");
        let mut launch = self
            .shared
            .launch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let report = launch.as_mut().expect("test fixture has a launch report");
        assert!(report.lanes.replace(lanes).is_none());
    }

    #[cfg(test)]
    pub(crate) fn has_client_lanes_for_test(&self) -> bool {
        self.lanes.is_some()
            || self
                .shared
                .launch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|report| report.lanes.is_some())
    }

    #[cfg(test)]
    pub(crate) fn hold_control_lock_for_test(
        &self,
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    ) -> JoinHandle<()> {
        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            let _control = shared
                .control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entered.send(()).expect("observe held control lock");
            release.recv().expect("release held control lock");
        })
    }

    pub(crate) fn execute_effect(
        &mut self,
        effect: ActorEffect,
    ) -> Result<Option<ActorEvent>, AdapterControlError> {
        match effect {
            ActorEffect::LaunchGeneration { generation } => {
                let mut control = self
                    .shared
                    .control
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match control.phase {
                    WorkerPhase::Idle => {
                        if control
                            .last_retired
                            .is_some_and(|retired| generation <= retired)
                        {
                            return Err(AdapterControlError::GenerationMismatch);
                        }
                        if let Some(queued) = control.launch {
                            return if queued == generation {
                                Ok(None)
                            } else {
                                Err(AdapterControlError::GenerationBusy)
                            };
                        }
                        control.phase = WorkerPhase::Launching(generation);
                        control.launch = Some(generation);
                        self.shared.control_ready.notify_one();
                        Ok(None)
                    }
                    WorkerPhase::Launching(active) | WorkerPhase::Active(active)
                        if active == generation =>
                    {
                        Ok(None)
                    }
                    WorkerPhase::Publishing(active) if control.acknowledgement == Some(active) => {
                        if generation <= active
                            || control
                                .last_retired
                                .is_some_and(|retired| generation <= retired)
                        {
                            return Err(AdapterControlError::GenerationMismatch);
                        }
                        if let Some(queued) = control.launch {
                            return if queued == generation {
                                Ok(None)
                            } else {
                                Err(AdapterControlError::GenerationBusy)
                            };
                        }
                        control.launch = Some(generation);
                        self.shared.control_ready.notify_one();
                        Ok(None)
                    }
                    WorkerPhase::Closed => Err(AdapterControlError::Closed),
                    _ => Err(AdapterControlError::GenerationBusy),
                }
            }
            ActorEffect::CancelLaunch { generation } => {
                {
                    let mut control = self
                        .shared
                        .control
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !phase_matches(control.phase, generation)
                        && control.launch != Some(generation)
                    {
                        return Err(AdapterControlError::GenerationMismatch);
                    }
                    control.cancel = Some(generation);
                }
                self.close_lanes(generation);
                self.shared.control_ready.notify_one();
                Ok(None)
            }
            ActorEffect::SendFrame {
                generation,
                sequence,
                bytes,
                ..
            } => {
                let Some(lanes) = self.lanes.as_ref() else {
                    return Err(AdapterControlError::NoActiveLanes);
                };
                if lanes.generation != generation {
                    return Err(AdapterControlError::GenerationMismatch);
                }
                match lanes.writer.try_send(generation, sequence, bytes) {
                    Ok(()) => Ok(None),
                    Err(WriterHandoffError::Full | WriterHandoffError::Disconnected) => {
                        Ok(Some(ActorEvent::WriterFinished {
                            generation,
                            sequence,
                            outcome: WriterOutcome::HandoffRejected,
                        }))
                    }
                }
            }
            ActorEffect::BeginCleanup { generation, .. } => {
                {
                    let mut control = self
                        .shared
                        .control
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !phase_matches(control.phase, generation)
                        && control.launch != Some(generation)
                    {
                        return Err(AdapterControlError::GenerationMismatch);
                    }
                    control.cleanup = Some(generation);
                }
                self.close_lanes(generation);
                self.shared.control_ready.notify_one();
                Ok(None)
            }
            ActorEffect::PublishSnapshot { .. } => Err(AdapterControlError::UnsupportedEffect),
        }
    }

    pub(crate) fn poll_event(&mut self) -> Option<ActorEvent> {
        if let Some(fatal) = self.shared.fatal.take() {
            return Some(match fatal {
                AdapterFatal::Reader { generation, cause } => {
                    ActorEvent::ReaderFatal { generation, cause }
                }
                AdapterFatal::Writer { generation, cause } => {
                    ActorEvent::WriterFatal { generation, cause }
                }
            });
        }
        if let Some(report) = self
            .shared
            .child_exit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return Some(ActorEvent::ChildExited {
                generation: report.generation,
                status: report.status,
            });
        }
        if let Some(report) = self
            .shared
            .launch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            if let Some(lanes) = report.lanes {
                let keep = {
                    let control = self
                        .shared
                        .control
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    control.phase == WorkerPhase::Active(report.generation)
                        && control.cancel != Some(report.generation)
                        && control.cleanup != Some(report.generation)
                        && !control.shutdown
                        && !self.shared.coordinator_gone.load(Ordering::Acquire)
                };
                if keep {
                    self.lanes = Some(lanes);
                } else {
                    lanes.stop.request();
                }
            }
            if report.release_phase_on_delivery {
                let mut control = self
                    .shared
                    .control
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if control.phase == WorkerPhase::Launching(report.generation) {
                    control.phase = WorkerPhase::Idle;
                }
                control.last_retired = Some(
                    control
                        .last_retired
                        .map_or(report.generation, |retired| retired.max(report.generation)),
                );
                if control.cancel == Some(report.generation) {
                    control.cancel = None;
                }
                if control.cleanup == Some(report.generation) {
                    control.cleanup = None;
                }
                self.shared.control_ready.notify_one();
            }
            return Some(ActorEvent::LaunchFinished {
                generation: report.generation,
                outcome: report.outcome,
            });
        }
        if let Some(lanes) = self.lanes.as_ref()
            && let Some(outcome) = lanes.writer_outcomes.take()
        {
            return Some(ActorEvent::WriterFinished {
                generation: outcome.generation,
                sequence: outcome.sequence,
                outcome: outcome.outcome,
            });
        }
        let finalization_event = {
            let mut finalization = self
                .shared
                .finalization
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            finalization.as_mut().and_then(|report| {
                if report.delivered {
                    return None;
                }
                report.delivered = true;
                Some(match report.outcome {
                    FinalizationOutcome::Finalized => ActorEvent::FinalizedGeneration {
                        generation: report.generation,
                        stderr_tail: report.stderr_tail.clone(),
                    },
                    FinalizationOutcome::Quarantined(cause) => ActorEvent::CleanupFailed {
                        generation: report.generation,
                        cause,
                        stderr_tail: report.stderr_tail.clone(),
                    },
                })
            })
        };
        if finalization_event.is_some() {
            return finalization_event;
        }
        if let Some(lanes) = self.lanes.as_ref()
            && let Some(message) = lanes.reader.try_recv()
        {
            return Some(ActorEvent::ReaderMessage {
                generation: lanes.generation,
                message,
            });
        }
        None
    }

    pub(crate) fn wait_for_wake(&self, timeout: Duration) -> bool {
        let Some(wake) = &self.wake else {
            return false;
        };
        match wake.recv_timeout(timeout) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => false,
        }
    }

    pub(crate) fn acknowledge_finalization(
        &self,
        generation: ProcessGeneration,
    ) -> Result<(), AdapterControlError> {
        let mut control = self
            .shared
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if control.phase != WorkerPhase::Publishing(generation) {
            return Err(AdapterControlError::GenerationMismatch);
        }
        control.acknowledgement = Some(generation);
        self.shared.control_ready.notify_one();
        Ok(())
    }

    pub(crate) fn request_shutdown(&mut self) {
        {
            let mut control = self
                .shared
                .control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            control.shutdown = true;
        }
        self.close_all_lanes();
        self.shared.control_ready.notify_one();
    }

    fn close_lanes(&mut self, generation: ProcessGeneration) {
        if self
            .lanes
            .as_ref()
            .is_some_and(|lanes| lanes.generation == generation)
        {
            let lanes = self.lanes.take().expect("matching active lanes");
            lanes.stop.request();
        }
        let pending = self
            .shared
            .launch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .filter(|report| report.generation == generation)
            .and_then(|report| report.lanes.take());
        if let Some(lanes) = pending {
            lanes.stop.request();
        }
    }

    fn close_all_lanes(&mut self) {
        if let Some(lanes) = self.lanes.take() {
            lanes.stop.request();
        }
        let pending = self
            .shared
            .launch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .and_then(|report| report.lanes.take());
        if let Some(lanes) = pending {
            lanes.stop.request();
        }
    }
}

impl Drop for LanguageProcessAdapter {
    fn drop(&mut self) {
        if let Some(lanes) = self.lanes.take() {
            lanes.stop.request();
        }
        self.shared.coordinator_gone.store(true, Ordering::Release);
        self.shared.control_ready.notify_one();
        self.shared.wake();
    }
}

fn phase_matches(phase: WorkerPhase, generation: ProcessGeneration) -> bool {
    matches!(
        phase,
        WorkerPhase::Launching(active)
            | WorkerPhase::Active(active)
            | WorkerPhase::Cleaning(active)
            | WorkerPhase::Publishing(active)
            | WorkerPhase::Quarantined(active)
            if active == generation
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LaneResult<T> {
    Finished(T),
    Panicked,
}

fn spawn_lane<T, F>(
    name: String,
    completion: Arc<CompletionCell<LaneResult<T>>>,
    worker_shared: Arc<SharedProcessState>,
    body: F,
) -> io::Result<JoinHandle<LaneResult<T>>>
where
    T: Clone + Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    thread::Builder::new().name(name).spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
            .map_or(LaneResult::Panicked, LaneResult::Finished);
        completion.publish(outcome.clone());
        worker_shared.control_ready.notify_one();
        outcome
    })
}

struct IoThreadFactory {
    #[cfg(test)]
    fail_after: Option<usize>,
    #[cfg(test)]
    started: usize,
}

impl IoThreadFactory {
    fn from_config(config: &LanguageProcessConfig) -> Self {
        #[cfg(not(test))]
        let _ = config;
        Self {
            #[cfg(test)]
            fail_after: config.io_thread_fail_after,
            #[cfg(test)]
            started: 0,
        }
    }

    fn spawn<T, F>(
        &mut self,
        name: String,
        completion: Arc<CompletionCell<LaneResult<T>>>,
        worker_shared: Arc<SharedProcessState>,
        body: F,
    ) -> io::Result<JoinHandle<LaneResult<T>>>
    where
        T: Clone + Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        #[cfg(test)]
        if self.fail_after == Some(self.started) {
            return Err(io::Error::other("injected I/O thread startup failure"));
        }
        let handle = spawn_lane(name, completion, worker_shared, body)?;
        #[cfg(test)]
        {
            self.started += 1;
        }
        Ok(handle)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StderrThreadOutcome {
    tail: BoundedStderrTail,
    read_failed: bool,
}

fn run_stderr(mut stderr: impl Read) -> StderrThreadOutcome {
    let mut retained = BoundedStderrBuffer::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => {
                return StderrThreadOutcome {
                    tail: retained.finish(),
                    read_failed: false,
                };
            }
            Ok(read) => retained.push(&chunk[..read]),
            Err(_) => {
                return StderrThreadOutcome {
                    tail: retained.finish(),
                    read_failed: true,
                };
            }
        }
    }
}

struct StartedIoThreads {
    writer: Option<JoinHandle<LaneResult<WriterThreadOutcome>>>,
    writer_completion: Arc<CompletionCell<LaneResult<WriterThreadOutcome>>>,
    reader: Option<JoinHandle<LaneResult<ReaderThreadOutcome>>>,
    reader_completion: Arc<CompletionCell<LaneResult<ReaderThreadOutcome>>>,
    stderr: Option<JoinHandle<LaneResult<StderrThreadOutcome>>>,
    stderr_completion: Arc<CompletionCell<LaneResult<StderrThreadOutcome>>>,
}

struct ActiveProcess {
    generation: ProcessGeneration,
    process: ContainedChild,
    threads: StartedIoThreads,
    stop: IoStop,
    terminal_reported: bool,
}

impl ActiveProcess {
    fn take_terminal_fatal(&mut self) -> Option<AdapterFatal> {
        if self.terminal_reported {
            return None;
        }
        let fatal = match self.threads.reader_completion.cloned() {
            Some(LaneResult::Finished(ReaderThreadOutcome::CleanEof)) => {
                Some(AdapterFatal::Reader {
                    generation: self.generation,
                    cause: ReaderFatalCause::Io,
                })
            }
            Some(LaneResult::Panicked) => Some(AdapterFatal::Reader {
                generation: self.generation,
                cause: ReaderFatalCause::AdapterInvariant,
            }),
            _ => match self.threads.writer_completion.cloned() {
                Some(LaneResult::Panicked) => Some(AdapterFatal::Writer {
                    generation: self.generation,
                    cause: WriterFatalCause::AdapterInvariant,
                }),
                _ => None,
            },
        };
        if fatal.is_some() {
            self.terminal_reported = true;
        }
        fatal
    }
}

enum WorkerOwnership {
    Idle,
    Partial {
        generation: ProcessGeneration,
        partial: PartialContainedChild,
    },
    Active(ActiveProcess),
    Publishing(ProcessGeneration),
    Quarantined(QuarantinedGeneration),
}

struct QuarantinedGeneration {
    _generation: ProcessGeneration,
    _active: Option<ActiveProcess>,
    _partial: Option<PartialContainedChild>,
}

fn process_worker(shared: Arc<SharedProcessState>, config: LanguageProcessConfig) {
    let mut ownership = WorkerOwnership::Idle;
    loop {
        ownership = match ownership {
            WorkerOwnership::Idle => {
                let launch = wait_for_idle_work(&shared, config.process_tick);
                match launch {
                    IdleWork::Launch(generation) => {
                        config.wait_at_launch_gate(generation);
                        launch_generation(&shared, &config, generation)
                    }
                    IdleWork::Shutdown => {
                        set_phase(&shared, WorkerPhase::Closed);
                        return;
                    }
                    IdleWork::None => WorkerOwnership::Idle,
                }
            }
            WorkerOwnership::Partial {
                generation,
                partial,
            } => {
                if cleanup_requested(&shared, generation) {
                    set_phase(&shared, WorkerPhase::Cleaning(generation));
                    cleanup_partial(&shared, generation, partial, config.cleanup_timeout)
                } else {
                    wait_for_control(&shared, config.process_tick);
                    WorkerOwnership::Partial {
                        generation,
                        partial,
                    }
                }
            }
            WorkerOwnership::Active(mut active) => {
                let generation = active.generation;
                if cleanup_requested(&shared, generation) {
                    active.stop.request();
                    set_phase(&shared, WorkerPhase::Cleaning(generation));
                    cleanup_active(&shared, active, config.cleanup_timeout)
                } else {
                    match active.process.try_wait_root() {
                        Ok(Some(status)) => {
                            shared.publish_child_exit(ChildExitReport {
                                generation,
                                status: bounded_exit(status),
                            });
                            active.stop.request();
                            set_phase(&shared, WorkerPhase::Cleaning(generation));
                            cleanup_active(&shared, active, config.cleanup_timeout)
                        }
                        Err(_) => {
                            shared.publish_child_exit(ChildExitReport {
                                generation,
                                status: BoundedChildExit::Failure(None),
                            });
                            active.stop.request();
                            set_phase(&shared, WorkerPhase::Cleaning(generation));
                            cleanup_active(&shared, active, config.cleanup_timeout)
                        }
                        Ok(None) => {
                            if let Some(fatal) = active.take_terminal_fatal() {
                                shared.fatal.store_first(fatal);
                            }
                            wait_for_control(&shared, config.process_tick);
                            WorkerOwnership::Active(active)
                        }
                    }
                }
            }
            WorkerOwnership::Publishing(generation) => {
                let mut control = shared
                    .control
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if control.acknowledgement == Some(generation) {
                    control.acknowledgement = None;
                    control.last_retired = Some(
                        control
                            .last_retired
                            .map_or(generation, |retired| retired.max(generation)),
                    );
                    control.phase = WorkerPhase::Idle;
                    drop(control);
                    shared
                        .finalization
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    WorkerOwnership::Idle
                } else if shared.coordinator_gone.load(Ordering::Acquire) {
                    return;
                } else {
                    let (_guard, _) = shared
                        .control_ready
                        .wait_timeout(control, config.process_tick)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    WorkerOwnership::Publishing(generation)
                }
            }
            WorkerOwnership::Quarantined(quarantine) => {
                let control = shared
                    .control
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (_guard, _) = shared
                    .control_ready
                    .wait_timeout(control, config.process_tick)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                WorkerOwnership::Quarantined(quarantine)
            }
        };
    }
}

enum IdleWork {
    Launch(ProcessGeneration),
    Shutdown,
    None,
}

fn wait_for_idle_work(shared: &SharedProcessState, timeout: Duration) -> IdleWork {
    let mut control = shared
        .control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if control.shutdown || shared.coordinator_gone.load(Ordering::Acquire) {
        return IdleWork::Shutdown;
    }
    if let Some(generation) = control.launch.take() {
        control.phase = WorkerPhase::Launching(generation);
        return IdleWork::Launch(generation);
    }
    let (next, _) = shared
        .control_ready
        .wait_timeout(control, timeout)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    control = next;
    if control.shutdown || shared.coordinator_gone.load(Ordering::Acquire) {
        IdleWork::Shutdown
    } else if let Some(generation) = control.launch.take() {
        control.phase = WorkerPhase::Launching(generation);
        IdleWork::Launch(generation)
    } else {
        IdleWork::None
    }
}

fn wait_for_control(shared: &SharedProcessState, timeout: Duration) {
    let control = shared
        .control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = shared
        .control_ready
        .wait_timeout(control, timeout)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
}

fn set_phase(shared: &SharedProcessState, phase: WorkerPhase) {
    shared
        .control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .phase = phase;
}

fn cleanup_requested(shared: &SharedProcessState, generation: ProcessGeneration) -> bool {
    let mut control = shared
        .control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let requested = control.shutdown
        || shared.coordinator_gone.load(Ordering::Acquire)
        || control.cancel == Some(generation)
        || control.cleanup == Some(generation);
    if requested {
        if control.cancel == Some(generation) {
            control.cancel = None;
        }
        if control.cleanup == Some(generation) {
            control.cleanup = None;
        }
    }
    requested
}

fn launch_generation(
    shared: &Arc<SharedProcessState>,
    config: &LanguageProcessConfig,
    generation: ProcessGeneration,
) -> WorkerOwnership {
    match ContainedPipedChild::spawn(config.command()) {
        ContainedSpawnAttempt::FailedNoOwner { cause } => {
            let cancelled = cleanup_requested(shared, generation);
            shared.publish_launch(LaunchReport {
                generation,
                outcome: LaunchOutcome::FailedBeforeOwnership {
                    cause: if cancelled {
                        LaunchFailure::Cancelled
                    } else {
                        launch_failure(cause)
                    },
                },
                lanes: None,
                release_phase_on_delivery: true,
            });
            WorkerOwnership::Idle
        }
        ContainedSpawnAttempt::FailedWithOwner { cause, partial } => {
            shared.publish_launch(LaunchReport {
                generation,
                outcome: LaunchOutcome::FailedWithOwnedResources {
                    cause: launch_failure(cause),
                },
                lanes: None,
                release_phase_on_delivery: false,
            });
            set_phase(shared, WorkerPhase::Active(generation));
            WorkerOwnership::Partial {
                generation,
                partial,
            }
        }
        ContainedSpawnAttempt::Ready(child) => match start_io(shared, generation, child, config) {
            Ok((active, lanes)) => {
                config.wait_at_ready_gate(generation);
                let mut control = shared
                    .control
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let cancelled = control.shutdown
                    || shared.coordinator_gone.load(Ordering::Acquire)
                    || control.cancel == Some(generation)
                    || control.cleanup == Some(generation);
                if control.cancel == Some(generation) {
                    control.cancel = None;
                }
                if control.cleanup == Some(generation) {
                    control.cleanup = None;
                }
                if cancelled {
                    control.phase = WorkerPhase::Cleaning(generation);
                    shared.publish_launch(LaunchReport {
                        generation,
                        outcome: LaunchOutcome::FailedWithOwnedResources {
                            cause: LaunchFailure::Cancelled,
                        },
                        lanes: None,
                        release_phase_on_delivery: false,
                    });
                    drop(control);
                    cleanup_active(shared, active, config.cleanup_timeout)
                } else {
                    control.phase = WorkerPhase::Active(generation);
                    shared.publish_launch(LaunchReport {
                        generation,
                        outcome: LaunchOutcome::Ready,
                        lanes: Some(lanes),
                        release_phase_on_delivery: false,
                    });
                    drop(control);
                    WorkerOwnership::Active(active)
                }
            }
            Err(active) => {
                shared.publish_launch(LaunchReport {
                    generation,
                    outcome: LaunchOutcome::FailedWithOwnedResources {
                        cause: LaunchFailure::Thread,
                    },
                    lanes: None,
                    release_phase_on_delivery: false,
                });
                set_phase(shared, WorkerPhase::Active(generation));
                WorkerOwnership::Active(*active)
            }
        },
    }
}

fn launch_failure(cause: CachedIoError) -> LaunchFailure {
    match cause.kind() {
        io::ErrorKind::NotFound => LaunchFailure::MissingSibling,
        io::ErrorKind::BrokenPipe => LaunchFailure::Pipe,
        _ => LaunchFailure::Containment,
    }
}

fn start_io(
    shared: &Arc<SharedProcessState>,
    generation: ProcessGeneration,
    child: ContainedPipedChild,
    config: &LanguageProcessConfig,
) -> Result<(ActiveProcess, ActiveClientLanes), Box<ActiveProcess>> {
    let (process, stdin, stdout, stderr) = child.into_parts();
    let stop = IoStop::new();
    let outcomes = WriterOutcomeSlot::with_wake(shared.wake.clone());
    let (writer, writer_receiver) = writer_input();
    let budget = InboxBudget::new(READER_INBOX_BODY_BYTES);
    let (reader_sender, reader) = reader_lane_with_wake(budget, shared.wake.clone());
    let writer_completion = Arc::new(CompletionCell::new());
    let reader_completion = Arc::new(CompletionCell::new());
    let stderr_completion = Arc::new(CompletionCell::new());
    let mut threads = IoThreadFactory::from_config(config);

    let writer_handle = threads.spawn(
        format!("oxide-language-writer-{}", generation.get()),
        Arc::clone(&writer_completion),
        Arc::clone(shared),
        {
            let stop = stop.clone();
            let outcomes = outcomes.clone();
            let fatal = shared.fatal.clone();
            #[cfg(test)]
            let panic = config.io_lane_panic == Some(TestIoLanePanic::Writer);
            move || {
                #[cfg(test)]
                if panic {
                    panic!("injected writer-lane panic");
                }
                run_writer(stdin, generation, writer_receiver, stop, &outcomes, &fatal)
            }
        },
    );
    let Ok(writer_handle) = writer_handle else {
        stop.request();
        drop((stdout, stderr, writer, reader));
        return Err(Box::new(ActiveProcess {
            generation,
            process,
            threads: StartedIoThreads {
                writer: None,
                writer_completion,
                reader: None,
                reader_completion,
                stderr: None,
                stderr_completion,
            },
            stop,
            terminal_reported: false,
        }));
    };
    let reader_handle = threads.spawn(
        format!("oxide-language-reader-{}", generation.get()),
        Arc::clone(&reader_completion),
        Arc::clone(shared),
        {
            let fatal = shared.fatal.clone();
            #[cfg(test)]
            let panic = config.io_lane_panic == Some(TestIoLanePanic::Reader);
            move || {
                #[cfg(test)]
                if panic {
                    panic!("injected reader-lane panic");
                }
                run_reader(stdout, generation, reader_sender, &fatal)
            }
        },
    );
    let Ok(reader_handle) = reader_handle else {
        stop.request();
        drop((stderr, writer, reader));
        return Err(Box::new(ActiveProcess {
            generation,
            process,
            threads: StartedIoThreads {
                writer: Some(writer_handle),
                writer_completion,
                reader: None,
                reader_completion,
                stderr: None,
                stderr_completion,
            },
            stop,
            terminal_reported: false,
        }));
    };
    let stderr_handle = threads.spawn(
        format!("oxide-language-stderr-{}", generation.get()),
        Arc::clone(&stderr_completion),
        Arc::clone(shared),
        move || run_stderr(stderr),
    );
    let Ok(stderr_handle) = stderr_handle else {
        stop.request();
        drop((writer, reader));
        return Err(Box::new(ActiveProcess {
            generation,
            process,
            threads: StartedIoThreads {
                writer: Some(writer_handle),
                writer_completion,
                reader: Some(reader_handle),
                reader_completion,
                stderr: None,
                stderr_completion,
            },
            stop,
            terminal_reported: false,
        }));
    };

    Ok((
        ActiveProcess {
            generation,
            process,
            threads: StartedIoThreads {
                writer: Some(writer_handle),
                writer_completion,
                reader: Some(reader_handle),
                reader_completion,
                stderr: Some(stderr_handle),
                stderr_completion,
            },
            stop: stop.clone(),
            terminal_reported: false,
        },
        ActiveClientLanes {
            generation,
            writer,
            reader,
            writer_outcomes: outcomes,
            stop,
        },
    ))
}

fn cleanup_partial(
    shared: &SharedProcessState,
    generation: ProcessGeneration,
    partial: PartialContainedChild,
    timeout: Duration,
) -> WorkerOwnership {
    match cleanup_partial_owner(partial, timeout) {
        Ok(()) => publish_finalized(shared, generation, None),
        Err((cause, partial)) => {
            publish_quarantined(shared, generation, cause, None, Some(*partial), None)
        }
    }
}

pub(crate) fn cleanup_partial_owner(
    partial: PartialContainedChild,
    timeout: Duration,
) -> Result<(), (CleanupFailure, Box<PartialContainedChild>)> {
    cleanup_partial_owner_with(partial, timeout, |partial, deadline| {
        partial.cleanup_for_language_until(deadline)
    })
}

pub(crate) fn cleanup_partial_owner_with(
    partial: PartialContainedChild,
    timeout: Duration,
    cleanup: impl FnOnce(&mut PartialContainedChild, Instant) -> Result<(), PartialCleanupFailure>,
) -> Result<(), (CleanupFailure, Box<PartialContainedChild>)> {
    let now = Instant::now();
    cleanup_partial_owner_at(partial, now, timeout, cleanup)
}

pub(crate) fn cleanup_partial_owner_at(
    mut partial: PartialContainedChild,
    now: Instant,
    timeout: Duration,
    cleanup: impl FnOnce(&mut PartialContainedChild, Instant) -> Result<(), PartialCleanupFailure>,
) -> Result<(), (CleanupFailure, Box<PartialContainedChild>)> {
    let deadline = now.checked_add(timeout).unwrap_or(now);
    match cleanup(&mut partial, deadline) {
        Ok(()) => Ok(()),
        Err(cause) => {
            let cause = match cause {
                PartialCleanupFailure::Terminate => CleanupFailure::Terminate,
                PartialCleanupFailure::Reap => CleanupFailure::Reap,
                #[cfg(windows)]
                PartialCleanupFailure::VerifyTreeEmpty => CleanupFailure::VerifyTreeEmpty,
            };
            Err((cause, Box::new(partial)))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CleanupOpError;

pub(crate) trait CleanupProcessOps {
    fn now(&mut self) -> Instant;
    fn root_exited(&mut self) -> Result<bool, CleanupOpError>;
    fn terminate_tree_once(&mut self) -> Result<(), CleanupOpError>;
    fn reap_root_until(&mut self, deadline: Instant) -> Result<(), CleanupOpError>;

    #[cfg(windows)]
    fn active_processes(&mut self) -> Result<u32, CleanupOpError>;
    #[cfg(windows)]
    fn wait_for_progress_until(&mut self, deadline: Instant);
}

struct SystemCleanupProcess<'a> {
    process: &'a mut ContainedChild,
}

impl CleanupProcessOps for SystemCleanupProcess<'_> {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn root_exited(&mut self) -> Result<bool, CleanupOpError> {
        self.process
            .try_wait_root()
            .map(|status| status.is_some())
            .map_err(|_| CleanupOpError)
    }

    fn terminate_tree_once(&mut self) -> Result<(), CleanupOpError> {
        self.process
            .terminate_tree_once()
            .map_err(|_| CleanupOpError)
    }

    fn reap_root_until(&mut self, deadline: Instant) -> Result<(), CleanupOpError> {
        self.process
            .wait_root_until(deadline)
            .map(|_| ())
            .map_err(|_| CleanupOpError)
    }

    #[cfg(windows)]
    fn active_processes(&mut self) -> Result<u32, CleanupOpError> {
        self.process.active_processes().map_err(|_| CleanupOpError)
    }

    #[cfg(windows)]
    fn wait_for_progress_until(&mut self, deadline: Instant) {
        thread::park_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(1)),
        );
    }
}

pub(crate) fn run_process_cleanup(
    process: &mut impl CleanupProcessOps,
    timeout: Duration,
) -> (Instant, Option<CleanupFailure>) {
    let now = process.now();
    let deadline = now.checked_add(timeout).unwrap_or(now);
    let _root_already_exited = process.root_exited().unwrap_or(false);
    let mut failure = None;

    #[cfg(windows)]
    let tree_was_empty = match process.active_processes() {
        Ok(0) => true,
        Ok(_) => false,
        Err(CleanupOpError) => {
            failure = Some(CleanupFailure::VerifyTreeEmpty);
            false
        }
    };
    #[cfg(not(windows))]
    let tree_was_empty = _root_already_exited;

    if !tree_was_empty && process.terminate_tree_once().is_err() {
        failure.get_or_insert(CleanupFailure::Terminate);
    }
    if process.reap_root_until(deadline).is_err() {
        failure.get_or_insert(CleanupFailure::Reap);
    }

    #[cfg(windows)]
    if failure.is_none() {
        loop {
            match process.active_processes() {
                Ok(0) => break,
                Ok(_) => {
                    let now = process.now();
                    if now >= deadline {
                        failure = Some(CleanupFailure::VerifyTreeEmpty);
                        break;
                    }
                    process.wait_for_progress_until(deadline);
                }
                Err(CleanupOpError) => {
                    failure = Some(CleanupFailure::VerifyTreeEmpty);
                    break;
                }
            }
        }
    }

    (deadline, failure)
}

pub(crate) trait CleanupLaneOps {
    fn finish_writer(&mut self, deadline: Instant) -> Result<bool, CleanupOpError>;
    fn finish_reader(&mut self, deadline: Instant) -> Result<bool, CleanupOpError>;
    fn finish_stderr(
        &mut self,
        deadline: Instant,
    ) -> Result<(bool, Option<BoundedStderrTail>), CleanupOpError>;
}

struct SystemCleanupLanes<'a> {
    threads: &'a mut StartedIoThreads,
}

impl CleanupLaneOps for SystemCleanupLanes<'_> {
    fn finish_writer(&mut self, deadline: Instant) -> Result<bool, CleanupOpError> {
        finish_lane(
            &self.threads.writer_completion,
            &mut self.threads.writer,
            deadline,
        )
        .map(|outcome| matches!(outcome, Some(LaneResult::Panicked)))
        .map_err(|()| CleanupOpError)
    }

    fn finish_reader(&mut self, deadline: Instant) -> Result<bool, CleanupOpError> {
        finish_lane(
            &self.threads.reader_completion,
            &mut self.threads.reader,
            deadline,
        )
        .map(|outcome| matches!(outcome, Some(LaneResult::Panicked)))
        .map_err(|()| CleanupOpError)
    }

    fn finish_stderr(
        &mut self,
        deadline: Instant,
    ) -> Result<(bool, Option<BoundedStderrTail>), CleanupOpError> {
        finish_lane(
            &self.threads.stderr_completion,
            &mut self.threads.stderr,
            deadline,
        )
        .map(|outcome| match outcome {
            Some(LaneResult::Finished(outcome)) => (false, Some(outcome.tail)),
            Some(LaneResult::Panicked) => (true, None),
            None => (false, None),
        })
        .map_err(|()| CleanupOpError)
    }
}

pub(crate) fn run_lane_cleanup(
    lanes: &mut impl CleanupLaneOps,
    deadline: Instant,
) -> (Option<CleanupFailure>, Option<BoundedStderrTail>) {
    let writer = lanes.finish_writer(deadline);
    let reader = lanes.finish_reader(deadline);
    let stderr = lanes.finish_stderr(deadline);
    let failed = writer.as_ref().map_or(true, |panicked| *panicked)
        || reader.as_ref().map_or(true, |panicked| *panicked)
        || stderr.as_ref().map_or(true, |(panicked, _tail)| *panicked);
    let tail = stderr.ok().and_then(|(_panicked, tail)| tail);
    (failed.then_some(CleanupFailure::Join), tail)
}

pub(crate) fn resolve_cleanup_owner<T>(
    owner: T,
    failure: Option<CleanupFailure>,
) -> Result<T, (CleanupFailure, T)> {
    match failure {
        Some(cause) => Err((cause, owner)),
        None => Ok(owner),
    }
}

pub(crate) fn after_owner_drop<T, R>(owner: T, publish: impl FnOnce() -> R) -> R {
    drop(owner);
    publish()
}

fn cleanup_active(
    shared: &SharedProcessState,
    mut active: ActiveProcess,
    timeout: Duration,
) -> WorkerOwnership {
    let generation = active.generation;
    active.stop.request();
    let (deadline, mut failure) = run_process_cleanup(
        &mut SystemCleanupProcess {
            process: &mut active.process,
        },
        timeout,
    );

    let (lane_failure, tail) = run_lane_cleanup(
        &mut SystemCleanupLanes {
            threads: &mut active.threads,
        },
        deadline,
    );
    failure = failure.or(lane_failure);

    match resolve_cleanup_owner(active, failure) {
        Ok(proven_clean) => {
            after_owner_drop(proven_clean, || publish_finalized(shared, generation, tail))
        }
        Err((cause, active)) => {
            publish_quarantined(shared, generation, cause, Some(active), None, tail)
        }
    }
}

fn finish_lane<T: Clone + Send + 'static>(
    completion: &CompletionCell<LaneResult<T>>,
    handle: &mut Option<JoinHandle<LaneResult<T>>>,
    deadline: Instant,
) -> Result<Option<LaneResult<T>>, ()> {
    let Some(join) = handle.as_ref() else {
        return Ok(None);
    };
    if !completion.wait_until(deadline) {
        return Err(());
    }
    while !join.is_finished() {
        let now = Instant::now();
        if now >= deadline {
            return Err(());
        }
        thread::park_timeout(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(1)),
        );
    }
    join_if_completed(completion, handle).map_err(|_| ())
}

fn publish_finalized(
    shared: &SharedProcessState,
    generation: ProcessGeneration,
    tail: Option<BoundedStderrTail>,
) -> WorkerOwnership {
    set_phase(shared, WorkerPhase::Publishing(generation));
    shared.publish_finalization(FinalizationReport {
        generation,
        outcome: FinalizationOutcome::Finalized,
        stderr_tail: tail,
        delivered: false,
    });
    WorkerOwnership::Publishing(generation)
}

fn publish_quarantined(
    shared: &SharedProcessState,
    generation: ProcessGeneration,
    cause: CleanupFailure,
    active: Option<ActiveProcess>,
    partial: Option<PartialContainedChild>,
    stderr_tail: Option<BoundedStderrTail>,
) -> WorkerOwnership {
    set_phase(shared, WorkerPhase::Quarantined(generation));
    shared.publish_finalization(FinalizationReport {
        generation,
        outcome: FinalizationOutcome::Quarantined(cause),
        stderr_tail,
        delivered: false,
    });
    WorkerOwnership::Quarantined(QuarantinedGeneration {
        _generation: generation,
        _active: active,
        _partial: partial,
    })
}

fn bounded_exit(status: ExitStatus) -> BoundedChildExit {
    if status.success() {
        BoundedChildExit::Success
    } else {
        BoundedChildExit::Failure(status.code())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SiblingResolutionError {
    InvalidCurrentExecutable,
    Missing,
    Metadata(io::ErrorKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanguageProcessCommand {
    pub(crate) executable: PathBuf,
    pub(crate) arguments: Vec<OsString>,
}

pub(crate) fn resolve_language_command_from(
    current_executable: &Path,
    is_file: impl FnOnce(&Path) -> io::Result<bool>,
) -> Result<LanguageProcessCommand, SiblingResolutionError> {
    if !current_executable.is_absolute() {
        return Err(SiblingResolutionError::InvalidCurrentExecutable);
    }

    #[cfg(windows)]
    {
        let _ = is_file;
        Ok(LanguageProcessCommand {
            executable: current_executable.to_path_buf(),
            arguments: vec![OsString::from(crate::LANGUAGE_WORKER_ARGUMENT)],
        })
    }

    #[cfg(not(windows))]
    {
        let executable = resolve_sibling_lsp_from(current_executable, is_file)?;
        Ok(LanguageProcessCommand {
            executable,
            arguments: Vec::new(),
        })
    }
}

#[cfg(not(windows))]
pub(crate) fn resolve_sibling_lsp_from(
    current_executable: &Path,
    is_file: impl FnOnce(&Path) -> io::Result<bool>,
) -> Result<PathBuf, SiblingResolutionError> {
    if !current_executable.is_absolute() {
        return Err(SiblingResolutionError::InvalidCurrentExecutable);
    }
    let parent = current_executable
        .parent()
        .filter(|parent| parent.is_absolute())
        .ok_or(SiblingResolutionError::InvalidCurrentExecutable)?;
    let candidate = parent.join("rlox-lsp");
    match is_file(&candidate) {
        Ok(true) => Ok(candidate),
        Ok(false) => Err(SiblingResolutionError::Missing),
        Err(error) => Err(SiblingResolutionError::Metadata(error.kind())),
    }
}

pub(crate) struct BoundedStderrBuffer {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl BoundedStderrBuffer {
    pub(crate) fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
            truncated: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes.iter().copied());
        self.enforce_byte_limit();
        self.enforce_line_limit();
    }

    pub(crate) fn finish(self) -> BoundedStderrTail {
        let raw: Vec<u8> = self.bytes.into_iter().collect();
        let mut text = String::from_utf8_lossy(&raw).into_owned();
        let mut truncated = self.truncated;
        if text.len() > MAX_STDERR_BYTES {
            let mut start = text.len() - MAX_STDERR_BYTES;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            text.drain(..start);
            truncated = true;
        }
        let line_count = logical_line_count(text.as_bytes());
        BoundedStderrTail {
            text: text.into(),
            line_count,
            truncated,
        }
    }

    fn enforce_byte_limit(&mut self) {
        if self.bytes.len() <= MAX_STDERR_BYTES {
            return;
        }
        let remove = self.bytes.len() - MAX_STDERR_BYTES;
        self.bytes.drain(..remove);
        self.truncated = true;
    }

    fn enforce_line_limit(&mut self) {
        while logical_line_count_deque(&self.bytes) > MAX_STDERR_LINES {
            let Some(newline) = self.bytes.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            self.bytes.drain(..=newline);
            self.truncated = true;
        }
    }
}

impl Default for BoundedStderrBuffer {
    fn default() -> Self {
        Self::new()
    }
}

fn logical_line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(bytes.last() != Some(&b'\n'))
}

fn logical_line_count_deque(bytes: &VecDeque<u8>) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(bytes.back() != Some(&b'\n'))
}
