//! CLI text output must not panic when a pipeline reader exits or a terminal
//! closes. Keep command cleanup and exit status intact; report other stdout
//! failures at the command boundary. This does not change server/client I/O.

use std::cell::RefCell;
use std::fmt::Arguments;
use std::io::{self, Write};

thread_local! {
    static STDOUT_ERROR: RefCell<Option<io::Error>> = const { RefCell::new(None) };
}

// Shadow the infallible std macros only within this CLI crate. Commands use
// several return types, so recording failures lets their normal cleanup run.
macro_rules! print {
    ($($arg:tt)*) => { $crate::output::stdout(format_args!($($arg)*)) };
}
macro_rules! println {
    () => { $crate::output::stdout(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::output::stdout(format_args!("{}\n", format_args!($($arg)*))) };
}
macro_rules! eprintln {
    () => { $crate::output::stderr(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::output::stderr(format_args!("{}\n", format_args!($($arg)*))) };
}

pub(super) fn stdout(args: Arguments<'_>) {
    if STDOUT_ERROR.with(|error| error.borrow().is_some()) {
        return;
    }
    if let Err(error) = io::stdout().lock().write_fmt(args) {
        STDOUT_ERROR.with(|slot| *slot.borrow_mut() = Some(error));
    }
}

pub(super) fn stderr(args: Arguments<'_>) {
    // A missing diagnostic consumer must not turn an ordinary command error
    // into a second panic while reporting the first one.
    let _ = io::stderr().lock().write_fmt(args);
}

pub(super) fn reset() {
    STDOUT_ERROR.with(|slot| *slot.borrow_mut() = None);
}

pub(super) fn check() -> Result<(), String> {
    STDOUT_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(error) => Err(error.to_string()),
        None => Ok(()),
    })
}

pub(super) fn finish(status: i32) -> i32 {
    let error = STDOUT_ERROR.with(|slot| slot.borrow_mut().take());
    let error = error.or_else(|| io::stdout().flush().err());
    match error {
        Some(error) if error.kind() != io::ErrorKind::BrokenPipe => {
            stderr(format_args!("uniterm: could not write stdout: {error}\n"));
            if status == 0 {
                1
            } else {
                status
            }
        }
        _ => status,
    }
}
