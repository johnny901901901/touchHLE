/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! The guest app's own console output (`stdout` and `stderr`).
//!
//! Apps write diagnostics of their own through `printf`, `fwrite`, `puts`,
//! `perror` and `NSLog`. These used to go straight to the *host* process's
//! standard streams, which is fine on desktop, where touchHLE is normally
//! started from a terminal — but on iOS and Android nothing is attached to
//! those streams, so the app's diagnostics were discarded. That is exactly the
//! output that says *why* a game failed: engines routinely print the name of
//! the asset they could not load before quietly rendering nothing, and with
//! the stream discarded all that reaches the log is the crash that happens
//! several frames later.
//!
//! Routing this output through [log] instead puts it in `touchHLE_log.txt`
//! (and still on the host's stderr, which is what `echo!` writes to), so it is
//! available on every platform and interleaved with touchHLE's own messages.
//! Output is buffered until a newline so that a `printf` split across several
//! calls — the usual case, since `printf("%s: %d\n", ...)` may reach us in
//! pieces — produces one log line rather than one per fragment.

use std::io::Write;
use std::sync::Mutex;

/// The point at which a line with no newline in sight is flushed anyway, so a
/// guest that never prints `\n` can't make us buffer without bound.
const MAX_PENDING: usize = 4096;

/// Pending partial lines, indexed by [Stream] (`stdout`, then `stderr`).
static PENDING: Mutex<[Vec<u8>; 2]> = Mutex::new([Vec::new(), Vec::new()]);

/// Which of the guest's standard streams output belongs to.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}
impl Stream {
    fn name(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// A [Write] implementation for one of the guest's standard streams, so call
/// sites can keep using `write_all()`/`write()` as they did with
/// [std::io::stdout] and [std::io::stderr].
pub struct GuestConsole(Stream);

impl Write for GuestConsole {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        write(self.0, buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        flush(self.0);
        Ok(())
    }
}

/// The guest's `stdout`, as a [Write].
pub fn stdout() -> GuestConsole {
    GuestConsole(Stream::Stdout)
}

/// The guest's `stderr`, as a [Write].
pub fn stderr() -> GuestConsole {
    GuestConsole(Stream::Stderr)
}

/// Record output the guest wrote to one of its standard streams, logging each
/// complete line.
pub fn write(stream: Stream, bytes: &[u8]) {
    let Ok(mut pending) = PENDING.lock() else {
        // A poisoned lock must not take down an app just because it printed
        // something; drop the output instead.
        return;
    };
    let pending = &mut pending[stream as usize];
    pending.extend_from_slice(bytes);

    while let Some(newline) = pending.iter().position(|&byte| byte == b'\n') {
        let line: Vec<u8> = pending.drain(..=newline).collect();
        emit(stream, &line[..line.len() - 1]);
    }

    if pending.len() >= MAX_PENDING {
        let line = std::mem::take(pending);
        emit(stream, &line);
    }
}

/// Log any buffered partial line, e.g. a progress message or prompt printed
/// without a trailing newline, so that it isn't lost.
pub fn flush(stream: Stream) {
    let Ok(mut pending) = PENDING.lock() else {
        return;
    };
    let line = std::mem::take(&mut pending[stream as usize]);
    drop(pending);
    if !line.is_empty() {
        emit(stream, &line);
    }
}

fn emit(stream: Stream, line: &[u8]) {
    // Guest output is not necessarily UTF-8 (or even text), so substitute
    // rather than discarding it.
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
        return;
    }
    log!("[app {}] {}", stream.name(), line);
}
