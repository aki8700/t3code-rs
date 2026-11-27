use super::{Command, CoreError, CoreResult, io, process_error, process_start_time};
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    pin::Pin,
    sync::Arc,
    thread,
};
use tokio::process::Child;

type Hook =
    Arc<dyn Fn(i32, String) -> Pin<Box<dyn Future<Output = CoreResult<()>> + Send>> + Send + Sync>;
tokio::task_local! { static SPAWN_HOOK: Hook; }

/// Register each CLI attempt in this future before allowing its process group to exec.
pub async fn with_spawn_hook<T: Future, F: Future<Output = CoreResult<()>> + Send + 'static>(
    turn: T,
    record: impl Fn(i32, String) -> F + Send + Sync + 'static,
) -> T::Output {
    SPAWN_HOOK
        .scope(
            Arc::new(move |pid, start| Box::pin(record(pid, start))),
            turn,
        )
        .await
}

// Another concurrent fork can temporarily inherit our writer. An explicit denial
// also wakes the child if the task is cancelled while its hook is pending.
struct Permit(Option<io::PipeWriter>);

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(writer) = &mut self.0 {
            let _ = writer.write_all(&[0]);
        }
    }
}

fn await_gate(reader: &mut io::PipeReader, parent: libc::pid_t) -> io::Result<()> {
    let mut fd = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let denied = || io::Error::from_raw_os_error(libc::ECANCELED);
    loop {
        // SAFETY: only async-signal-safe operations run between fork and exec.
        // If another pre-exec child inherited our writer, reparenting still aborts
        // this child even when the parent's death cannot immediately deliver EOF.
        if unsafe { libc::getppid() } != parent {
            return Err(denied());
        }
        match unsafe { libc::poll(&mut fd, 1, 1000) } {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
            -1 => return Err(io::Error::last_os_error()),
            0 => continue,
            _ => {
                let mut byte = [0];
                return match reader.read(&mut byte) {
                    Ok(1) if byte == [1] => Ok(()),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    _ => Err(denied()),
                };
            }
        }
    }
}

fn join(launch: thread::JoinHandle<io::Result<Child>>) -> io::Result<Child> {
    launch
        .join()
        .map_err(|_| io::Error::other("CLI spawn thread panicked"))?
}

type Spawn = Box<dyn FnOnce() -> io::Result<Child> + Send>;

pub(super) async fn prepare(mut cmd: Command) -> CoreResult<Spawn> {
    let Ok(record) = SPAWN_HOOK.try_with(Arc::clone) else {
        return Ok(Box::new(move || cmd.spawn()));
    };
    let (mut pid_reader, mut pid_writer) = io::pipe().map_err(process_error)?;
    let (mut gate_reader, writer) = io::pipe().map_err(process_error)?;
    let writer_fd = writer.as_raw_fd();
    let mut permit = Permit(Some(writer));
    let parent = std::process::id() as libc::pid_t;

    // SAFETY: the callback uses only close, getpid, write and await_gate's syscalls.
    // The parent owns writer_fd until the synchronous PID handshake completes.
    unsafe {
        cmd.pre_exec(move || {
            libc::close(writer_fd);
            pid_writer.write_all(&libc::getpid().to_ne_bytes())?;
            await_gate(&mut gate_reader, parent)
        });
    }

    // Command::spawn waits for exec. Give it its own thread so the async SQLite
    // hook (including its blocking-pool work) can run on any Tokio runtime.
    let runtime = tokio::runtime::Handle::current();
    let launch = thread::Builder::new()
        .spawn(move || {
            let _entered = runtime.enter();
            cmd.spawn()
        })
        .map_err(process_error)?;
    let mut bytes = [0; size_of::<libc::pid_t>()];
    let pid_read =
        tokio::task::spawn_blocking(move || pid_reader.read_exact(&mut bytes).map(|()| bytes))
            .await
            .map_err(|err| {
                CoreError::Provider(format!("CLI spawn handshake task failed: {err}"))
            })?;
    let bytes = match pid_read {
        Ok(bytes) => bytes,
        Err(err) => {
            drop(permit);
            return Err(process_error(join(launch).err().unwrap_or(err)));
        }
    };
    let pid = libc::pid_t::from_ne_bytes(bytes);
    let start = tokio::task::spawn_blocking(move || {
        for attempt in 0..5 {
            if let Some(start) = process_start_time(pid as u32) {
                return Some(start);
            }
            if attempt < 4 {
                thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        None
    })
    .await
    .map_err(|err| CoreError::Provider(format!("CLI start-time probe task failed: {err}")))?
    .ok_or_else(|| CoreError::Provider(format!("could not identify starting CLI process {pid}")))?;
    record(pid, start).await?;
    // Release and obtain the Child in one poll under the active-run lock, so a
    // cancelled turn cannot leave an exec'd process group without its RunGuard.
    Ok(Box::new(move || {
        permit.0.take().expect("pending gate").write_all(&[1])?;
        join(launch)
    }))
}

pub(super) fn has_hook() -> bool {
    SPAWN_HOOK.try_with(|_| ()).is_ok()
}
