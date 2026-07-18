use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

use crate::{FileFailureKind, FileModelEvent, FileOperationId, UnsavedChoice};

pub const SOURCE_EXTENSION: &str = "ox";
pub const SOURCE_FILTER_NAME: &str = "Oxide source";
const DEFAULT_FILE_NAME: &str = "untitled.ox";
const SAVE_LABEL: &str = "Save";
const DISCARD_LABEL: &str = "Discard";
const CANCEL_LABEL: &str = "Cancel";
const TEMP_ATTEMPTS: usize = 128;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveDialogHint {
    pub directory: Option<PathBuf>,
    pub file_name: String,
}

pub fn save_dialog_hint(suggested_path: Option<&Path>) -> SaveDialogHint {
    let Some(path) = suggested_path else {
        return SaveDialogHint {
            directory: None,
            file_name: DEFAULT_FILE_NAME.to_string(),
        };
    };
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_FILE_NAME.to_string());
    SaveDialogHint {
        directory,
        file_name,
    }
}

pub fn show_open_dialog(parent: &eframe::Frame, operation_id: FileOperationId) -> FileModelEvent {
    let picked = catch_unwind(AssertUnwindSafe(|| {
        FileDialog::new()
            .set_parent(parent)
            .set_title("Open Oxide source")
            .add_filter(SOURCE_FILTER_NAME, &[SOURCE_EXTENSION])
            .pick_file()
    }))
    .ok()
    .flatten();
    map_open_dialog_result(operation_id, picked)
}

pub fn show_save_dialog(
    parent: &eframe::Frame,
    operation_id: FileOperationId,
    suggested_path: Option<&Path>,
) -> FileModelEvent {
    let hint = save_dialog_hint(suggested_path);
    let picked = catch_unwind(AssertUnwindSafe(|| {
        let mut dialog = FileDialog::new()
            .set_parent(parent)
            .set_title("Save Oxide source")
            .add_filter(SOURCE_FILTER_NAME, &[SOURCE_EXTENSION])
            .set_file_name(hint.file_name);
        if let Some(directory) = hint.directory {
            dialog = dialog.set_directory(directory);
        }
        dialog.save_file()
    }))
    .ok()
    .flatten();
    map_save_dialog_result(operation_id, picked)
}

pub fn show_unsaved_dialog(parent: &eframe::Frame, display_name: &str) -> UnsavedChoice {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        MessageDialog::new()
            .set_parent(parent)
            .set_level(MessageLevel::Warning)
            .set_title("Unsaved changes")
            .set_description(format!("Save changes to {display_name} before continuing?"))
            .set_buttons(MessageButtons::YesNoCancelCustom(
                SAVE_LABEL.to_string(),
                DISCARD_LABEL.to_string(),
                CANCEL_LABEL.to_string(),
            ))
            .show()
    }))
    .ok();
    map_unsaved_dialog_outcome(outcome)
}

pub fn map_open_dialog_result(
    operation_id: FileOperationId,
    path: Option<PathBuf>,
) -> FileModelEvent {
    FileModelEvent::OpenPicked { operation_id, path }
}

pub fn map_save_dialog_result(
    operation_id: FileOperationId,
    path: Option<PathBuf>,
) -> FileModelEvent {
    FileModelEvent::SavePicked { operation_id, path }
}

pub fn map_unsaved_dialog_outcome(result: Option<MessageDialogResult>) -> UnsavedChoice {
    result.map_or(UnsavedChoice::Cancel, map_unsaved_dialog_result)
}

pub fn map_unsaved_dialog_result(result: MessageDialogResult) -> UnsavedChoice {
    match result {
        MessageDialogResult::Custom(label) if label == SAVE_LABEL => UnsavedChoice::Save,
        MessageDialogResult::Custom(label) if label == DISCARD_LABEL => UnsavedChoice::Discard,
        MessageDialogResult::Yes => UnsavedChoice::Save,
        MessageDialogResult::No => UnsavedChoice::Discard,
        _ => UnsavedChoice::Cancel,
    }
}

#[derive(Clone, Debug)]
pub enum FileJob {
    Read {
        operation_id: FileOperationId,
        path: PathBuf,
        max_bytes: usize,
    },
    Write {
        operation_id: FileOperationId,
        path: PathBuf,
        contents: Arc<[u8]>,
    },
}

impl FileJob {
    pub(crate) fn failure_event(&self, failure: FileFailureKind) -> FileModelEvent {
        match self {
            Self::Read { operation_id, .. } => FileModelEvent::ReadFinished {
                operation_id: *operation_id,
                result: Err(failure),
            },
            Self::Write { operation_id, .. } => FileModelEvent::WriteFinished {
                operation_id: *operation_id,
                result: Err(failure),
            },
        }
    }

    pub(crate) fn matches_event(&self, event: &FileModelEvent) -> bool {
        matches!(
            (self, event),
            (
                Self::Read { operation_id: expected, .. },
                FileModelEvent::ReadFinished { operation_id: actual, .. }
            ) | (
                Self::Write { operation_id: expected, .. },
                FileModelEvent::WriteFinished { operation_id: actual, .. }
            ) if expected == actual
        )
    }

    fn execute(self) -> FileModelEvent {
        match self {
            Self::Read {
                operation_id,
                path,
                max_bytes,
            } => FileModelEvent::ReadFinished {
                operation_id,
                result: read_file_bounded(&path, max_bytes),
            },
            Self::Write {
                operation_id,
                path,
                contents,
            } => FileModelEvent::WriteFinished {
                operation_id,
                result: write_file_atomically(&path, &contents),
            },
        }
    }
}

#[derive(Debug)]
pub enum FileSubmitError {
    Busy(FileJob),
    Closed(FileJob),
}

impl FileSubmitError {
    pub fn into_job(self) -> FileJob {
        match self {
            Self::Busy(job) | Self::Closed(job) => job,
        }
    }

    pub fn into_failure_event(self, failure: FileFailureKind) -> FileModelEvent {
        self.into_job().failure_event(failure)
    }
}

pub struct FileExecutor {
    sender: mpsc::SyncSender<FileJob>,
    busy: Arc<AtomicBool>,
}

impl FileExecutor {
    pub fn spawn<W>(wake: W) -> io::Result<(Self, FileEventReceiver)>
    where
        W: Fn() + Send + Sync + 'static,
    {
        let (event_sender, event_receiver) = mpsc::sync_channel(1);
        let executor = Self::spawn_with_handlers(
            move |event| event_sender.send(event).map_err(|error| error.0),
            wake,
        )?;
        Ok((
            executor,
            FileEventReceiver {
                receiver: event_receiver,
            },
        ))
    }

    pub fn spawn_with_handlers<E, W>(enqueue: E, wake: W) -> io::Result<Self>
    where
        E: Fn(FileModelEvent) -> Result<(), FileModelEvent> + Send + Sync + 'static,
        W: Fn() + Send + Sync + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel::<FileJob>(1);
        let busy = Arc::new(AtomicBool::new(false));
        let worker_busy = Arc::clone(&busy);
        thread::Builder::new()
            .name("oxide-file-io".to_string())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let failure = job.failure_event(FileFailureKind::Other);
                    let event = catch_unwind(AssertUnwindSafe(|| job.execute())).unwrap_or(failure);
                    let enqueued = enqueue(event).is_ok();
                    worker_busy.store(false, Ordering::Release);
                    if enqueued {
                        wake();
                    }
                }
            })?;
        Ok(Self { sender, busy })
    }

    pub fn try_submit(&self, job: FileJob) -> Result<(), FileSubmitError> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(FileSubmitError::Busy(job));
        }
        match self.sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(job)) => {
                self.busy.store(false, Ordering::Release);
                Err(FileSubmitError::Busy(job))
            }
            Err(mpsc::TrySendError::Disconnected(job)) => {
                self.busy.store(false, Ordering::Release);
                Err(FileSubmitError::Closed(job))
            }
        }
    }
}

pub struct FileEventReceiver {
    receiver: mpsc::Receiver<FileModelEvent>,
}

impl FileEventReceiver {
    pub fn try_recv(&self) -> Result<FileModelEvent, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<FileModelEvent, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

pub fn read_file_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, FileFailureKind> {
    read_file_bounded_io(path, max_bytes).map_err(|error| classify_io_error(&error))
}

fn read_file_bounded_io(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let limit = u64::try_from(max_bytes)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file limit is too large"))?;
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source file exceeds the configured limit",
        ));
    }
    Ok(bytes)
}

pub fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<(), FileFailureKind> {
    write_file_atomically_io(path, contents).map_err(|error| classify_io_error(&error))
}

fn write_file_atomically_io(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no file name",
        ));
    }
    #[cfg(unix)]
    let existing_permissions = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Some(metadata.permissions()),
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let (temporary_path, mut temporary_file) = create_sibling_temp(parent)?;
    let mut pending = PendingTemp::new(temporary_path);
    temporary_file.write_all(contents)?;
    temporary_file.flush()?;
    #[cfg(unix)]
    if let Some(permissions) = existing_permissions {
        temporary_file.set_permissions(permissions)?;
    }
    temporary_file.sync_all()?;
    drop(temporary_file);
    commit_temp(pending.path(), path)?;
    pending.committed = true;
    Ok(())
}

fn create_sibling_temp(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_ATTEMPTS {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".oxide-save-");
        name.push(std::process::id().to_string());
        name.push("-");
        name.push(sequence.to_string());
        name.push(".tmp");
        let path = parent.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
}

#[cfg(not(windows))]
fn commit_temp(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temporary, target)
}

#[cfg(windows)]
fn commit_temp(temporary: &Path, target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(target) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "safe replacement of an existing Windows file is unavailable",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(temporary, target),
        Err(error) => Err(error),
    }
}

struct PendingTemp {
    path: PathBuf,
    committed: bool,
}

impl PendingTemp {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PendingTemp {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn classify_io_error(error: &io::Error) -> FileFailureKind {
    match error.kind() {
        io::ErrorKind::NotFound => FileFailureKind::NotFound,
        io::ErrorKind::PermissionDenied => FileFailureKind::PermissionDenied,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => FileFailureKind::InvalidData,
        io::ErrorKind::AlreadyExists => FileFailureKind::AlreadyExists,
        _ => FileFailureKind::Other,
    }
}
