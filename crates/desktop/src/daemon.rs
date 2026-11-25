use std::{
    env,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const ADDRESS: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 3774);
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);

pub(super) fn ensure_running() -> io::Result<Option<SpawnedDaemon>> {
    if health_ok(ADDRESS, PROBE_TIMEOUT) {
        return Ok(None);
    }

    let mut daemon = SpawnedDaemon(spawn()?);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while let Some(remaining) = remaining_time(deadline) {
        if health_ok(ADDRESS, remaining.min(PROBE_TIMEOUT)) {
            return Ok(Some(daemon));
        }
        if let Some(status) = daemon.0.try_wait()? {
            return Err(io::Error::other(format!(
                "t3d exited during startup ({status})"
            )));
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL),
        );
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "t3d did not become healthy within 5 seconds",
    ))
}

fn spawn() -> io::Result<Child> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let config = home.as_ref().map(|path| path.join(".t3-rs/config.toml"));
    let sibling = env::current_exe()
        .ok()
        .map(|path| path.with_file_name("t3d"));
    let local = home.map(|path| path.join(".local/bin/t3d"));
    for executable in sibling.into_iter().chain(local) {
        match spawn_at(&executable, config.as_deref()) {
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) => {}
            result => return result,
        }
    }
    spawn_at(Path::new("t3d"), config.as_deref())
}

fn spawn_at(executable: &Path, config: Option<&Path>) -> io::Result<Child> {
    let mut command = Command::new(executable);
    command.arg("serve");
    if let Some(path) = config.filter(|path| path.is_file()) {
        command.arg("--config").arg(path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

pub(super) struct SpawnedDaemon(Child);

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        // A child PID is not reused while the child is running or has yet to be reaped.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while let Some(remaining) = remaining_time(deadline) {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(remaining.min(POLL_INTERVAL));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn remaining_time(deadline: Instant) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    (!remaining.is_zero()).then_some(remaining)
}

fn health_ok(address: SocketAddr, timeout: Duration) -> bool {
    probe(address, timeout).unwrap_or(false)
}

fn probe(address: SocketAddr, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    let budget = || remaining_time(deadline).ok_or(io::ErrorKind::TimedOut);
    let mut stream = TcpStream::connect_timeout(&address, budget()?)?;
    stream.set_write_timeout(Some(budget()?))?;
    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;

    let mut response = [0; 256];
    let mut filled = 0;
    while filled < response.len() {
        stream.set_read_timeout(Some(budget()?))?;
        let read = stream.read(&mut response[filled..])?;
        if read == 0 {
            return Ok(false);
        }
        filled += read;
        if let Some(end) = response[..filled].iter().position(|byte| *byte == b'\n') {
            let Some(line) = response[..end].strip_suffix(b"\r") else {
                return Ok(false);
            };
            let mut fields = line.split(|byte| *byte == b' ');
            return Ok(matches!(fields.next(), Some(b"HTTP/1.0" | b"HTTP/1.1"))
                && fields.next() == Some(b"200"));
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
    };

    use super::*;

    #[test]
    fn health_probe_requires_http_200_on_healthz() {
        for (status_line, healthy) in [
            ("HTTP/1.1 200 OK\r\n", true),
            ("HTTP/1.0 200 OK\r\n", true),
            ("HTTP/1.1 503 Unavailable\r\n", false),
            ("HTTP/1.1 2000 Invalid\r\n", false),
            ("not HTTP 200\r\n", false),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = BufReader::new(&stream);
                let mut line = String::new();
                request.read_line(&mut line).unwrap();
                assert_eq!(line, "GET /healthz HTTP/1.1\r\n");
                while line != "\r\n" && !line.is_empty() {
                    line.clear();
                    request.read_line(&mut line).unwrap();
                }
                let _ = write!(stream, "{status_line}Content-Length: 0\r\n\r\n");
            });
            assert_eq!(health_ok(address, PROBE_TIMEOUT), healthy, "{status_line}");
            server.join().unwrap();
        }
    }
}
