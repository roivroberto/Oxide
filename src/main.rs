#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    io::{self, Write},
    process::ExitCode,
};

use lsp_server::Connection;
use oxide_ide::{APP_NAME, LANGUAGE_WORKER_ARGUMENT, run_visible};
use rlox_lsp::{ServerOutcome, run_connection};

const MAX_STARTUP_DETAIL_CHARS: usize = 2_048;
const MAX_WORKER_FAILURE_CHARS: usize = 2_048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupMode {
    Visible,
    LanguageWorker,
}

fn startup_mode(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> StartupMode {
    let arguments: Vec<_> = arguments.into_iter().collect();
    if arguments.len() == 1 && arguments[0] == LANGUAGE_WORKER_ARGUMENT {
        StartupMode::LanguageWorker
    } else {
        StartupMode::Visible
    }
}

fn main() -> ExitCode {
    match startup_mode(std::env::args_os().skip(1)) {
        StartupMode::Visible => run_visible_mode(),
        StartupMode::LanguageWorker => run_language_worker(),
    }
}

fn run_visible_mode() -> ExitCode {
    match run_visible() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_visible_startup_error(&error);
            ExitCode::from(70)
        }
    }
}

fn run_language_worker() -> ExitCode {
    let (connection, io_threads) = Connection::stdio();
    match run_connection(connection) {
        Ok(ServerOutcome::CleanExit) => match io_threads.join() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail_language_worker(format!("LSP transport failed: {error}")),
        },
        Ok(ServerOutcome::ExitWithoutShutdown) => {
            let join_error = io_threads.join().err();
            fail_language_worker(match join_error {
                Some(error) => {
                    format!("client exited without shutdown; transport failed: {error}")
                }
                None => "client exited without shutdown".to_owned(),
            })
        }
        Ok(ServerOutcome::ChannelClosed) => {
            let join_error = io_threads.join().err();
            fail_language_worker(match join_error {
                Some(error) => format!("LSP input closed unexpectedly: {error}"),
                None => "LSP input closed without exit".to_owned(),
            })
        }
        Ok(ServerOutcome::OutputClosed) => fail_language_worker("LSP output closed unexpectedly"),
        Err(error) => {
            let message = match flush_pending_worker_output() {
                Ok(()) => error.to_string(),
                Err(flush_error) => format!("{error}; LSP output flush failed: {flush_error}"),
            };
            fail_language_worker(message)
        }
    }
}

fn flush_pending_worker_output() -> io::Result<()> {
    io::stdout().lock().flush()
}

fn fail_language_worker(message: impl AsRef<str>) -> ExitCode {
    eprintln!("rlox-lsp: {}", bounded_worker_failure(message.as_ref()));
    ExitCode::FAILURE
}

fn bounded_worker_failure(message: &str) -> String {
    let mut bounded = String::new();
    let mut bounded_chars = 0;
    let content_limit = MAX_WORKER_FAILURE_CHARS.saturating_sub(1);
    for character in message.chars() {
        let safe = if character.is_control() {
            character.escape_default().to_string()
        } else {
            character.to_string()
        };
        let safe_chars = safe.chars().count();
        if bounded_chars + safe_chars > content_limit {
            bounded.push('…');
            return bounded;
        }
        bounded.push_str(&safe);
        bounded_chars += safe_chars;
    }
    bounded
}

fn report_visible_startup_error(error: &eframe::Error) {
    let detail = error.to_string();
    let mut characters = detail.chars();
    let mut detail: String = characters.by_ref().take(MAX_STARTUP_DETAIL_CHARS).collect();
    if characters.next().is_some() {
        detail.push('…');
    }
    let message = visible_startup_message(&detail);
    eprintln!("{message}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(APP_NAME)
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

fn visible_startup_message(detail: &str) -> String {
    format!(
        "{APP_NAME} could not start.\n\n{detail}\n\nTry again. If the problem repeats, keep this message for troubleshooting."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn zero_arguments_select_visible_mode() {
        assert_eq!(startup_mode(Vec::<OsString>::new()), StartupMode::Visible);
    }

    #[test]
    fn exact_private_argument_selects_language_worker() {
        assert_eq!(
            startup_mode([OsString::from(LANGUAGE_WORKER_ARGUMENT)]),
            StartupMode::LanguageWorker
        );
    }

    #[test]
    fn private_argument_with_extra_argument_selects_visible_mode() {
        assert_eq!(
            startup_mode([
                OsString::from(LANGUAGE_WORKER_ARGUMENT),
                OsString::from("extra"),
            ]),
            StartupMode::Visible
        );
    }

    #[test]
    fn unknown_argument_selects_visible_mode() {
        assert_eq!(
            startup_mode([OsString::from("--oxide-language-worker-extra")]),
            StartupMode::Visible
        );
    }

    #[test]
    fn startup_message_preserves_the_cause_without_guessing_the_remedy() {
        let message = visible_startup_message("could not start the runtime coordinator");

        assert!(message.contains("could not start the runtime coordinator"));
        assert!(!message.contains("graphics driver"));
        assert!(message.contains("keep this message for troubleshooting"));
    }

    #[test]
    fn worker_failure_reason_escapes_controls_and_is_bounded() {
        let reason = format!("bad\n{}", "x".repeat(MAX_WORKER_FAILURE_CHARS * 2));
        let bounded = bounded_worker_failure(&reason);

        assert!(bounded.starts_with("bad\\n"));
        assert!(!bounded.contains('\n'));
        assert!(bounded.chars().count() <= MAX_WORKER_FAILURE_CHARS);
        assert!(bounded.ends_with('…'));
    }
}
