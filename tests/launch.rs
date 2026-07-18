use std::ffi::OsString;

use oxide_ide::{LaunchError, LaunchMode, WorkerSessionId, route_arguments};

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn no_arguments_selects_the_visible_ide() {
    assert_eq!(route_arguments(args(&[])), Ok(LaunchMode::Visible));
}

#[test]
fn exact_worker_arguments_select_headless_worker_mode() {
    assert_eq!(
        route_arguments(args(&["--worker", "--worker-session", "42"])),
        Ok(LaunchMode::Worker(WorkerSessionId(42)))
    );
}

#[test]
fn malformed_or_extra_arguments_are_rejected() {
    for values in [
        vec!["--worker"],
        vec!["--worker", "--worker-session", "0"],
        vec!["--worker", "--worker-session", "01"],
        vec!["--worker", "--worker-session", "nope"],
        vec!["--worker", "--worker-session", "1", "extra"],
        vec!["--unknown"],
    ] {
        assert_eq!(route_arguments(args(&values)), Err(LaunchError::Usage));
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_rejected_without_lossy_conversion() {
    use std::os::unix::ffi::OsStringExt;

    assert_eq!(
        route_arguments(vec![OsString::from_vec(vec![0xff])]),
        Err(LaunchError::Usage)
    );
}
