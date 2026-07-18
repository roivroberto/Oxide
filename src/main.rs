#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use oxide_ide::{APP_NAME, LaunchMode, route_arguments, run_visible, run_worker};

const MAX_STARTUP_DETAIL_CHARS: usize = 2_048;

fn main() -> ExitCode {
    let mode = match route_arguments(std::env::args_os().skip(1)) {
        Ok(mode) => mode,
        Err(_) => return ExitCode::from(64),
    };
    match mode {
        LaunchMode::Visible => match run_visible() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                report_visible_startup_error(&error);
                ExitCode::from(70)
            }
        },
        LaunchMode::Worker(session) => {
            match run_worker(std::io::stdin(), std::io::stdout(), session) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Oxide IDE worker failed: {error:?}");
                    ExitCode::from(70)
                }
            }
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

    #[test]
    fn startup_message_preserves_the_cause_without_guessing_the_remedy() {
        let message = visible_startup_message("could not start the runtime coordinator");

        assert!(message.contains("could not start the runtime coordinator"));
        assert!(!message.contains("graphics driver"));
        assert!(message.contains("keep this message for troubleshooting"));
    }
}
