use std::ffi::{OsStr, OsString};

use crate::WorkerSessionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    Visible,
    Worker(WorkerSessionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchError {
    Usage,
}

pub fn route_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<LaunchMode, LaunchError> {
    let mut arguments = arguments.into_iter();
    let Some(mode) = arguments.next() else {
        return Ok(LaunchMode::Visible);
    };
    if mode != OsStr::new("--worker")
        || arguments.next().as_deref() != Some(OsStr::new("--worker-session"))
    {
        return Err(LaunchError::Usage);
    }
    let session = arguments
        .next()
        .as_deref()
        .and_then(parse_session_id)
        .ok_or(LaunchError::Usage)?;
    if arguments.next().is_some() {
        return Err(LaunchError::Usage);
    }
    Ok(LaunchMode::Worker(WorkerSessionId(session)))
}

fn parse_session_id(value: &OsStr) -> Option<u64> {
    let value = value.to_str()?;
    if value.is_empty()
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let parsed = value.parse::<u64>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}
