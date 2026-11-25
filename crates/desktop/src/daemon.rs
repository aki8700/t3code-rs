use std::{
    env,
    ffi::{CStr, CString, c_char, c_void},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) const PORT: u16 = 3774;
const IPV4_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT);
const IPV6_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), PORT);
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_HEALTH_RESPONSE: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);

pub(super) fn ensure_running(check_ipv6: bool) -> io::Result<Option<SpawnedDaemon>> {
    if probe_localhost(check_ipv6, Instant::now() + PROBE_TIMEOUT)? {
        return Ok(None);
    }

    let mut daemon = SpawnedDaemon(spawn()?);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if probe_localhost(check_ipv6, deadline.min(Instant::now() + PROBE_TIMEOUT))? {
            // Another instance may have won the bind and our child may have exited.
            if daemon.0.try_wait()?.is_some() {
                return Ok(None);
            }
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
    #[cfg(unix)]
    {
        let mut entries: Vec<PathBuf> = env::var_os("PATH")
            .map(|p| env::split_paths(&p).collect())
            .unwrap_or_default();
        let mut extras = Vec::new();
        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            extras.push(home.join(".local/bin"));
        }
        extras.push(PathBuf::from("/opt/homebrew/bin"));
        extras.push(PathBuf::from("/usr/local/bin"));
        for dir in extras.into_iter().rev() {
            if !entries.contains(&dir) {
                entries.insert(0, dir);
            }
        }
        if let Ok(joined) = env::join_paths(entries) {
            command.env("PATH", joined);
        }
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
        if !matches!(self.0.try_wait(), Ok(None)) {
            return;
        }
        // A child PID is not reused while the child is running or has yet to be reaped.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while let Some(remaining) = remaining_time(deadline) {
            if !matches!(self.0.try_wait(), Ok(None)) {
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

fn probe_localhost(check_ipv6: bool, deadline: Instant) -> io::Result<bool> {
    let ipv4 = probe(
        IPV4_ADDRESS,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    // WebKit can use either loopback address for localhost.
    let ipv6 = check_ipv6
        && probe(
            IPV6_ADDRESS,
            deadline.saturating_duration_since(Instant::now()),
        )?;
    Ok(ipv4 || ipv6)
}

// A refused connection allows us to start a daemon. A response from an
// unverified listener must stop startup before the WebView is created.
fn probe(address: SocketAddr, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    let budget = || remaining_time(deadline).ok_or(io::ErrorKind::TimedOut);
    let mut stream = match TcpStream::connect_timeout(&address, budget()?) {
        Ok(stream) => stream,
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => return Ok(false),
        Err(err) => return Err(err),
    };

    let result: io::Result<bool> = (|| {
        stream.set_write_timeout(Some(budget()?))?;
        write!(
            stream,
            "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        )?;
        let mut response = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            stream.set_read_timeout(Some(budget()?))?;
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            if read > MAX_HEALTH_RESPONSE - response.len() {
                return Ok(false);
            }
            response.extend_from_slice(&chunk[..read]);
        }
        Ok(verify_health_response(&response))
    })();
    match result {
        Ok(true) => Ok(true),
        Ok(false) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("service at {address} did not return a valid t3d /healthz response"),
        )),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("could not verify t3d at {address} via /healthz: {err}"),
        )),
    }
}

fn verify_health_response(response: &[u8]) -> bool {
    let Ok(response) = std::str::from_utf8(response) else {
        return false;
    };
    let Some((head, body)) = response.split_once("\r\n\r\n") else {
        return false;
    };
    let mut lines = head.split("\r\n");
    let mut status = lines.next().unwrap_or_default().split(' ');
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) || status.next() != Some("200") {
        return false;
    }
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return false;
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() || value.trim().parse::<usize>() != Ok(body.len()) {
                return false;
            }
            content_length = Some(body.len());
        }
    }
    verify_health_json(body.as_bytes())
}

// Use the macOS JavaScriptCore JSON parser to validate the response under the
// desktop's existing dependency constraints.
fn verify_health_json(body: &[u8]) -> bool {
    type Context = *mut c_void;
    type Value = *const c_void;
    type JsString = *mut c_void;

    #[link(name = "JavaScriptCore", kind = "framework")]
    unsafe extern "C" {
        fn JSGlobalContextCreate(class: *mut c_void) -> Context;
        fn JSGlobalContextRelease(context: Context);
        fn JSStringCreateWithUTF8CString(string: *const c_char) -> JsString;
        fn JSStringRelease(string: JsString);
        fn JSValueMakeFromJSONString(context: Context, string: JsString) -> Value;
        fn JSValueIsObject(context: Context, value: Value) -> bool;
        fn JSValueIsBoolean(context: Context, value: Value) -> bool;
        fn JSValueIsString(context: Context, value: Value) -> bool;
        fn JSValueToBoolean(context: Context, value: Value) -> bool;
        fn JSValueProtect(context: Context, value: Value);
        fn JSValueUnprotect(context: Context, value: Value);
        fn JSObjectGetProperty(
            context: Context,
            object: *mut c_void,
            name: JsString,
            exception: *mut Value,
        ) -> Value;
    }

    let Ok(body) = CString::new(body) else {
        return false;
    };
    // The context and strings are owned by this call. Keep the parsed object
    // protected from GC while inspecting its data properties.
    unsafe {
        let context = JSGlobalContextCreate(std::ptr::null_mut());
        let string = JSStringCreateWithUTF8CString(body.as_ptr());
        let object = JSValueMakeFromJSONString(context, string);
        JSStringRelease(string);
        let valid = if !object.is_null() && JSValueIsObject(context, object) {
            JSValueProtect(context, object);
            let get = |name: &CStr| {
                let key = JSStringCreateWithUTF8CString(name.as_ptr());
                let value =
                    JSObjectGetProperty(context, object.cast_mut(), key, std::ptr::null_mut());
                JSStringRelease(key);
                value
            };
            let boolean_is = |name, expected| {
                let value = get(name);
                !value.is_null()
                    && JSValueIsBoolean(context, value)
                    && JSValueToBoolean(context, value) == expected
            };
            let version = get(c"version");
            let valid = !version.is_null()
                && JSValueIsString(context, version)
                && boolean_is(c"ok", true)
                && boolean_is(c"telemetry_enabled", false);
            JSValueUnprotect(context, object);
            valid
        } else {
            false
        };
        JSGlobalContextRelease(context);
        valid
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
    };

    use super::*;

    #[test]
    fn health_probe_requires_t3d_health_json() {
        let valid = r#"{"ok":true,"version":"0.1.0","telemetry_enabled":false}"#;
        for (status_line, body, healthy) in [
            ("HTTP/1.1 200 OK", "", false),
            ("HTTP/1.1 200 OK", "Hello", false),
            (
                "HTTP/1.1 200 OK",
                r#"{"ok":true,"version":"0.1.0","telemetry_enabled":false,}"#,
                false,
            ),
            (
                "HTTP/1.1 200 OK",
                r#"{"ok":true,"version":"0.1.0","telemetry_enabled":false} garbage"#,
                false,
            ),
            ("HTTP/1.1 200 OK", r#"{"ok":true}"#, false),
            (
                "HTTP/1.1 200 OK",
                r#"{"__proto__":{"ok":true,"version":"0.1.0","telemetry_enabled":false}}"#,
                false,
            ),
            (
                "HTTP/1.1 200 OK",
                r#"{"ok":true,"version":1,"telemetry_enabled":false}"#,
                false,
            ),
            (
                "HTTP/1.1 200 OK",
                r#"{"ok":true,"version":"0.1.0","telemetry_enabled":true}"#,
                false,
            ),
            (
                "HTTP/1.1 200 OK",
                r#"{"ok":1,"version":"0.1.0","telemetry_enabled":0}"#,
                false,
            ),
            (
                "HTTP/1.1 200 OK",
                r#"{"ok":false,"version":"0.1.0","telemetry_enabled":false}"#,
                false,
            ),
            ("HTTP/1.1 200 OK", valid, true),
            (
                "HTTP/1.1 200 OK",
                r#"{ "telemetry_enabled": false, "version": "0.1.\u0030", "ok": true }"#,
                true,
            ),
            ("HTTP/1.0 200 OK", valid, true),
            ("HTTP/1.1 503 Unavailable", valid, false),
            ("HTTP/1.1 2000 Invalid", valid, false),
            ("not HTTP 200", valid, false),
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
                let _ = write!(
                    stream,
                    "{status_line}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            });
            let result = probe(address, PROBE_TIMEOUT);
            if healthy {
                assert!(
                    matches!(result, Ok(true)),
                    "{status_line} {body}: {result:?}"
                );
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
            }
            server.join().unwrap();
        }
    }
}
