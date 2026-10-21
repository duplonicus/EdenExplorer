use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows::Win32::System::Console::{AllocConsole, GetConsoleWindow};

const LOG_FILE_NAME: &str = "eden.log";
const DEFAULT_FILTER: &str = "warn,EdenExplorer=info";
const DEBUG_FILTER: &str = "info,EdenExplorer=debug";

/// Start logging to `%LOCALAPPDATA%\ExplorerEden\eden.log`, recreated on each
/// launch. With `debug`, log more and open a console window to watch it live
/// (debug builds already have one). `RUST_LOG` overrides the filter either way.
pub fn init(debug: bool) {
    if debug && !has_console() {
        unsafe {
            let _ = AllocConsole();
        }
    }

    let file = open_log_file();
    let log_path = file.as_ref().map(|(_, path)| path.clone());
    let writer = LogWriter {
        file: file.map(|(file, _)| file),
        console: has_console(),
    };

    let default_filter = if debug { DEBUG_FILTER } else { DEFAULT_FILTER };
    let env = env_logger::Env::default().default_filter_or(default_filter);
    let result = env_logger::Builder::from_env(env)
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(writer)))
        .try_init();
    if result.is_err() {
        return;
    }

    install_panic_hook();

    log::info!("EdenExplorer {} starting", env!("CARGO_PKG_VERSION"));
    match log_path {
        Some(path) => log::info!("Logging to {}", path.display()),
        None => log::warn!("Could not create a log file; logging to the console only"),
    }
}

fn has_console() -> bool {
    !unsafe { GetConsoleWindow() }.is_invalid()
}

/// Open `eden.log`, or `eden-<pid>.log` when another Eden window (`--new-window`)
/// already holds it. Other processes can read the file while it is open, but
/// not write to or delete it, so a second window can't truncate the first one's log.
fn open_log_file() -> Option<(File, PathBuf)> {
    let dir = dirs::data_local_dir()?.join("ExplorerEden");
    std::fs::create_dir_all(&dir).ok()?;
    remove_stale_window_logs(&dir);

    let names = [
        LOG_FILE_NAME.to_owned(),
        format!("eden-{}.log", std::process::id()),
    ];
    names.into_iter().find_map(|name| {
        let path = dir.join(name);
        create_log_file(&path).ok().map(|file| (file, path))
    })
}

fn create_log_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
}

/// Delete `eden-<pid>.log` files left by windows that have closed. Logs of
/// windows still running are open without delete sharing, so they stay.
fn remove_stale_window_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if is_window_log_name(&entry.file_name().to_string_lossy()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn is_window_log_name(name: &str) -> bool {
    name.strip_prefix("eden-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

/// Release builds have no console, so a panic message would otherwise be lost.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("{info}\n{}", std::backtrace::Backtrace::force_capture());
        default_hook(info);
    }));
}

struct LogWriter {
    file: Option<File>,
    console: bool,
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            let _ = file.write_all(buf);
        }
        if self.console {
            let _ = io::stderr().write_all(buf);
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_only_per_window_log_names() {
        assert!(is_window_log_name("eden-1234.log"));
        assert!(!is_window_log_name("eden.log"));
        assert!(!is_window_log_name("eden-.log"));
        assert!(!is_window_log_name("eden-12a4.log"));
        assert!(!is_window_log_name("eden-1234.log.bak"));
        assert!(!is_window_log_name("settings.bin"));
    }

    #[test]
    fn second_open_of_a_log_file_fails_while_the_first_is_open() {
        let dir = std::env::temp_dir().join(format!("eden-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOG_FILE_NAME);

        let mut first = create_log_file(&path).unwrap();
        first.write_all(b"first window\n").unwrap();
        assert!(create_log_file(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        // Readers (Get-Content -Wait, an editor) can still open it.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first window\n");

        drop(first);
        assert!(create_log_file(&path).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
