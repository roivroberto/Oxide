use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;
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

    #[cfg(windows)]
    fn new_relative() -> (Self, PathBuf) {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let relative = PathBuf::from(format!(
            ".oxide-relative-file-dialog-test-{}-{sequence}",
            std::process::id()
        ));
        let absolute = std::env::current_dir().unwrap().join(&relative);
        fs::create_dir(&absolute).expect("create relative test directory");
        (Self(absolute), relative)
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

#[cfg(windows)]
fn windows_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().chain([0]).collect()
}

#[cfg(windows)]
fn file_dacl(path: &Path) -> Vec<usize> {
    use std::ptr;

    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetFileSecurityW};

    let path = windows_path(path);
    let mut required = 0_u32;
    // SAFETY: the null buffer and zero length request only the required size.
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            &mut required,
        );
    }
    assert!(required > 0, "query destination DACL size");
    let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
    let mut descriptor = vec![0_usize; words];
    // SAFETY: descriptor has the reported capacity and path is NUL-terminated.
    assert_ne!(
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        },
        0
    );
    descriptor
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

    let result = write_file_atomically(&target, b"print 1;\n");
    #[cfg(windows)]
    assert_eq!(result, Err(FileFailureKind::AlreadyExists));
    #[cfg(not(windows))]
    assert!(result.is_err());
    assert_eq!(
        fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        [target.file_name().unwrap().to_os_string()]
    );
}

#[test]
fn atomic_write_replaces_an_existing_file_without_exposing_partial_bytes() {
    let directory = TempDir::new();
    let target = directory.path().join("existing.ox");
    fs::write(&target, b"old contents").unwrap();

    assert_eq!(write_file_atomically(&target, b"new contents\n"), Ok(()));
    assert_eq!(fs::read(&target).unwrap(), b"new contents\n");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn atomic_write_preserves_an_existing_named_stream() {
    let directory = TempDir::new();
    let target = directory.path().join("streams.ox");
    fs::write(&target, b"old contents").unwrap();
    let mut stream_name = target.as_os_str().to_os_string();
    stream_name.push(":oxide-metadata");
    let stream = PathBuf::from(stream_name);
    fs::write(&stream, b"preserved stream").unwrap();

    assert_eq!(write_file_atomically(&target, b"new contents\n"), Ok(()));
    assert_eq!(fs::read(&target).unwrap(), b"new contents\n");
    assert_eq!(fs::read(&stream).unwrap(), b"preserved stream");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn atomic_write_preserves_an_existing_hidden_attribute() {
    use std::os::windows::fs::MetadataExt;

    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

    let directory = TempDir::new();
    let target = directory.path().join("hidden.ox");
    fs::write(&target, b"old contents").unwrap();
    let target_wide = windows_path(&target);
    let attributes = fs::metadata(&target).unwrap().file_attributes();
    // SAFETY: target_wide is a live, NUL-terminated UTF-16 path.
    assert_ne!(
        unsafe { SetFileAttributesW(target_wide.as_ptr(), attributes | FILE_ATTRIBUTE_HIDDEN) },
        0
    );

    assert_eq!(write_file_atomically(&target, b"new contents\n"), Ok(()));
    assert_ne!(
        fs::metadata(&target).unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN,
        0
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn atomic_write_preserves_a_protected_destination_dacl() {
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl,
        PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED, SetFileSecurityW,
        SetSecurityDescriptorControl,
    };

    let directory = TempDir::new();
    let target = directory.path().join("protected-dacl.ox");
    fs::write(&target, b"old contents").unwrap();
    let target_wide = windows_path(&target);
    let mut descriptor = file_dacl(&target);
    // SAFETY: descriptor contains a valid mutable security descriptor returned
    // by GetFileSecurityW.
    assert_ne!(
        unsafe {
            SetSecurityDescriptorControl(
                descriptor.as_mut_ptr().cast(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
        },
        0
    );
    // SAFETY: both the path and security descriptor remain valid for the call.
    assert_ne!(
        unsafe {
            SetFileSecurityW(
                target_wide.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
            )
        },
        0
    );

    assert_eq!(write_file_atomically(&target, b"new contents\n"), Ok(()));
    let mut descriptor = file_dacl(&target);
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor contains a valid security descriptor and the output
    // pointers refer to initialized writable values.
    assert_ne!(
        unsafe {
            GetSecurityDescriptorControl(
                descriptor.as_mut_ptr().cast(),
                &mut control,
                &mut revision,
            )
        },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn atomic_new_file_does_not_overwrite_a_destination_created_during_save() {
    let directory = TempDir::new();
    let target = directory.path().join("raced.ox");
    let watcher_directory = directory.path().to_path_buf();
    let watcher_target = target.clone();
    let ready = Arc::new(Barrier::new(2));
    let watcher_ready = Arc::clone(&ready);
    let watcher = thread::spawn(move || {
        watcher_ready.wait();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let temporary_exists = fs::read_dir(&watcher_directory)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".oxide-save-")
                });
            if temporary_exists {
                fs::write(&watcher_target, b"concurrent contents").unwrap();
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "save temporary file did not appear"
            );
            thread::yield_now();
        }
    });

    ready.wait();
    let contents = vec![b'X'; 32 * 1024 * 1024];
    let result = write_file_atomically(&target, &contents);
    watcher.join().expect("destination watcher");

    assert!(
        result.is_err(),
        "the raced destination must not be replaced"
    );
    assert_eq!(fs::read(&target).unwrap(), b"concurrent contents");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn concurrent_readers_observe_only_complete_atomic_replacements() {
    let directory = TempDir::new();
    let target = directory.path().join("observed.ox");
    let old = Arc::new(vec![b'A'; 16 * 1024]);
    let new = Arc::new(vec![b'B'; 16 * 1024]);
    fs::write(&target, old.as_slice()).unwrap();

    let keep_reading = Arc::new(AtomicBool::new(true));
    let ready = Arc::new(Barrier::new(2));
    let reader_target = target.clone();
    let reader_old = Arc::clone(&old);
    let reader_new = Arc::clone(&new);
    let reader_keep_reading = Arc::clone(&keep_reading);
    let reader_ready = Arc::clone(&ready);
    let reader = thread::spawn(move || {
        reader_ready.wait();
        let mut observations = 0usize;
        loop {
            let observed = fs::read(&reader_target).expect("atomic target remains readable");
            assert!(
                observed.as_slice() == reader_old.as_slice()
                    || observed.as_slice() == reader_new.as_slice(),
                "reader observed a partial or mixed replacement"
            );
            observations += 1;
            if !reader_keep_reading.load(Ordering::Acquire) {
                return observations;
            }
        }
    });

    ready.wait();
    for replacement in 0..12 {
        let contents = if replacement % 2 == 0 { &new } else { &old };
        assert_eq!(write_file_atomically(&target, contents.as_slice()), Ok(()));
    }
    keep_reading.store(false, Ordering::Release);

    assert!(reader.join().expect("reader thread") > 0);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn atomic_write_replaces_a_unicode_path_with_spaces_without_temp_artifacts() {
    let directory = TempDir::new();
    let source_directory = directory.path().join("Mga Programa");
    fs::create_dir(&source_directory).unwrap();
    let target = source_directory.join("wika-語.ox");
    fs::write(&target, b"old contents").unwrap();

    assert_eq!(
        write_file_atomically(&target, b"print \"kumusta\";\n"),
        Ok(())
    );
    assert_eq!(fs::read(&target).unwrap(), b"print \"kumusta\";\n");
    assert_eq!(fs::read_dir(&source_directory).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn atomic_write_replaces_a_file_beyond_the_legacy_windows_path_limit() {
    use std::os::windows::ffi::OsStrExt;

    let directory = TempDir::new();
    let mut source_directory = directory.path().to_path_buf();
    let mut segment = 0_u32;
    while source_directory.as_os_str().encode_wide().count() <= 280 {
        source_directory.push(format!("source-segment-{segment:02}-abcdefghijklmnop"));
        fs::create_dir(&source_directory).unwrap();
        segment += 1;
    }
    let target = source_directory.join("program.ox");
    fs::write(&target, b"old contents").unwrap();

    assert_eq!(write_file_atomically(&target, b"print 42;\n"), Ok(()));
    assert_eq!(fs::read(&target).unwrap(), b"print 42;\n");
    assert_eq!(fs::read_dir(&source_directory).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn atomic_write_resolves_a_relative_target_before_replacing_it() {
    let (_directory, relative_directory) = TempDir::new_relative();
    let target = relative_directory.join("relative.ox");
    assert!(!target.is_absolute());
    fs::write(&target, b"old contents").unwrap();

    assert_eq!(write_file_atomically(&target, b"print 7;\n"), Ok(()));
    assert_eq!(fs::read(&target).unwrap(), b"print 7;\n");
    assert_eq!(fs::read_dir(&relative_directory).unwrap().count(), 1);
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
