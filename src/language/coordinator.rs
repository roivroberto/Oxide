use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;

use crate::{DocumentStamp, NavigationGeneration};

use super::actor::{
    ActorEffect, ActorEvent, ActorSeeds, DurableDesiredDocument, LanguageActor, MAX_NOTICES,
    ManualTick, READER_INBOX_ITEMS, UiDocumentFence,
};
use super::process_adapter::{LanguageProcessAdapter, LanguageProcessConfig};
use super::snapshot::{
    CaretGeneration, DefinitionResultId, LanguageSnapshot, LanguageSnapshotDraft, LanguageStatus,
    LspVersion, NoticeId, ProcessGeneration, SnapshotPublisher, SnapshotRevision,
    SnapshotSubscriber, WriterState, snapshot_channel,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DesiredDocument {
    pub(crate) stamp: DocumentStamp,
    pub(crate) text: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CaretIntent {
    pub(crate) process_generation: ProcessGeneration,
    pub(crate) stamp: DocumentStamp,
    pub(crate) generation: CaretGeneration,
    pub(crate) primary_character: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CaretCommand {
    Current(CaretIntent),
    Exhausted {
        process_generation: ProcessGeneration,
        stamp: DocumentStamp,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefinitionIntent {
    pub(crate) process_generation: ProcessGeneration,
    pub(crate) navigation_generation: NavigationGeneration,
    pub(crate) stamp: DocumentStamp,
    pub(crate) caret_generation: CaretGeneration,
    pub(crate) caret_character: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AcknowledgementBatch {
    pub(crate) process_generation: ProcessGeneration,
    pub(crate) observed_revision: SnapshotRevision,
    pub(crate) definition: Option<DefinitionResultId>,
    pub(crate) notices: Arc<[NoticeId]>,
}

impl AcknowledgementBatch {
    pub(crate) fn new(
        process_generation: ProcessGeneration,
        observed_revision: SnapshotRevision,
        definition: Option<DefinitionResultId>,
        notices: Arc<[NoticeId]>,
    ) -> Option<Self> {
        (notices.len() <= MAX_NOTICES).then_some(Self {
            process_generation,
            observed_revision,
            definition,
            notices,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LanguageClosed;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DocumentCommand {
    Set(DesiredDocument),
    Close,
}

pub(super) struct CommandMailbox {
    document: ArcSwapOption<DocumentCommand>,
    caret: ArcSwapOption<CaretCommand>,
    definition: ArcSwapOption<DefinitionIntent>,
    acknowledgement: ArcSwapOption<AcknowledgementBatch>,
    shutdown: AtomicBool,
    closed: AtomicBool,
    wake: mpsc::SyncSender<()>,
    #[cfg(test)]
    before_store: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl CommandMailbox {
    pub(super) fn channel() -> (Arc<Self>, mpsc::Receiver<()>) {
        let (wake, receiver) = mpsc::sync_channel(1);
        (
            Arc::new(Self {
                document: ArcSwapOption::empty(),
                caret: ArcSwapOption::empty(),
                definition: ArcSwapOption::empty(),
                acknowledgement: ArcSwapOption::empty(),
                shutdown: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                wake,
                #[cfg(test)]
                before_store: Mutex::new(None),
            }),
            receiver,
        )
    }

    fn accepts_commands(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && !self.shutdown.load(Ordering::Acquire)
    }

    fn notify(&self) {
        let _ = self.wake.try_send(());
    }

    #[cfg(test)]
    fn run_before_store_hook(&self) {
        let hook = self
            .before_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(not(test))]
    fn run_before_store_hook(&self) {}

    fn store_latest<T>(
        &self,
        slot: &ArcSwapOption<T>,
        value: Arc<T>,
    ) -> Result<(), LanguageClosed> {
        if !self.accepts_commands() {
            return Err(LanguageClosed);
        }
        self.run_before_store_hook();
        slot.store(Some(Arc::clone(&value)));
        if !self.accepts_commands() {
            let expected = Some(value);
            let _ = slot.compare_and_swap(&expected, None);
            return Err(LanguageClosed);
        }
        self.notify();
        Ok(())
    }

    pub(super) fn submit_document(
        &self,
        desired: Option<DesiredDocument>,
    ) -> Result<(), LanguageClosed> {
        let command = desired.map_or(DocumentCommand::Close, DocumentCommand::Set);
        self.store_latest(&self.document, Arc::new(command))
    }

    pub(super) fn submit_caret(&self, command: CaretCommand) -> Result<(), LanguageClosed> {
        self.store_latest(&self.caret, Arc::new(command))
    }

    pub(super) fn request_definition(
        &self,
        intent: DefinitionIntent,
    ) -> Result<(), LanguageClosed> {
        self.store_latest(&self.definition, Arc::new(intent))
    }

    pub(super) fn acknowledge_items(
        &self,
        batch: AcknowledgementBatch,
    ) -> Result<(), LanguageClosed> {
        if !self.accepts_commands() {
            return Err(LanguageClosed);
        }
        self.run_before_store_hook();
        let incoming = Arc::new(batch);
        let mut current = self.acknowledgement.load_full();
        let stored = loop {
            let next = Arc::new(match &current {
                Some(existing) => merge_acknowledgements(existing, &incoming),
                None => incoming.as_ref().clone(),
            });
            let previous = self
                .acknowledgement
                .compare_and_swap(&current, Some(Arc::clone(&next)));
            if option_arc_ptr_eq(&previous, &current) {
                break next;
            }
            current = previous.as_ref().map(Arc::clone);
        };
        if !self.accepts_commands() {
            let expected = Some(stored);
            let _ = self.acknowledgement.compare_and_swap(&expected, None);
            return Err(LanguageClosed);
        }
        self.notify();
        Ok(())
    }

    pub(super) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.notify();
    }

    fn wake_sender(&self) -> mpsc::SyncSender<()> {
        self.wake.clone()
    }

    fn has_pending_slots(&self) -> bool {
        self.document.load().is_some()
            || self.caret.load().is_some()
            || self.definition.load().is_some()
            || self.acknowledgement.load().is_some()
    }

    pub(super) fn take_document(&self) -> Option<Arc<DocumentCommand>> {
        self.document.swap(None)
    }

    pub(super) fn take_caret(&self) -> Option<Arc<CaretCommand>> {
        self.caret.swap(None)
    }

    pub(super) fn take_definition(&self) -> Option<Arc<DefinitionIntent>> {
        self.definition.swap(None)
    }

    pub(super) fn take_acknowledgement(&self) -> Option<Arc<AcknowledgementBatch>> {
        self.acknowledgement.swap(None)
    }

    pub(super) fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.document.store(None);
        self.caret.store(None);
        self.definition.store(None);
        self.acknowledgement.store(None);
        self.notify();
    }

    #[cfg(test)]
    pub(super) fn test_set_before_store_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self
            .before_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
    }
}

fn option_arc_ptr_eq<T>(left: &Option<Arc<T>>, right: &Option<Arc<T>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn merge_acknowledgements(
    existing: &AcknowledgementBatch,
    incoming: &AcknowledgementBatch,
) -> AcknowledgementBatch {
    if incoming.process_generation > existing.process_generation {
        return incoming.clone();
    }
    if incoming.process_generation < existing.process_generation {
        return existing.clone();
    }
    let incoming_is_newer = incoming.observed_revision >= existing.observed_revision;
    let (newer, older) = if incoming_is_newer {
        (incoming, existing)
    } else {
        (existing, incoming)
    };
    let mut notices = Vec::with_capacity(MAX_NOTICES);
    for id in newer.notices.iter().chain(older.notices.iter()) {
        if notices.len() == MAX_NOTICES {
            break;
        }
        if !notices.contains(id) {
            notices.push(*id);
        }
    }
    AcknowledgementBatch {
        process_generation: newer.process_generation,
        observed_revision: newer.observed_revision,
        definition: newer.definition.or(older.definition),
        notices: notices.into(),
    }
}

pub(crate) struct LanguageCoordinator {
    mailbox: Arc<CommandMailbox>,
    snapshots: SnapshotSubscriber,
    actor_thread: Option<JoinHandle<()>>,
}

impl LanguageCoordinator {
    pub(crate) fn start(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        let (publisher, snapshots) = initial_snapshot_channel();
        let (mailbox, wake) = CommandMailbox::channel();
        let publisher_slot = Arc::new(Mutex::new(Some(publisher)));
        let thread_publisher = Arc::clone(&publisher_slot);
        let thread_mailbox = Arc::clone(&mailbox);
        let thread_repaint = Arc::clone(&repaint);
        let actor_thread = thread::Builder::new()
            .name("oxide-language-coordinator".to_owned())
            .spawn(move || {
                let publisher = thread_publisher
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("coordinator owns the snapshot publisher");
                run_native_coordinator(
                    Arc::clone(&thread_mailbox),
                    wake,
                    publisher,
                    thread_repaint,
                );
                thread_mailbox.close();
            });

        let actor_thread = match actor_thread {
            Ok(handle) => Some(handle),
            Err(_) => {
                if let Some(publisher) = publisher_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    publish_unavailable(&publisher, &repaint);
                }
                mailbox.close();
                None
            }
        };
        Self::from_parts(mailbox, snapshots, actor_thread)
    }

    pub(super) fn from_parts(
        mailbox: Arc<CommandMailbox>,
        snapshots: SnapshotSubscriber,
        actor_thread: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            mailbox,
            snapshots,
            actor_thread,
        }
    }

    pub(crate) fn load_snapshot(&self) -> Arc<LanguageSnapshot> {
        self.snapshots.load_stable()
    }

    pub(crate) fn submit_document(
        &self,
        desired: Option<DesiredDocument>,
    ) -> Result<(), LanguageClosed> {
        self.mailbox.submit_document(desired)
    }

    pub(crate) fn submit_caret(&self, command: CaretCommand) -> Result<(), LanguageClosed> {
        self.mailbox.submit_caret(command)
    }

    pub(crate) fn request_definition(
        &self,
        intent: DefinitionIntent,
    ) -> Result<(), LanguageClosed> {
        self.mailbox.request_definition(intent)
    }

    pub(crate) fn acknowledge_items(
        &self,
        batch: AcknowledgementBatch,
    ) -> Result<(), LanguageClosed> {
        self.mailbox.acknowledge_items(batch)
    }

    pub(crate) fn request_shutdown(&self) {
        self.mailbox.request_shutdown();
    }
}

impl Drop for LanguageCoordinator {
    fn drop(&mut self) {
        self.request_shutdown();
        let _ = self.actor_thread.take();
    }
}

pub(super) fn initial_snapshot_channel() -> (SnapshotPublisher, SnapshotSubscriber) {
    snapshot_channel(LanguageSnapshot::bounded(LanguageSnapshotDraft {
        revision: SnapshotRevision::from_raw(1).expect("initial snapshot revision"),
        process_generation: None,
        status: LanguageStatus::Starting,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AdapterFacadeError;

pub(super) trait ProcessAdapterFacade {
    fn poll_event(&mut self) -> Option<ActorEvent>;

    fn execute_effect(
        &mut self,
        effect: ActorEffect,
    ) -> Result<Option<ActorEvent>, AdapterFacadeError>;

    fn acknowledge_finalization(
        &mut self,
        generation: ProcessGeneration,
    ) -> Result<(), AdapterFacadeError>;

    fn request_shutdown(&mut self);
}

impl ProcessAdapterFacade for LanguageProcessAdapter {
    fn poll_event(&mut self) -> Option<ActorEvent> {
        LanguageProcessAdapter::poll_event(self)
    }

    fn execute_effect(
        &mut self,
        effect: ActorEffect,
    ) -> Result<Option<ActorEvent>, AdapterFacadeError> {
        LanguageProcessAdapter::execute_effect(self, effect).map_err(|_| AdapterFacadeError)
    }

    fn acknowledge_finalization(
        &mut self,
        generation: ProcessGeneration,
    ) -> Result<(), AdapterFacadeError> {
        LanguageProcessAdapter::acknowledge_finalization(self, generation)
            .map_err(|_| AdapterFacadeError)
    }

    fn request_shutdown(&mut self) {
        LanguageProcessAdapter::request_shutdown(self);
    }
}

pub(super) struct EpochResult {
    pub(super) next_deadline: Option<ManualTick>,
    pub(super) stopped: bool,
    adapter_drain_saturated: bool,
}

pub(super) struct CoordinatorCore<A> {
    actor: LanguageActor,
    adapter: A,
    snapshots: SnapshotPublisher,
    repaint: Arc<dyn Fn() + Send + Sync>,
    retained_caret: Option<Arc<CaretCommand>>,
    applied_caret: Option<AppliedCaret>,
    pending_definition: Option<Arc<DefinitionIntent>>,
    shutdown_forwarded: bool,
    cleanup_quarantined: bool,
    stopped: bool,
    adapter_shutdown: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AppliedCaret {
    process_generation: ProcessGeneration,
    lsp_version: LspVersion,
    command: CaretCommand,
}

const ADAPTER_EVENTS_PER_EPOCH: usize = READER_INBOX_ITEMS + 5;
const INTERNAL_EVENT_LIMIT: usize = 256;
const DRIVE_LIMIT: usize = 256;

impl<A: ProcessAdapterFacade> CoordinatorCore<A> {
    pub(super) fn new(
        adapter: A,
        snapshots: SnapshotPublisher,
        repaint: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let mut actor = LanguageActor::new(ActorSeeds {
            next_snapshot_revision: 2,
            ..ActorSeeds::default()
        })
        .expect("coordinator actor seeds are valid");
        let start_effects = actor.reduce(ActorEvent::Start {
            now: ManualTick::from_raw(0),
        });
        debug_assert!(start_effects.is_empty());
        Self {
            actor,
            adapter,
            snapshots,
            repaint,
            retained_caret: None,
            applied_caret: None,
            pending_definition: None,
            shutdown_forwarded: false,
            cleanup_quarantined: false,
            stopped: false,
            adapter_shutdown: false,
        }
    }

    pub(super) fn run_epoch(&mut self, now: ManualTick, mailbox: &CommandMailbox) -> EpochResult {
        if self.stopped {
            mailbox.close();
            return EpochResult {
                next_deadline: None,
                stopped: true,
                adapter_drain_saturated: false,
            };
        }

        let mut adapter_events = 0usize;
        if let Some(event) = self.adapter.poll_event() {
            adapter_events += 1;
            self.handle_event(event);
            self.replay_caret_and_definition();
        }

        if mailbox.shutdown_requested() && !self.shutdown_forwarded {
            self.shutdown_forwarded = true;
            self.handle_event(ActorEvent::ShutdownRequested);
        }

        if let Some(command) = mailbox.take_document() {
            let desired = match command.as_ref() {
                DocumentCommand::Set(document) => Some(DurableDesiredDocument {
                    stamp: document.stamp,
                    source: Arc::clone(&document.text),
                }),
                DocumentCommand::Close => None,
            };
            self.handle_event(ActorEvent::DesiredDocumentChanged { desired });
        }

        if let Some(caret) = mailbox.take_caret() {
            if self.retained_caret.as_deref() != Some(caret.as_ref()) {
                self.applied_caret = None;
            }
            self.retained_caret = Some(caret);
        }
        if let Some(definition) = mailbox.take_definition() {
            self.pending_definition = Some(definition);
        }
        self.replay_caret_and_definition();

        if let Some(batch) = mailbox.take_acknowledgement() {
            self.handle_event(ActorEvent::SnapshotItemsAcknowledged {
                process_generation: batch.process_generation,
                observed_revision: batch.observed_revision,
                definition: batch.definition,
                notices: Arc::clone(&batch.notices),
            });
        }

        while !self.stopped && adapter_events < ADAPTER_EVENTS_PER_EPOCH {
            let Some(event) = self.adapter.poll_event() else {
                break;
            };
            adapter_events += 1;
            self.handle_event(event);
            self.replay_caret_and_definition();
        }

        if !self.stopped {
            self.handle_event(ActorEvent::Tick { now });
            self.replay_caret_and_definition();
            self.drive_to_quiescence();
        }

        if self.cleanup_quarantined
            || (self.actor.is_terminal() && self.actor.cleanup_pending().is_none())
        {
            self.stop_adapter();
            self.stopped = true;
        }
        if self.stopped {
            mailbox.close();
        }

        EpochResult {
            next_deadline: (!self.stopped)
                .then(|| self.actor.next_deadline())
                .flatten(),
            stopped: self.stopped,
            adapter_drain_saturated: adapter_events == ADAPTER_EVENTS_PER_EPOCH,
        }
    }

    fn handle_event(&mut self, event: ActorEvent) {
        if self.stopped {
            return;
        }
        let mut events = VecDeque::from([event]);
        let mut handled = 0usize;
        while let Some(event) = events.pop_front() {
            handled += 1;
            if handled > INTERNAL_EVENT_LIMIT {
                self.fail_runtime();
                return;
            }
            let finalized = match &event {
                ActorEvent::FinalizedGeneration { generation, .. } => Some(*generation),
                _ => None,
            };
            let cleanup_failed = matches!(&event, ActorEvent::CleanupFailed { .. });
            let cleanup_before = self.actor.cleanup_pending();
            let effects = self.actor.reduce(event);

            if let Some(generation) = finalized
                && cleanup_before == Some(generation)
                && self.actor.cleanup_pending() != Some(generation)
                && self.adapter.acknowledge_finalization(generation).is_err()
            {
                self.fail_runtime();
                return;
            }
            if cleanup_failed {
                self.cleanup_quarantined = true;
            }

            for effect in effects {
                if self.stopped {
                    return;
                }
                match effect {
                    ActorEffect::PublishSnapshot { snapshot } => {
                        if self.snapshots.publish(*snapshot).is_err() {
                            self.fail_runtime();
                            return;
                        }
                        (self.repaint)();
                    }
                    lifecycle => match self.adapter.execute_effect(lifecycle) {
                        Ok(Some(immediate)) => events.push_back(immediate),
                        Ok(None) => {}
                        Err(AdapterFacadeError) => {
                            self.fail_runtime();
                            return;
                        }
                    },
                }
            }
        }
    }

    fn replay_caret_and_definition(&mut self) {
        if self.stopped {
            return;
        }
        let Some(written) = self.actor.written_fence().cloned() else {
            return;
        };
        if self
            .pending_definition
            .as_ref()
            .is_some_and(|definition| definition.process_generation < written.generation)
        {
            self.pending_definition = None;
        }
        let Some(caret) = self.retained_caret.clone() else {
            return;
        };
        if caret_process_generation(&caret) != written.generation
            || caret_stamp(&caret) != written.stamp
        {
            return;
        }
        let application = AppliedCaret {
            process_generation: written.generation,
            lsp_version: written.lsp_version,
            command: caret.as_ref().clone(),
        };
        if self.applied_caret.as_ref() != Some(&application) {
            let fence = UiDocumentFence {
                process_generation: written.generation,
                stamp: written.stamp,
                lsp_version: Some(written.lsp_version),
            };
            let event = match caret.as_ref() {
                CaretCommand::Current(intent) => ActorEvent::CaretChanged {
                    fence,
                    caret_generation: intent.generation,
                },
                CaretCommand::Exhausted { .. } => ActorEvent::CaretGenerationExhausted { fence },
            };
            self.handle_event(event);
            if self.stopped {
                return;
            }
            self.applied_caret = Some(application.clone());
        }

        let Some(definition) = self.pending_definition.clone() else {
            return;
        };
        let matches_caret = matches!(caret.as_ref(), CaretCommand::Current(intent)
            if intent.process_generation == definition.process_generation
                && intent.stamp == definition.stamp
                && intent.generation == definition.caret_generation
                && intent.primary_character == Some(definition.caret_character));
        if !matches_caret {
            return;
        }
        if self.applied_caret.as_ref() != Some(&application)
            || definition.process_generation != written.generation
            || definition.stamp != written.stamp
        {
            return;
        }
        self.pending_definition = None;
        self.handle_event(ActorEvent::DefinitionRequested {
            fence: UiDocumentFence {
                process_generation: written.generation,
                stamp: written.stamp,
                lsp_version: Some(written.lsp_version),
            },
            caret_generation: definition.caret_generation,
            navigation_generation: definition.navigation_generation,
            primary_character: definition.caret_character,
        });
    }

    fn drive_to_quiescence(&mut self) {
        for _ in 0..DRIVE_LIMIT {
            if self.stopped {
                return;
            }
            let effects = self.actor.reduce(ActorEvent::Drive);
            if effects.is_empty() {
                return;
            }
            for effect in effects {
                if self.stopped {
                    return;
                }
                match effect {
                    ActorEffect::PublishSnapshot { snapshot } => {
                        if self.snapshots.publish(*snapshot).is_err() {
                            self.fail_runtime();
                            return;
                        }
                        (self.repaint)();
                    }
                    lifecycle => match self.adapter.execute_effect(lifecycle) {
                        Ok(Some(event)) => self.handle_event(event),
                        Ok(None) => {}
                        Err(AdapterFacadeError) => {
                            self.fail_runtime();
                            return;
                        }
                    },
                }
            }
        }
        self.fail_runtime();
    }

    fn fail_runtime(&mut self) {
        if self.stopped {
            return;
        }
        let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::terminal(),
            process_generation: None,
            status: LanguageStatus::Disabled,
            desired_document: None,
            written_document: None,
            diagnostics: None,
            syntax: None,
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            writer: WriterState::Closed,
        });
        if self.snapshots.publish(snapshot).is_ok() {
            (self.repaint)();
        }
        self.stop_adapter();
        self.stopped = true;
    }

    fn stop_adapter(&mut self) {
        if !self.adapter_shutdown {
            self.adapter.request_shutdown();
            self.adapter_shutdown = true;
        }
    }
}

fn caret_stamp(command: &CaretCommand) -> DocumentStamp {
    match command {
        CaretCommand::Current(intent) => intent.stamp,
        CaretCommand::Exhausted { stamp, .. } => *stamp,
    }
}

fn caret_process_generation(command: &CaretCommand) -> ProcessGeneration {
    match command {
        CaretCommand::Current(intent) => intent.process_generation,
        CaretCommand::Exhausted {
            process_generation, ..
        } => *process_generation,
    }
}

fn run_native_coordinator(
    mailbox: Arc<CommandMailbox>,
    wake: mpsc::Receiver<()>,
    publisher: SnapshotPublisher,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let config = match LanguageProcessConfig::sibling() {
        Ok(config) => config,
        Err(_) => {
            publish_unavailable(&publisher, &repaint);
            return;
        }
    };
    let adapter = match LanguageProcessAdapter::start_with_wake(config, mailbox.wake_sender()) {
        Ok(adapter) => adapter,
        Err(_) => {
            publish_unavailable(&publisher, &repaint);
            return;
        }
    };
    let mut core = CoordinatorCore::new(adapter, publisher, repaint);
    let epoch = Instant::now();
    loop {
        while wake.try_recv().is_ok() {}
        let now = elapsed_tick(epoch);
        let outcome = core.run_epoch(now, &mailbox);
        if outcome.stopped {
            return;
        }
        if outcome.adapter_drain_saturated
            || mailbox.has_pending_slots()
            || (mailbox.shutdown_requested() && !core.shutdown_forwarded)
        {
            continue;
        }

        let wait_now = elapsed_tick(epoch);
        match outcome.next_deadline {
            Some(deadline) if deadline <= wait_now => continue,
            Some(deadline) => {
                let timeout = duration_until(deadline, wait_now)
                    .expect("future deadline has a positive wait duration");
                let _ = wake.recv_timeout(timeout);
            }
            None => {
                let _ = wake.recv();
            }
        }
    }
}

fn elapsed_tick(epoch: Instant) -> ManualTick {
    let elapsed = epoch.elapsed().as_millis();
    ManualTick::from_raw(u64::try_from(elapsed).unwrap_or(u64::MAX))
}

pub(super) fn duration_until(deadline: ManualTick, now: ManualTick) -> Option<Duration> {
    deadline
        .get()
        .checked_sub(now.get())
        .filter(|remaining| *remaining > 0)
        .map(Duration::from_millis)
}

pub(super) fn publish_unavailable(
    publisher: &SnapshotPublisher,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
        revision: SnapshotRevision::from_raw(2).expect("startup failure revision"),
        process_generation: None,
        status: LanguageStatus::Unavailable,
        desired_document: None,
        written_document: None,
        diagnostics: None,
        syntax: None,
        definition: None,
        notices: Vec::new(),
        stderr_tail: None,
        writer: WriterState::Closed,
    });
    if publisher.publish(snapshot).is_ok() {
        repaint();
    }
}
