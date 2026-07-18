use std::process::ExitCode;

use oxide_ide::{WorkerSessionId, run_worker};

fn main() -> ExitCode {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    match arguments.next().and_then(|value| value.into_string().ok()) {
        None => ExitCode::SUCCESS,
        Some(mode) if mode == "--worker" => {
            if arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .as_deref()
                != Some("--worker-session")
            {
                return ExitCode::from(64);
            }
            let Some(session) = arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .and_then(|value| parse_session_id(&value))
            else {
                return ExitCode::from(64);
            };
            if arguments.next().is_some() {
                return ExitCode::from(64);
            }
            match run_worker(
                std::io::stdin(),
                std::io::stdout(),
                WorkerSessionId(session),
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(70),
            }
        }
        Some(_) => ExitCode::from(64),
    }
}

fn parse_session_id(value: &str) -> Option<u64> {
    if value.is_empty()
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let parsed = value.parse::<u64>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}
