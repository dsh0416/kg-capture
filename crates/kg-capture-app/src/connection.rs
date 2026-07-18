use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ipc_channel::ipc::{IpcOneShotServer, IpcReceiver, IpcSender};
use kg_capture_protocol::{HookEvent, HookHandshake, HostCommand, PROTOCOL_VERSION, SessionNonce};
#[derive(Clone, Debug)]
pub struct Session {
    pub process_id: u32,
    pub command_sender: IpcSender<HostCommand>,
    pub event_receiver: Arc<Mutex<IpcReceiver<HookEvent>>>,
    log_directory: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Off,
}

impl LogLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Off => "OFF",
        }
    }
}

impl Session {
    pub fn connect(executable: PathBuf) -> Result<Self, String> {
        let executable = if executable.is_absolute() {
            executable
        } else {
            env::current_dir()
                .map_err(|error| format!("resolve current directory: {error}"))?
                .join(executable)
        };
        if !executable.is_file() {
            return Err(format!(
                "selected WeSing executable does not exist: {}",
                executable.display()
            ));
        }
        if executable.extension().and_then(|value| value.to_str()) != Some("exe") {
            return Err("selected WeSing program is not an .exe file".into());
        }
        let (server, endpoint) = IpcOneShotServer::<HookHandshake>::new()
            .map_err(|error| format!("create IPC server: {error}"))?;

        let mut nonce_bytes = [0; 16];
        getrandom::fill(&mut nonce_bytes)
            .map_err(|error| format!("create session nonce: {error}"))?;
        let nonce = SessionNonce(nonce_bytes);
        let log_directory = create_log_directory(nonce)?;
        let host_log = log_directory.join("host.log");
        let injector_log = log_directory.join("injector.log");
        let hook_log = log_directory.join("hook.log");
        append_log(
            &host_log,
            LogLevel::Info,
            format_args!(
                "session starting executable={} pid={} protocol={}",
                executable.display(),
                std::process::id(),
                PROTOCOL_VERSION
            ),
        );
        let with_logs = |error: String| {
            append_log(&host_log, LogLevel::Error, format_args!("{error}"));
            error
        };
        let injector = component_path("KG_CAPTURE_INJECTOR_PATH", "kg-capture-injector.exe")
            .map_err(&with_logs)?;
        let hook =
            component_path("KG_CAPTURE_HOOK_PATH", "kg_capture_hook.dll").map_err(&with_logs)?;
        append_log(
            &host_log,
            LogLevel::Debug,
            format_args!(
                "components injector={} hook={}",
                injector.display(),
                hook.display()
            ),
        );
        let status_file = env::temp_dir().join(format!(
            "kg-capture-injector-{}-{}.txt",
            std::process::id(),
            nonce_hex(nonce)
        ));
        let _ = fs::remove_file(&status_file);

        // Accept on a dedicated thread before injection begins. On Windows the
        // one-shot transport must be listening while the injected DLL connects.
        let (accept_sender, accept_receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("kg-capture-handshake".into())
            .spawn(move || {
                let _ = accept_sender.send(server.accept());
            })
            .map_err(|error| format!("start IPC accept thread: {error}"))?;

        let status = hidden_command(&injector)
            .env("KG_CAPTURE_INJECTOR_STATUS_FILE", &status_file)
            .env("KG_CAPTURE_INJECTOR_LOG_FILE", &injector_log)
            .args([
                "--launch",
                &executable.to_string_lossy(),
                "--launch-arg",
                "/DeskTop",
                "--dll",
                &hook.to_string_lossy(),
                "--ipc",
                &endpoint,
                "--nonce",
                &nonce_hex(nonce),
                "--hook-log",
                &hook_log.to_string_lossy(),
            ])
            .status()
            .map_err(|error| format!("launch {}: {error}", injector.display()))?;
        if !status.success() {
            let error = injector_error(status, &status_file);
            append_log(
                &host_log,
                LogLevel::Error,
                format_args!("injector: {error}"),
            );
            return Err(error);
        }
        append_log(
            &host_log,
            LogLevel::Info,
            format_args!("injector completed successfully"),
        );
        let _ = fs::remove_file(&status_file);

        let (_, handshake) = accept_receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| with_logs(format!("hook IPC handshake timed out: {error}")))?
            .map_err(|error| with_logs(format!("accept hook IPC connection: {error}")))?;
        append_log(
            &host_log,
            LogLevel::Info,
            format_args!(
                "hook handshake pid={} protocol={}",
                handshake.hello.process_id, handshake.hello.protocol_version
            ),
        );
        if handshake.hello.protocol_version != PROTOCOL_VERSION {
            return Err(format!(
                "protocol mismatch: host={}, hook={}",
                PROTOCOL_VERSION, handshake.hello.protocol_version
            ));
        }
        if handshake.hello.session_nonce != nonce {
            return Err("hook session nonce did not match".into());
        }

        Ok(Self {
            process_id: handshake.hello.process_id,
            command_sender: handshake.command_sender,
            event_receiver: Arc::new(Mutex::new(handshake.event_receiver)),
            log_directory,
        })
    }

    pub fn send(&self, command: HostCommand) -> Result<(), String> {
        append_log(
            &self.log_directory.join("host.log"),
            LogLevel::Debug,
            format_args!("send command {command:?}"),
        );
        self.command_sender
            .send(command)
            .map_err(|error| format!("send hook command: {error}"))
    }

    pub fn log_event(&self, event: &HookEvent) {
        let level = match event {
            HookEvent::Warning(_) => LogLevel::Warn,
            HookEvent::Error(_) => LogLevel::Error,
            _ => LogLevel::Debug,
        };
        append_log(
            &self.log_directory.join("host.log"),
            level,
            format_args!("receive event {event:?}"),
        );
    }
}

fn create_log_directory(nonce: SessionNonce) -> Result<PathBuf, String> {
    let root = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("kg-capture")
        .join("logs");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let directory = root.join(format!(
        "{timestamp}-{}-{}",
        std::process::id(),
        &nonce_hex(nonce)[..8]
    ));
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create log directory {}: {error}", directory.display()))?;
    Ok(directory)
}

fn append_log(path: &Path, level: LogLevel, arguments: std::fmt::Arguments<'_>) {
    if level < configured_log_level() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "{timestamp} {} {arguments}", level.label());
    }
}

fn configured_log_level() -> LogLevel {
    static LEVEL: OnceLock<LogLevel> = OnceLock::new();
    *LEVEL.get_or_init(|| minimum_log_level(env::var("RUST_LOG").ok().as_deref()))
}

fn minimum_log_level(filter: Option<&str>) -> LogLevel {
    let mut global = None;
    let mut package = None;
    for directive in filter.into_iter().flat_map(|value| value.split(',')) {
        let directive = directive.trim();
        let (target, level) = directive
            .rsplit_once('=')
            .map_or((None, directive), |(target, level)| (Some(target), level));
        let level = match level.trim().to_ascii_lowercase().as_str() {
            "trace" | "debug" => LogLevel::Debug,
            "info" => LogLevel::Info,
            "warn" => LogLevel::Warn,
            "error" => LogLevel::Error,
            "off" => LogLevel::Off,
            _ => continue,
        };
        match target {
            Some(target) if target.trim().starts_with("kg_capture") => package = Some(level),
            None => global = Some(level),
            _ => {}
        }
    }
    package.or(global).unwrap_or(LogLevel::Info)
}

fn injector_error(status: std::process::ExitStatus, status_file: &Path) -> String {
    let diagnostic = fs::read_to_string(status_file).ok();
    let _ = fs::remove_file(status_file);
    diagnostic
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| format!("injector exited with {status}"))
}

fn component_path(variable: &str, file_name: &str) -> Result<PathBuf, String> {
    if let Some(path) = env::var_os(variable).map(PathBuf::from) {
        return existing_file(path, variable);
    }

    let executable = env::current_exe().map_err(|error| format!("locate application: {error}"))?;
    let executable_directory = executable
        .parent()
        .ok_or_else(|| "application path has no parent directory".to_owned())?;
    let packaged = executable_directory.join(file_name);
    if packaged.is_file() {
        return Ok(packaged);
    }

    // Developer layout: target/<host triple>/<profile>/kg-capture.exe.
    if let (Some(profile), Some(target_root)) = (
        executable_directory.file_name(),
        executable_directory.parent().and_then(Path::parent),
    ) {
        let development = target_root
            .join("i686-pc-windows-msvc")
            .join(profile)
            .join(file_name);
        if development.is_file() {
            return Ok(development);
        }
    }

    Err(format!(
        "{file_name} was not found beside the application; set {variable} for a development build"
    ))
}

fn existing_file(path: PathBuf, variable: &str) -> Result<PathBuf, String> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{variable} points to missing file {}",
            path.display()
        ))
    }
}

fn nonce_hex(nonce: SessionNonce) -> String {
    let mut value = String::with_capacity(32);
    for byte in nonce.0 {
        use std::fmt::Write;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn hidden_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_is_fixed_width_hex() {
        assert_eq!(
            nonce_hex(SessionNonce([0xab; 16])),
            "abababababababababababababababab"
        );
    }

    #[test]
    fn log_level_defaults_to_info_and_honors_package_filter() {
        assert_eq!(minimum_log_level(None), LogLevel::Info);
        assert_eq!(
            minimum_log_level(Some("warn,kg_capture=debug")),
            LogLevel::Debug
        );
        assert_eq!(minimum_log_level(Some("error")), LogLevel::Error);
    }
}
