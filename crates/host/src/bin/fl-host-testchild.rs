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
        "report" => report(&rest),
        "exit" => {
            ready();
            let code = rest.first().and_then(|text| text.parse().ok()).unwrap_or(0);
            exit(code);
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
        "marker" => {
            ready();
            if let Some(path) = rest.first() {
                std::fs::write(path, b"ran").expect("marker");
            }
            park();
        }
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

fn park() {
    loop {
        std::thread::park();
    }
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
fn grandchild(detach: bool) {
    let _ = detach;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap_or_default())
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
fn orphan() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap_or_default())
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
