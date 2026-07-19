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
    #[cfg(windows)]
    let canonical_parent = fs::canonicalize(parent)?;
    #[cfg(windows)]
    let parent = canonical_parent.as_path();
    #[cfg(windows)]
    let canonical_target = parent.join(path.file_name().expect("file name was validated"));
    #[cfg(windows)]
    let target = canonical_target.as_path();
    #[cfg(not(windows))]
    let target = path;
    #[cfg(windows)]
    let commit_mode = windows_commit_mode(target)?;
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
    #[cfg(windows)]
    commit_temp(pending.path(), target, commit_mode)?;
    #[cfg(not(windows))]
    commit_temp(pending.path(), target)?;
    pending.committed = true;
    Ok(())
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsCommitMode {
    CreateNew,
    ReplaceExisting,
}

#[cfg(windows)]
fn windows_commit_mode(target: &Path) -> io::Result<WindowsCommitMode> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(WindowsCommitMode::ReplaceExisting),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "save target is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(WindowsCommitMode::CreateNew),
        Err(error) => Err(error),
    }
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
fn commit_temp(temporary: &Path, target: &Path, commit_mode: WindowsCommitMode) -> io::Result<()> {
    match commit_mode {
        WindowsCommitMode::CreateNew => move_new_file(temporary, target),
        WindowsCommitMode::ReplaceExisting => replace_existing_file(temporary, target),
    }
}

#[cfg(windows)]
fn move_new_file(temporary: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let temporary = windows_path_utf16(temporary)?;
    let target = windows_path_utf16(target)?;
    // SAFETY: both paths are NUL-terminated UTF-16 buffers that remain alive
    // for the duration of the call.
    if unsafe { MoveFileExW(temporary.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn replace_existing_file(temporary: &Path, target: &Path) -> io::Result<()> {
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let mut stage = create_sibling_hard_link_stage(parent, target)?;
    let (backup_path, backup_file) = reserve_sibling_replace_backup(parent)?;
    drop(backup_file);
    let mut backup = PendingTemp::new(backup_path);
    let stage_wide = windows_path_utf16(stage.path())?;
    let temporary_wide = windows_path_utf16(temporary)?;
    let backup_wide = windows_path_utf16(backup.path())?;
    // Merge the destination metadata into the completed temporary file while
    // the public target name still refers to the old file. No ignore flags are
    // used: failing to merge attributes, streams, or ACLs must fail the save.
    // The owned backup name makes every documented partial-failure location
    // known so cleanup cannot strand an unspecified old-content hard link.
    // SAFETY: all paths are live, NUL-terminated UTF-16 buffers, and the
    // reserved pointer parameters are null.
    if unsafe {
        ReplaceFileW(
            stage_wide.as_ptr(),
            temporary_wide.as_ptr(),
            backup_wide.as_ptr(),
            0,
            ptr::null(),
            ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    backup.remove()?;
    publish_replacement_stage(stage.path(), target)?;
    stage.committed = true;
    Ok(())
}

#[cfg(windows)]
fn reserve_sibling_replace_backup(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_ATTEMPTS {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".oxide-save-{}-{sequence}.backup",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique replacement backup",
    ))
}

#[cfg(windows)]
fn create_sibling_hard_link_stage(parent: &Path, target: &Path) -> io::Result<PendingTemp> {
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::CreateHardLinkW;

    // A volume without hard-link support fails here before either the public
    // target or completed temporary file is changed. Falling back to a direct
    // ReplaceFileW would reintroduce the public-name gap this stage prevents.
    let target_wide = windows_path_utf16(target)?;
    for _ in 0..TEMP_ATTEMPTS {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".oxide-save-{}-{sequence}.stage",
            std::process::id()
        ));
        let path_wide = windows_path_utf16(&path)?;
        // SAFETY: both paths are live, NUL-terminated UTF-16 buffers, and the
        // reserved security-attributes parameter is null as required.
        if unsafe { CreateHardLinkW(path_wide.as_ptr(), target_wide.as_ptr(), ptr::null()) } != 0 {
            return Ok(PendingTemp::new(path));
        }
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(code)
                if code == ERROR_ALREADY_EXISTS as i32 || code == ERROR_FILE_EXISTS as i32
        ) {
            continue;
        }
        return Err(error);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique replacement stage",
    ))
}

#[cfg(windows)]
fn publish_replacement_stage(stage: &Path, target: &Path) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileRenameInfoEx, SetFileInformationByHandle,
    };

    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;

    let target_name = windows_rename_target_utf16(target)?;
    let file_name_bytes = (target_name.len() - 1)
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "save path is too long"))?;
    let file_name_length = u32::try_from(file_name_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "save path is too long"))?;
    let buffer_bytes = windows_rename_info_buffer_bytes(file_name_bytes)?;
    let buffer_size = u32::try_from(buffer_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "save path is too long"))?;
    let mut buffer = vec![0_usize; buffer_bytes.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: the usize-backed buffer is suitably aligned and large enough for
    // the fixed header plus the full target path and its trailing NUL.
    unsafe {
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).RootDirectory = ptr::null_mut();
        (*info).FileNameLength = file_name_length;
        ptr::copy_nonoverlapping(
            target_name.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            target_name.len(),
        );
    }

    let stage_file = OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(stage)?;
    // SAFETY: stage_file owns a valid handle with DELETE access, and info
    // points to a live FILE_RENAME_INFO buffer of buffer_size bytes.
    if unsafe {
        SetFileInformationByHandle(
            stage_file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast(),
            buffer_size,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_rename_info_buffer_bytes(file_name_bytes: usize) -> io::Result<usize> {
    use std::mem::size_of;

    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    size_of::<FILE_RENAME_INFO>()
        .checked_add(file_name_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "save path is too long"))
}

#[cfg(windows)]
fn windows_rename_target_utf16(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    const BACKSLASH: u16 = b'\\' as u16;
    const VERBATIM_PREFIX: [u16; 4] = [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows rename target is not absolute",
        ));
    }
    let encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        ));
    }

    let is_verbatim_unc = encoded.starts_with(&VERBATIM_PREFIX)
        && encoded.len() >= 8
        && matches!(encoded[4], value if value == b'U' as u16 || value == b'u' as u16)
        && matches!(encoded[5], value if value == b'N' as u16 || value == b'n' as u16)
        && matches!(encoded[6], value if value == b'C' as u16 || value == b'c' as u16)
        && encoded[7] == BACKSLASH;
    let is_verbatim_drive = encoded.starts_with(&VERBATIM_PREFIX)
        && encoded.len() >= 7
        && matches!(encoded[4], value if (b'A' as u16..=b'Z' as u16).contains(&value) || (b'a' as u16..=b'z' as u16).contains(&value))
        && encoded[5] == b':' as u16
        && encoded[6] == BACKSLASH;
    let mut normalized = if is_verbatim_unc {
        let mut normalized = Vec::with_capacity(encoded.len() - 6);
        normalized.extend_from_slice(&[BACKSLASH, BACKSLASH]);
        normalized.extend_from_slice(&encoded[8..]);
        normalized
    } else if is_verbatim_drive {
        encoded[4..].to_vec()
    } else {
        encoded
    };
    normalized.push(0);
    Ok(normalized)
}

#[cfg(windows)]
fn windows_path_utf16(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows save path is not absolute",
        ));
    }
    let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        ));
    }
    encoded.push(0);
    Ok(encoded)
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

    #[cfg(windows)]
    fn remove(&mut self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => {
                self.committed = true;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.committed = true;
                Ok(())
            }
            Err(error) => Err(error),
        }
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

#[cfg(all(test, windows))]
mod tests {
    use std::mem::{offset_of, size_of};

    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    use super::windows_rename_info_buffer_bytes;

    #[test]
    fn rename_information_buffer_meets_the_documented_minimum() {
        let file_name_bytes = 42;
        let buffer_bytes = windows_rename_info_buffer_bytes(file_name_bytes).unwrap();

        assert_eq!(
            buffer_bytes,
            size_of::<FILE_RENAME_INFO>() + file_name_bytes
        );
        assert!(
            buffer_bytes
                >= offset_of!(FILE_RENAME_INFO, FileName) + file_name_bytes + size_of::<u16>()
        );
    }
}
