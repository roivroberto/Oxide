#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use oxide_ide::{APP_NAME, LANGUAGE_WORKER_ARGUMENT, run_visible};

const MAX_STARTUP_DETAIL_CHARS: usize = 2_048;

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
    match run_visible() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_visible_startup_error(&error);
            ExitCode::from(70)
        }
    }
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
}
