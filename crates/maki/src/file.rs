use std::{fs::File, io, path::Path};

#[cfg(target_vendor = "apple")]
pub(crate) fn open(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    // Darwin's O_NOFOLLOW_ANY checks all components in the kernel.
    const O_NOFOLLOW_ANY: i32 = 0x20000000;
    const O_NONBLOCK: i32 = 0x4;
    File::options()
        .read(true)
        .custom_flags(O_NOFOLLOW_ANY | O_NONBLOCK)
        .open(path)
}

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub(crate) use unix::open;

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
mod unix {
    use super::*;
    use std::{
        ffi::{CString, c_char, c_int},
        os::fd::{AsRawFd, FromRawFd},
        path::Component,
    };

    #[cfg(target_os = "linux")]
    const O_CLOEXEC: c_int = if cfg!(any(target_arch = "sparc", target_arch = "sparc64")) {
        0x400000
    } else {
        0x80000
    };
    #[cfg(target_os = "linux")]
    const O_NOFOLLOW: c_int = if cfg!(any(
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "m68k"
    )) {
        0x8000
    } else {
        0x20000
    };
    #[cfg(target_os = "linux")]
    const O_NONBLOCK: c_int = if cfg!(any(target_arch = "sparc", target_arch = "sparc64")) {
        0x4000
    } else if cfg!(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    )) {
        0x80
    } else {
        0x800
    };

    #[cfg(target_os = "freebsd")]
    const O_CLOEXEC: c_int = 0x00100000;
    #[cfg(target_os = "openbsd")]
    const O_CLOEXEC: c_int = 0x00010000;
    #[cfg(target_os = "netbsd")]
    const O_CLOEXEC: c_int = 0x00400000;
    #[cfg(target_os = "dragonfly")]
    const O_CLOEXEC: c_int = 0x00020000;
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    const O_NOFOLLOW: c_int = 0x100;
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    const O_NONBLOCK: c_int = 0x4;

    unsafe extern "C" {
        fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int, ...) -> c_int;
    }

    pub(crate) fn open(path: &Path) -> io::Result<File> {
        let mut file = File::open(if path.is_absolute() { "/" } else { "." })?;
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => name,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid path component",
                    ));
                }
            };
            let name = CString::new(name.as_encoded_bytes())?;
            // Each lookup is relative to the previously opened directory descriptor.
            // O_NONBLOCK prevents a substituted FIFO from blocking before metadata validation.
            let fd = unsafe {
                openat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // openat returned a new descriptor whose ownership is transferred to File.
            file = unsafe { File::from_raw_fd(fd) };
            if components.peek().is_some() && !file.metadata()?.is_dir() {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
        }
        Ok(file)
    }
}

#[cfg(all(
    unix,
    not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))
))]
pub(crate) fn open(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no secure file open on this platform",
    ))
}

#[cfg(not(unix))]
pub(crate) fn open(path: &Path) -> io::Result<File> {
    File::open(path)
}
