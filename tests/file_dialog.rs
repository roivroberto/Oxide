use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::Duration;

use oxide_ide::file_dialog::{
    FileExecutor, FileJob, FileSubmitError, SOURCE_EXTENSION, SaveDialogHint,
    map_open_dialog_result, map_save_dialog_result, map_unsaved_dialog_outcome,
    map_unsaved_dialog_result, read_file_bounded, save_dialog_hint, write_file_atomically,
};
use oxide_ide::{FileFailureKind, FileModelEvent, FileOperationId, UnsavedChoice};
use rfd::MessageDialogResult;

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "oxide-file-dialog-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn operation(value: u64) -> FileOperationId {
    FileOperationId::from_raw(value).expect("nonzero operation id")
}

#[test]
fn bounded_read_accepts_the_limit_and_rejects_one_extra_byte() {
    let directory = TempDir::new();
    let exact = directory.path().join("exact.ox");
    let oversized = directory.path().join("oversized.ox");
    fs::write(&exact, b"1234").unwrap();
    fs::write(&oversized, b"12345").unwrap();

    assert_eq!(read_file_bounded(&exact, 4), Ok(b"1234".to_vec()));
    assert_eq!(
        read_file_bounded(&oversized, 4),
        Err(FileFailureKind::InvalidData)
    );
}

#[test]
fn atomic_write_creates_a_complete_new_file_without_temp_artifacts() {
    let directory = TempDir::new();
    let target = directory.path().join("new.ox");

    assert_eq!(
        write_file_atomically(&target, b"print \"hello\";\n"),
        Ok(())
    );
    assert_eq!(fs::read(&target).unwrap(), b"print \"hello\";\n");
    assert_eq!(
        fs::read_dir(directory.path()).unwrap().count(),
        1,
        "the committed target must be the only directory entry"
    );
}

#[test]
fn failed_atomic_commit_removes_the_pending_temp_file() {
    let directory = TempDir::new();
    let target = directory.path().join("target.ox");
    fs::create_dir(&target).unwrap();

    assert!(write_file_atomically(&target, b"print 1;\n").is_err());
    assert_eq!(
        fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        [target.file_name().unwrap().to_os_string()]
    );
}

#[cfg(not(windows))]
#[test]
fn atomic_write_replaces_an_existing_file_without_exposing_partial_bytes() {
    let directory = TempDir::new();
    let target = directory.path().join("existing.ox");
    fs::write(&target, b"old contents").unwrap();

    assert_eq!(write_file_atomically(&target, b"new contents\n"), Ok(()));
    assert_eq!(fs::read(&target).unwrap(), b"new contents\n");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn atomic_write_preserves_existing_file_permissions() {
    let directory = TempDir::new();
    let target = directory.path().join("permissions.ox");
    fs::write(&target, b"old contents").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();

    assert_eq!(write_file_atomically(&target, b"new contents\n"), Ok(()));
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[cfg(target_os = "linux")]
#[test]
fn atomic_write_preserves_a_non_utf8_native_path() {
    use std::os::unix::ffi::OsStringExt;

    let directory = TempDir::new();
    let name = OsString::from_vec(b"wika-\xff.ox".to_vec());
    let target = directory.path().join(name);

    assert_eq!(write_file_atomically(&target, b"print 1;\n"), Ok(()));
    assert_eq!(fs::read(target).unwrap(), b"print 1;\n");
}

#[test]
fn executor_preserves_operation_and_path_and_wakes_after_enqueue() {
    let directory = TempDir::new();
    let path = directory.path().join("source.ox");
    fs::write(&path, b"print 42;\n").unwrap();
    let operation_id = operation(7);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let order = Arc::new(Mutex::new(Vec::new()));
    let enqueue_order = Arc::clone(&order);
    let wake_order = Arc::clone(&order);
    let executor = FileExecutor::spawn_with_handlers(
        move |event| {
            enqueue_order.lock().unwrap().push("enqueue");
            result_tx.send(event).map_err(|error| error.0)
        },
        move || {
            wake_order.lock().unwrap().push("wake");
            wake_tx.send(()).unwrap();
        },
    )
    .unwrap();

    executor
        .try_submit(FileJob::Read {
            operation_id,
            path: path.clone(),
            max_bytes: 64,
        })
        .unwrap();

    assert_eq!(
        result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        FileModelEvent::ReadFinished {
            operation_id,
            result: Ok(b"print 42;\n".to_vec()),
        }
    );
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(*order.lock().unwrap(), ["enqueue", "wake"]);
}

#[test]
fn executor_reports_write_failure_with_the_matching_operation() {
    let directory = TempDir::new();
    let impossible_target = directory.path().join("missing").join("source.ox");
    let operation_id = operation(9);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let (executor, results) = FileExecutor::spawn(move || wake_tx.send(()).unwrap()).unwrap();

    executor
        .try_submit(FileJob::Write {
            operation_id,
            path: impossible_target,
            contents: Arc::from(&b"print 1;\n"[..]),
        })
        .unwrap();

    assert_eq!(
        results.recv_timeout(Duration::from_secs(2)).unwrap(),
        FileModelEvent::WriteFinished {
            operation_id,
            result: Err(FileFailureKind::NotFound),
        }
    );
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn rejected_submission_can_complete_the_exact_pending_file_operation() {
    let operation_id = operation(10);
    let rejected = FileSubmitError::Busy(FileJob::Read {
        operation_id,
        path: PathBuf::from("busy.ox"),
        max_bytes: 64,
    });

    assert_eq!(
        rejected.into_failure_event(FileFailureKind::Other),
        FileModelEvent::ReadFinished {
            operation_id,
            result: Err(FileFailureKind::Other),
        }
    );
}

#[test]
fn executor_rejects_a_second_job_while_the_first_event_is_being_delivered() {
    let directory = TempDir::new();
    let path = directory.path().join("source.ox");
    fs::write(&path, b"print 1;\n").unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let delivery_gate = Arc::new(Barrier::new(2));
    let worker_gate = Arc::clone(&delivery_gate);
    let executor = FileExecutor::spawn_with_handlers(
        move |event| {
            entered_tx.send(()).unwrap();
            worker_gate.wait();
            result_tx.send(event).map_err(|error| error.0)
        },
        move || wake_tx.send(()).unwrap(),
    )
    .unwrap();

    executor
        .try_submit(FileJob::Read {
            operation_id: operation(11),
            path: path.clone(),
            max_bytes: 64,
        })
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let rejected = executor
        .try_submit(FileJob::Read {
            operation_id: operation(12),
            path: path.clone(),
            max_bytes: 64,
        })
        .unwrap_err();
    assert!(matches!(rejected, FileSubmitError::Busy(_)));

    delivery_gate.wait();
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        FileModelEvent::ReadFinished {
            operation_id,
            result: Ok(_),
        } if operation_id == operation(11)
    ));
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    executor
        .try_submit(FileJob::Read {
            operation_id: operation(13),
            path,
            max_bytes: 64,
        })
        .unwrap();
}

#[test]
fn unsaved_prompt_mapping_is_fail_closed() {
    assert_eq!(
        map_unsaved_dialog_result(MessageDialogResult::Custom("Save".into())),
        UnsavedChoice::Save
    );
    assert_eq!(
        map_unsaved_dialog_result(MessageDialogResult::Custom("Discard".into())),
        UnsavedChoice::Discard
    );
    assert_eq!(
        map_unsaved_dialog_result(MessageDialogResult::Yes),
        UnsavedChoice::Save
    );
    assert_eq!(
        map_unsaved_dialog_result(MessageDialogResult::No),
        UnsavedChoice::Discard
    );
    for unexpected in [
        MessageDialogResult::Cancel,
        MessageDialogResult::Ok,
        MessageDialogResult::Custom("unexpected".into()),
    ] {
        assert_eq!(map_unsaved_dialog_result(unexpected), UnsavedChoice::Cancel);
    }
    assert_eq!(map_unsaved_dialog_outcome(None), UnsavedChoice::Cancel);
}

#[test]
fn picker_mapping_preserves_the_selected_path_and_correlates_cancellation() {
    let selected = PathBuf::from("Mga Programa").join("wika-語.ox");

    assert_eq!(
        map_open_dialog_result(operation(14), Some(selected.clone())),
        FileModelEvent::OpenPicked {
            operation_id: operation(14),
            path: Some(selected.clone()),
        }
    );
    assert_eq!(
        map_open_dialog_result(operation(15), None),
        FileModelEvent::OpenPicked {
            operation_id: operation(15),
            path: None,
        }
    );
    assert_eq!(
        map_save_dialog_result(operation(16), Some(selected.clone())),
        FileModelEvent::SavePicked {
            operation_id: operation(16),
            path: Some(selected),
        }
    );
    assert_eq!(
        map_save_dialog_result(operation(17), None),
        FileModelEvent::SavePicked {
            operation_id: operation(17),
            path: None,
        }
    );
}

#[test]
fn save_dialog_hint_uses_oxide_extension_without_rebuilding_the_path() {
    assert_eq!(SOURCE_EXTENSION, "ox");
    assert_eq!(
        save_dialog_hint(None),
        SaveDialogHint {
            directory: None,
            file_name: "untitled.ox".to_string(),
        }
    );

    let suggested = PathBuf::from("Mga Programa").join("wika-語.ox");
    let hint = save_dialog_hint(Some(&suggested));
    assert_eq!(hint.file_name, "wika-語.ox");
    assert_eq!(hint.directory, suggested.parent().map(Path::to_path_buf));
}
