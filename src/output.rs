//! Results propagate write and flush errors to the command boundary.
//! Diagnostics are best effort so reporting an error cannot cause another failure.

use std::fmt;
use std::io::{self, Write};

pub fn write(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

pub fn write_line(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{text}")?;
    stdout.flush()
}

pub fn diagnostic(message: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "{message}");
}
