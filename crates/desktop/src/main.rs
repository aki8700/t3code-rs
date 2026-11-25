#[cfg(target_os = "macos")]
mod daemon;
#[cfg(target_os = "macos")]
mod links;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    if let Err(err) = macos::run() {
        eprintln!("T3 Code RS: {err}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("T3 Code RS desktop requires macOS");
    std::process::ExitCode::FAILURE
}
