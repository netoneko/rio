//! A pty stand-in for kernels without `/dev/ptmx` (Akuma, musl build).
//!
//! When `forkpty` fails, the shell runs as `sh -i` on pipes and a relay
//! thread plays the kernel's line discipline: rio's side of the "pty" is one
//! end of a socketpair (so the reactor, epoll and `Pty` stay unchanged), the
//! relay owns the other end plus the shell's stdin/stdout. Cooked mode only:
//! echo, erase, kill-line, ^C as SIGINT to the shell's process group, ^D as
//! EOF, CR→NL on input and NL→CRLF on output. Full-screen programs need a
//! real tty and will not work here.

/// The cooked-mode half of a line discipline (ICANON|ECHO|ICRNL), fed with
/// the bytes the terminal writes to the "master".
#[derive(Default)]
pub(crate) struct LineDiscipline {
    line: Vec<u8>,
    esc: Esc,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Esc {
    #[default]
    None,
    Start,
    Seq,
}

/// What one batch of terminal input asks the relay to do.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Output {
    /// Bytes to send back to the terminal (the echo).
    pub echo: Vec<u8>,
    /// Completed input for the shell's stdin.
    pub to_shell: Vec<u8>,
    /// ^C was typed: interrupt the foreground process group.
    pub interrupt: bool,
    /// ^D on an empty line: close the shell's stdin.
    pub eof: bool,
}

impl LineDiscipline {
    pub(crate) fn input(&mut self, bytes: &[u8]) -> Output {
        let mut out = Output::default();
        for &b in bytes {
            // Escape sequences (arrows, function keys) have no meaning in
            // cooked mode without line editing: swallow them whole.
            match self.esc {
                Esc::Start => {
                    self.esc = if b == b'[' || b == b'O' { Esc::Seq } else { Esc::None };
                    continue;
                }
                Esc::Seq => {
                    if (0x40..=0x7e).contains(&b) {
                        self.esc = Esc::None;
                    }
                    continue;
                }
                Esc::None => {}
            }
            match b {
                0x1b => self.esc = Esc::Start,
                b'\r' | b'\n' => {
                    self.line.push(b'\n');
                    out.to_shell.append(&mut self.line);
                    out.echo.extend_from_slice(b"\r\n");
                }
                0x7f | 0x08 => {
                    if self.erase_char() {
                        out.echo.extend_from_slice(b"\x08 \x08");
                    }
                }
                0x17 => {
                    // ^W: erase trailing blanks, then the word before them.
                    while self.line.last() == Some(&b' ') {
                        self.line.pop();
                        out.echo.extend_from_slice(b"\x08 \x08");
                    }
                    while self.line.last().is_some_and(|&c| c != b' ') {
                        self.erase_char();
                        out.echo.extend_from_slice(b"\x08 \x08");
                    }
                }
                0x15 => {
                    // ^U: kill the whole line.
                    while self.erase_char() {
                        out.echo.extend_from_slice(b"\x08 \x08");
                    }
                }
                0x03 => {
                    self.line.clear();
                    out.echo.extend_from_slice(b"^C\r\n");
                    out.interrupt = true;
                }
                0x04 => {
                    if self.line.is_empty() {
                        out.eof = true;
                    } else {
                        // VEOF on a non-empty line: deliver it without a newline.
                        out.to_shell.append(&mut self.line);
                    }
                }
                b'\t' => {
                    self.line.push(b);
                    out.echo.push(b);
                }
                c if c < 0x20 => {}
                c => {
                    self.line.push(c);
                    out.echo.push(c);
                }
            }
        }
        out
    }

    /// Drop the last character (a whole UTF-8 sequence). False if empty.
    fn erase_char(&mut self) -> bool {
        if self.line.is_empty() {
            return false;
        }
        while let Some(c) = self.line.pop() {
            if c & 0xc0 != 0x80 {
                break;
            }
        }
        true
    }
}

/// OPOST|ONLCR: the shell's output with every NL turned into CRLF.
pub(crate) fn onlcr(bytes: &[u8], out: &mut Vec<u8>) {
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
}

#[cfg(target_env = "musl")]
pub(super) use spawn::spawn;

#[cfg(target_env = "musl")]
mod spawn {
    use super::{onlcr, LineDiscipline};
    use crate::unix::{
        default_shell_command, set_child_envs, set_nonblocking, signals::Signals,
        Child, Pty,
    };
    use std::fs::File;
    use std::io::{Error, ErrorKind};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

    fn pipe() -> Result<(OwnedFd, OwnedFd), Error> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            return Err(Error::last_os_error());
        }
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    fn socketpair() -> Result<(OwnedFd, OwnedFd), Error> {
        let mut fds = [0; 2];
        let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
        if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) } < 0 {
            return Err(Error::last_os_error());
        }
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    /// Spawn `shell` on pipes behind a relay thread. The returned `Pty`'s
    /// file is the terminal's end of a socketpair.
    pub(crate) fn spawn(
        shell: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: Option<&str>,
        signals: Signals,
    ) -> Result<Pty, Error> {
        let cwd = cwd.and_then(|d| std::ffi::CString::new(d).ok());
        let (master, relay_end) = socketpair()?;
        let (shell_in_r, shell_in_w) = pipe()?;
        let (shell_out_r, shell_out_w) = pipe()?;

        // Without a tty the shell is only interactive (prompt, no exit on
        // the first error) when told so.
        let interactive = ["-i".to_string()];
        let args = if args.is_empty() { &interactive[..] } else { args };

        match unsafe { libc::fork() } {
            0 => unsafe {
                // Own process group, so ^C reaches the shell and its jobs
                // without touching rio.
                libc::setsid();
                libc::dup2(shell_in_r.as_raw_fd(), 0);
                libc::dup2(shell_out_w.as_raw_fd(), 1);
                libc::dup2(shell_out_w.as_raw_fd(), 2);
                // Close everything else rio has open. Akuma does not honour
                // SOCK_CLOEXEC (rio's own sockets showed up in every shell),
                // and a leaked relay socket or pipe end keeps another pane's
                // EOF from ever arriving.
                for fd in 3..1024 {
                    libc::close(fd);
                }
                if let Some(dir) = &cwd {
                    libc::chdir(dir.as_ptr());
                }
                set_child_envs(envs);
                default_shell_command(shell, args);
                libc::_exit(127)
            },
            pid if pid > 0 => {
                drop((shell_in_r, shell_out_w));
                let raw = master.into_raw_fd();
                unsafe { set_nonblocking(raw) };
                std::thread::Builder::new()
                    .name("pipe-pty relay".into())
                    .spawn(move || relay(relay_end, shell_in_w, shell_out_r, pid))?;
                // `Child` keeps the raw fd for ioctls; `file` owns it.
                let file = unsafe { File::from_raw_fd(raw) };
                Ok(Pty {
                    child: Child::new(raw, pid, String::new(), None),
                    child_event_emitted: false,
                    signals,
                    file,
                    token: corcovado::Token(0),
                    signals_token: corcovado::Token(0),
                })
            }
            _ => Err(Error::last_os_error()),
        }
    }

    fn read(fd: RawFd, buf: &mut [u8]) -> Result<usize, Error> {
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = Error::last_os_error();
            if err.kind() != ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    fn write_all(fd: RawFd, mut buf: &[u8]) -> Result<(), Error> {
        while !buf.is_empty() {
            let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
            if n > 0 {
                buf = &buf[n as usize..];
                continue;
            }
            let err = Error::last_os_error();
            match err.kind() {
                ErrorKind::Interrupted => {}
                ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(1))
                }
                _ => return Err(err),
            }
        }
        Ok(())
    }

    /// Shuttle bytes until either side closes. Dropping `term` on the way
    /// out is what rio sees as the pty hanging up.
    fn relay(term: OwnedFd, shell_in: OwnedFd, shell_out: OwnedFd, pid: libc::pid_t) {
        let mut shell_in = Some(shell_in);
        let mut ld = LineDiscipline::default();
        let mut buf = vec![0u8; 16 * 1024];
        let mut translated = Vec::with_capacity(32 * 1024);
        let (t, o) = (term.as_raw_fd(), shell_out.as_raw_fd());
        loop {
            let mut fds = [
                libc::pollfd { fd: t, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: o, events: libc::POLLIN, revents: 0 },
            ];
            // Sliced, never -1: a lost wakeup costs 50 ms instead of a hang.
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, 50) } < 0 {
                if Error::last_os_error().kind() == ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            let ready = libc::POLLIN | libc::POLLHUP | libc::POLLERR;

            if fds[0].revents & ready != 0 {
                let n = match read(t, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let out = ld.input(&buf[..n]);
                if write_all(t, &out.echo).is_err() {
                    return;
                }
                if let Some(fd) = &shell_in {
                    // A dead shell shows up as EOF on its stdout below.
                    let _ = write_all(fd.as_raw_fd(), &out.to_shell);
                }
                if out.interrupt {
                    unsafe { libc::kill(-pid, libc::SIGINT) };
                }
                if out.eof {
                    shell_in = None;
                }
            }

            if fds[1].revents & ready != 0 {
                let n = match read(o, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                translated.clear();
                onlcr(&buf[..n], &mut translated);
                if write_all(t, &translated).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(bytes: &[u8]) -> Output {
        LineDiscipline::default().input(bytes)
    }

    #[test]
    fn line_is_echoed_and_delivered_on_enter() {
        let out = feed(b"ls\r");
        assert_eq!(out.echo, b"ls\r\n");
        assert_eq!(out.to_shell, b"ls\n");
    }

    #[test]
    fn nothing_reaches_the_shell_before_enter() {
        let mut ld = LineDiscipline::default();
        assert_eq!(ld.input(b"ec").to_shell, b"");
        assert_eq!(ld.input(b"ho\r").to_shell, b"echo\n");
    }

    #[test]
    fn backspace_erases_one_utf8_char() {
        let out = feed("aé\x7f\x7f\x7fb\r".as_bytes());
        assert_eq!(out.to_shell, b"b\n");
        // two erases echoed, the third had nothing to erase
        assert_eq!(out.echo, "aé\x08 \x08\x08 \x08b\r\n".as_bytes());
    }

    #[test]
    fn kill_line_and_word_erase() {
        assert_eq!(feed(b"foo bar\x15x\r").to_shell, b"x\n");
        assert_eq!(feed(b"foo bar  \x17baz\r").to_shell, b"foo baz\n");
    }

    #[test]
    fn ctrl_c_discards_the_line_and_interrupts() {
        let out = feed(b"sleep 9\x03");
        assert!(out.interrupt);
        assert_eq!(out.to_shell, b"");
        let mut ld = LineDiscipline::default();
        ld.input(b"abc\x03");
        assert_eq!(ld.input(b"\r").to_shell, b"\n");
    }

    #[test]
    fn ctrl_d_is_eof_only_on_an_empty_line() {
        assert!(feed(b"\x04").eof);
        let out = feed(b"ab\x04");
        assert!(!out.eof);
        assert_eq!(out.to_shell, b"ab");
    }

    #[test]
    fn escape_sequences_are_swallowed() {
        let out = feed(b"a\x1b[A\x1bOPb\x1b[1;5Cc\r");
        assert_eq!(out.to_shell, b"abc\n");
        assert_eq!(out.echo, b"abc\r\n");
        // split across writes
        let mut ld = LineDiscipline::default();
        ld.input(b"x\x1b");
        ld.input(b"[");
        assert_eq!(ld.input(b"Dy\r").to_shell, b"xy\n");
    }

    #[test]
    fn output_nl_becomes_crlf() {
        let mut out = Vec::new();
        onlcr(b"a\nb\n", &mut out);
        assert_eq!(out, b"a\r\nb\r\n");
    }
}
