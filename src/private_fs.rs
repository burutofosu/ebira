//! Creation of corpus-owned files and directories, including temporary outputs.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// Request owner-only access at creation, before any data can be written. Existing
/// entries are not chmodded, and the caller's umask may further restrict new entries.
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

/// Also used for the lock file, which must be opened without truncation or replacement.
pub fn write_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
}

pub fn create(path: &Path) -> io::Result<File> {
    write_options().truncate(true).open(path)
}

pub fn copy(from: &Path, to: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        // fs::copy would carry the source's permissions onto the managed copy.
        let mut source = File::open(from)?;
        let mut destination = create(to)?;
        io::copy(&mut source, &mut destination)
    }
    #[cfg(not(unix))]
    {
        std::fs::copy(from, to)
    }
}
