//! Route Lege's diagnostics to logcat.
//!
//! Two separate problems:
//!
//! 1. The `log` crate has no output on Android without a logger installed.
//! 2. A large amount of pipeline and GPU diagnostics goes to stdout/stderr
//!    directly (`lege-gpu` alone has ~34 `eprintln!` sites in
//!    `vision/onnx/graph.rs`). Android discards a native library's stdio, so
//!    those lines vanish. Converting them all to `log` calls would touch
//!    hundreds of desktop lines; redirecting the file descriptors captures
//!    them without changing shared code at all.

use std::io::{BufRead, BufReader};
use std::sync::Once;

static INIT: Once = Once::new();

/// Install the logcat logger and redirect stdout/stderr into it.
///
/// Idempotent — safe to call from every JNI entry point.
pub fn init() {
    INIT.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("lege"),
        );

        if let Err(error) = redirect_stdio() {
            log::warn!("lege: stdout/stderr not redirected to logcat: {error}");
        }

        log::info!("lege: android logging initialised");
    });
}

/// Point fds 1 and 2 at a pipe drained by a forwarding thread.
fn redirect_stdio() -> std::io::Result<()> {
    let mut fds = [0_i32; 2];

    // SAFETY: `pipe` writes exactly two ints into the array we hand it, and
    // `dup2` only reinstalls descriptors we own. Failure is reported through
    // the return value, not through invalid state.
    let read_fd = unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        if libc::dup2(write_fd, libc::STDOUT_FILENO) < 0
            || libc::dup2(write_fd, libc::STDERR_FILENO) < 0
        {
            let error = std::io::Error::last_os_error();
            libc::close(read_fd);
            libc::close(write_fd);
            return Err(error);
        }
        // Both standard descriptors now reference the pipe; the original
        // write end is redundant.
        libc::close(write_fd);
        read_fd
    };

    std::thread::Builder::new()
        .name("lege-logcat".to_owned())
        .spawn(move || {
            // SAFETY: `read_fd` is owned by this thread from here on and is
            // not closed anywhere else.
            let pipe = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(read_fd) };
            for line in BufReader::new(pipe).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => log::info!("{line}"),
                    Err(_) => break,
                }
            }
        })?;

    Ok(())
}
