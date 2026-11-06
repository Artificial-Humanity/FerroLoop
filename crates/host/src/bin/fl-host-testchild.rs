//! Containment fixture. Readiness is a `READY` line on stdout. Stdin EOF is not
//! treated as exit except by modes that copy stdin.

use std::io::{self, Read, Write};
use std::process::exit;

fn main() {
    let mut args = std::env::args();
    let _argv0 = args.next();
    let mode = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    match mode.as_str() {
        "echo" => echo(),
        #[cfg(unix)]
        "export-pipe-writers" => export_pipe_writers(&rest),
        "report" => report(&rest),
        "exit" => {
            ready();
            let code = rest.first().and_then(|text| text.parse().ok()).unwrap_or(0);
            exit(code);
        }
        "exit-with-survivor" => {
            // The inherited Job contains this child too; its open stdio and live
            // membership keep the leader's exit inspectable until explicit cleanup.
            let _child = std::process::Command::new(std::env::current_exe().expect("exe"))
                .arg("stall")
                .stdin(std::process::Stdio::null())
                .spawn()
                .expect("survivor");
            ready();
            exit(rest.first().and_then(|text| text.parse().ok()).unwrap_or(0));
        }
        "stall" => {
            ready();
            park();
        }
        "stderr-flood" => {
            ready();
            let mut err = io::stderr();
            let chunk = vec![b'e'; 4096];
            for _ in 0..64 {
                let _ = err.write_all(&chunk);
            }
            let _ = err.flush();
            exit(0);
        }
        "grandchild" => grandchild(false),
        "detach" => grandchild(true),
        "orphan" => orphan(),
        "term-ignore" => term_ignore(),
        "dual-exit" => dual_exit(),
        "survive-eof" => {
            ready();
            park();
        }
        "fds" => fds(),
        "--session-supervisor" => stuck_supervisor(),
        "marker" => {
            ready();
            if let Some(path) = rest.first() {
                std::fs::write(path, b"ran").expect("marker");
            }
            park();
        }
        "record" => record(rest.first().map(String::as_str)),
        "record-survive-eof" => {
            record(rest.first().map(String::as_str));
            std::fs::write(rest.get(1).expect("EOF marker"), b"eof").expect("EOF marker");
            park();
        }
        "gated-lines" => {
            ready();
            for (index, marker) in rest.iter().take(2).enumerate() {
                while !std::path::Path::new(marker).exists() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                if index == 0 {
                    println!("HOLD");
                    io::stdout().flush().expect("hold flush");
                }
            }
            let mut out = io::stdout();
            let batches = std::path::Path::new(rest.get(2).expect("batch gates"));
            for batch in 0..20 {
                while !batches.join(batch.to_string()).exists() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                for index in batch * 10..(batch + 1) * 10 {
                    writeln!(out, "L{index}").expect("line");
                }
                out.flush().expect("batch flush");
            }
            park();
        }
        "lines" => lines(
            rest.first().and_then(|text| text.parse().ok()).unwrap_or(1),
            rest.get(1).map(String::as_str),
        ),
        "flood-stdout" => flood_stdout(),
        "flood-empty" => flood_empty(),
        "overflow" => overflow_line(),
        "maxline" => max_line(),
        "stderr-and-stdout" => stderr_and_stdout(),
        _ => {
            let _ = writeln!(io::stderr(), "unknown mode");
            exit(2);
        }
    }
}

fn ready() {
    let mut out = io::stdout();
    let _ = out.write_all(b"READY\n");
    let _ = out.flush();
}

fn park() -> ! {
    loop {
        std::thread::park();
    }
}

fn record(path: Option<&str>) {
    ready();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.expect("record path"))
        .expect("record");
    let mut input = io::stdin();
    let mut buf = [0u8; 1024];
    loop {
        match input.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let _ = file.write_all(&buf[..n]);
                let _ = file.flush();
            }
        }
    }
}

fn lines(count: usize, flushed: Option<&str>) {
    ready();
    let mut out = io::stdout();
    for index in 0..count {
        let _ = writeln!(out, "L{index}");
    }
    let _ = out.flush();
    if let Some(path) = flushed {
        std::fs::write(path, b"flushed").expect("flush marker");
    }
    park();
}

fn flood_stdout() {
    ready();
    let mut out = io::stdout();
    let chunk = vec![b'x'; 256 * 1024];
    for _ in 0..40 {
        let _ = out.write_all(&chunk);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
    let _ = out.write_all(b"TAIL\n");
    let _ = out.flush();
    park();
}

fn flood_empty() {
    ready();
    let mut out = io::stdout();
    let chunk = vec![b'\n'; 1024];
    for _ in 0..8 {
        let _ = out.write_all(&chunk);
        let _ = out.flush();
    }
    let _ = out.write_all(b"TAIL\n");
    let _ = out.flush();
    park();
}

fn overflow_line() {
    ready();
    let mut out = io::stdout();
    let chunk = vec![b'z'; 64 * 1024];
    let mut left = (1 << 20) + 2;
    while left > 0 {
        let n = left.min(chunk.len());
        let _ = out.write_all(&chunk[..n]);
        left -= n;
    }
    let _ = out.flush();
    park();
}

fn max_line() {
    ready();
    let mut out = io::stdout();
    let chunk = vec![b'y'; 64 * 1024];
    let mut left = 1 << 20;
    while left > 0 {
        let n = left.min(chunk.len());
        let _ = out.write_all(&chunk[..n]);
        left -= n;
    }
    let _ = out.write_all(b"\n");
    let _ = out.flush();
    park();
}

fn stderr_and_stdout() {
    ready();
    let mut err = io::stderr();
    let chunk = vec![b'e'; 64 * 1024];
    for _ in 0..64 {
        let _ = err.write_all(&chunk);
    }
    let _ = err.flush();
    let mut out = io::stdout();
    let _ = out.write_all(b"STDOUT_OK\n");
    let _ = out.flush();
    park();
}

fn echo() {
    ready();
    let mut input = io::stdin();
    let mut output = io::stdout();
    let mut buf = [0u8; 1024];
    loop {
        match input.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if output.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = output.flush();
            }
        }
    }
}

fn report(rest: &[String]) {
    ready();
    let mut argv = vec![std::env::args().next().unwrap_or_default(), "report".into()];
    argv.extend(rest.iter().cloned());
    let mut out = String::from("{\"argv\":[");
    for (index, arg) in argv.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&json_escape(arg));
        out.push('"');
    }
    out.push_str("],\"env\":{");
    let mut first = true;
    for (key, value) in std::env::vars() {
        if !first {
            out.push(',');
        }
        first = false;
        out.push('"');
        out.push_str(&json_escape(&key));
        out.push_str("\":\"");
        out.push_str(&json_escape(&value));
        out.push('"');
    }
    let tty = stdout_is_tty();
    out.push_str(&format!("}},\"isatty\":{tty}}}\n"));
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

fn json_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

fn stdout_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(1) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
fn grandchild(detach: bool) {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        exit(2);
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        exit(2);
    }
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        if detach && unsafe { libc::setsid() } < 0 {
            unsafe { libc::_exit(2) };
        }
        let text = format!("{}\n", unsafe { libc::getpid() });
        let _ = unsafe { libc::write(fds[1], text.as_ptr().cast(), text.len()) };
        unsafe { libc::close(fds[1]) };
        park();
    }
    unsafe { libc::close(fds[1]) };
    let mut file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut buf = String::new();
    let _ = io::Read::read_to_string(&mut file, &mut buf);
    ready();
    println!("CHILD {} END", buf.trim());
    let _ = io::stdout().flush();
    park();
}

#[cfg(not(unix))]
#[allow(clippy::zombie_processes)] // The fixture deliberately leaves a live descendant for containment.
fn grandchild(detach: bool) {
    let _ = detach;
    let child = std::process::Command::new(std::env::current_exe().unwrap_or_default())
        .arg("survive-eof")
        .spawn()
        .expect("grandchild");
    ready();
    println!("CHILD {} END", child.id());
    let _ = io::stdout().flush();
    park();
}

#[cfg(unix)]
fn orphan() {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        exit(2);
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        exit(2);
    }
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        let text = format!("{}\n", unsafe { libc::getpid() });
        let _ = unsafe { libc::write(fds[1], text.as_ptr().cast(), text.len()) };
        unsafe { libc::close(fds[1]) };
        park();
    }
    unsafe { libc::close(fds[1]) };
    let mut file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut buf = String::new();
    let _ = io::Read::read_to_string(&mut file, &mut buf);
    ready();
    println!("CHILD {} END", buf.trim());
    let _ = io::stdout().flush();
    unsafe { libc::_exit(0) };
}

#[cfg(not(unix))]
#[allow(clippy::zombie_processes)] // The fixture deliberately orphans a live descendant.
fn orphan() {
    let child = std::process::Command::new(std::env::current_exe().unwrap_or_default())
        .arg("survive-eof")
        .spawn()
        .expect("grandchild");
    ready();
    println!("CHILD {} END", child.id());
    let _ = io::stdout().flush();
    exit(0);
}

#[cfg(unix)]
fn term_ignore() {
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    ready();
    park();
}

#[cfg(not(unix))]
fn term_ignore() {
    ready();
    park();
}

#[cfg(unix)]
fn dual_exit() {
    let mut ready_pipe = [0; 2];
    let mut go_pipe = [0; 2];
    if unsafe { libc::pipe(ready_pipe.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(go_pipe.as_mut_ptr()) } != 0
    {
        exit(2);
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        exit(2);
    }
    if pid == 0 {
        unsafe {
            libc::close(ready_pipe[0]);
            libc::close(go_pipe[1]);
            let _ = libc::write(ready_pipe[1], b"1".as_ptr().cast(), 1);
            libc::close(ready_pipe[1]);
            let mut byte = [0u8; 1];
            let _ = libc::read(go_pipe[0], byte.as_mut_ptr().cast(), 1);
            libc::_exit(3);
        }
    }
    unsafe {
        libc::close(ready_pipe[1]);
        libc::close(go_pipe[0]);
        let mut byte = [0u8; 1];
        let _ = libc::read(ready_pipe[0], byte.as_mut_ptr().cast(), 1);
    }
    ready();
    unsafe {
        let _ = libc::write(go_pipe[1], b"1".as_ptr().cast(), 1);
        libc::close(go_pipe[1]);
        libc::_exit(7);
    }
}

#[cfg(not(unix))]
fn dual_exit() {
    ready();
    exit(7);
}

fn fds() {
    ready();
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "." || name == ".." {
                continue;
            }
            let path = format!("{dir}/{name}");
            let target = std::fs::read_link(&path)
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            println!("FD {name} {target}");
        }
    }
    println!("END");
    let _ = io::stdout().flush();
    let mut byte = [0u8; 1];
    let _ = io::stdin().read(&mut byte);
}

#[cfg(unix)]
use std::os::fd::FromRawFd;

/// Supervisor stand-in that publishes a gated vendor and then ignores control EOF.
/// The vendor calls `setsid` and parks. It does not wait on the release pipe, so a
/// numeric kill of this process would reparent it.
#[cfg(unix)]
fn stuck_supervisor() -> ! {
    use std::os::unix::ffi::OsStringExt;
    use std::time::{Duration, Instant};

    if unsafe { libc::setsid() } < 0 {
        exit(2);
    }
    let vendor = unsafe { libc::fork() };
    if vendor < 0 {
        exit(2);
    }
    if vendor == 0 {
        if unsafe { libc::setsid() } < 0 {
            unsafe { libc::_exit(2) };
        }
        for fd in 0..64 {
            unsafe { libc::close(fd) };
        }
        park();
    }
    let mut control = unsafe { std::fs::File::from_raw_fd(3) };
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match control.read(&mut byte) {
            Ok(0) => exit(2),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() > 1024 * 1024 {
                    exit(2);
                }
            }
            Err(_) => exit(2),
        }
    }
    drop(control);
    let value: serde_json::Value = serde_json::from_slice(&buf).unwrap_or_default();
    let cwd_bytes: Vec<u8> = value["cwd"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_default();
    if cwd_bytes.is_empty() {
        exit(2);
    }
    let cwd = std::path::PathBuf::from(std::ffi::OsString::from_vec(cwd_bytes));
    let vendor_pid = u32::try_from(vendor).unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let (vendor_start, pgid) = loop {
        if let Ok(ident) = fl_host::process_identity(vendor_pid)
            && ident.pid == vendor_pid
            && ident.pgid == vendor_pid
        {
            break (ident.start, ident.pgid);
        }
        if Instant::now() >= deadline {
            exit(2);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let supervisor_pid = std::process::id();
    let Ok(supervisor) = fl_host::process_identity(supervisor_pid) else {
        exit(2);
    };
    let note = format!(
        "{vendor_pid} {vendor_start} {supervisor_pid} {}\n",
        supervisor.start
    );
    if std::fs::write(cwd.join("gated.pids"), note).is_err() {
        exit(2);
    }
    let line = format!(
        "{{\"op\":\"identity\",\"vendor_pid\":{vendor_pid},\"vendor_start\":{vendor_start},\"pgid\":{pgid},\"supervisor_pid\":{supervisor_pid},\"supervisor_start\":{}}}\n",
        supervisor.start
    );
    let mut status = unsafe { std::fs::File::from_raw_fd(4) };
    if status.write_all(line.as_bytes()).is_err() || status.flush().is_err() {
        exit(2);
    }
    drop(status);
    park();
}

#[cfg(not(unix))]
fn stuck_supervisor() -> ! {
    exit(2);
}

// Transfer writers outside containment, then wait for the receiver's readiness
// acknowledgement before exiting. The test harness owns the received descriptors.
#[cfg(unix)]
fn export_pipe_writers(args: &[String]) {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(&args[0]).expect("descriptor receiver");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("timeout");
    for fd in match args[1].as_str() {
        "stdout" => vec![libc::STDOUT_FILENO],
        "stderr" => vec![libc::STDERR_FILENO],
        "both" => vec![libc::STDOUT_FILENO, libc::STDERR_FILENO],
        _ => panic!("pipe selection"),
    } {
        let mut byte = [1u8];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        // usize storage gives cmsghdr its required alignment.
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(&fd) as _) } as usize;
        let mut control = vec![0usize; space.div_ceil(std::mem::size_of::<usize>())];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(&fd) as _) as _;
            libc::CMSG_DATA(header).cast::<libc::c_int>().write(fd);
            assert_eq!(libc::sendmsg(stream.as_raw_fd(), &msg, 0), 1, "send writer");
        }
    }
    let mut ack = [0u8];
    stream.read_exact(&mut ack).expect("receiver ready");
    assert_eq!(ack, [1]);
}
