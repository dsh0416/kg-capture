use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ipc_channel::ipc::IpcOneShotServer;
use kg_capture_protocol::{HookEvent, HookHandshake, HostCommand, SessionNonce};

const X86: u16 = 0x014c;
const X64: u16 = 0x8664;

fn main() -> ExitCode {
    let result = match env::args().nth(1).as_deref() {
        Some("build") => build_and_stage(),
        Some("run") => run_application(),
        Some("test") => test_all(),
        Some("smoke") => smoke_test(),
        Some("verify") => verify_distribution(&workspace_root().join("dist")),
        _ => {
            eprintln!("usage: cargo xtask <build|run|test|smoke|verify>");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_application() -> Result<(), String> {
    build_release_artifacts()?;
    let root = workspace_root();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let run_directory = root
        .join("target/kg-capture-runs")
        .join(format!("{timestamp}-{}", std::process::id()));
    stage_artifacts(&run_directory)?;
    verify_distribution(&run_directory)?;
    let application = run_directory.join("kg-capture.exe");
    println!("starting {}", application.display());
    let child = Command::new(&application)
        .current_dir(application.parent().expect("application has a parent"))
        .spawn()
        .map_err(|error| format!("run {}: {error}", application.display()))?;
    println!("application started as process {}", child.id());
    Ok(())
}

fn test_all() -> Result<(), String> {
    println!("checking formatting");
    run_cargo("check formatting", &["fmt", "--all", "--", "--check"])?;

    println!("running unit tests");
    run_cargo("test protocol", &["test", "-p", "kg-capture-protocol"])?;
    run_cargo(
        "test x64 host",
        &[
            "test",
            "-p",
            "kg-capture-app",
            "--target",
            "x86_64-pc-windows-msvc",
        ],
    )?;
    run_cargo(
        "test x86 injector",
        &[
            "test",
            "-p",
            "kg-capture-injector",
            "--target",
            "i686-pc-windows-msvc",
        ],
    )?;

    println!("running strict architecture-specific lints");
    run_cargo(
        "lint x64 crates",
        &[
            "clippy",
            "-p",
            "kg-capture-app",
            "-p",
            "kg-capture-xtask",
            "--target",
            "x86_64-pc-windows-msvc",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run_cargo(
        "lint x86 crates",
        &[
            "clippy",
            "-p",
            "kg-capture-hook",
            "-p",
            "kg-capture-injector",
            "-p",
            "kg-capture-fixture",
            "--target",
            "i686-pc-windows-msvc",
            "--",
            "-D",
            "warnings",
        ],
    )?;

    println!("building the x64 release application");
    run_cargo(
        "build x64 release application",
        &[
            "build",
            "--release",
            "--package",
            "kg-capture-app",
            "--target",
            "x86_64-pc-windows-msvc",
        ],
    )?;

    println!("running end-to-end smoke test");
    smoke_test()?;
    println!("all checks passed");
    Ok(())
}

fn smoke_test() -> Result<(), String> {
    let root = workspace_root();
    for package in [
        "kg-capture-hook",
        "kg-capture-injector",
        "kg-capture-fixture",
    ] {
        run_cargo(
            &format!("build {package} for smoke test"),
            &[
                "build",
                "--release",
                "--package",
                package,
                "--target",
                "i686-pc-windows-msvc",
            ],
        )?;
    }

    let smoke_distribution = root.join("target/kg-capture-smoke-dist");
    fs::create_dir_all(&smoke_distribution)
        .map_err(|error| format!("create smoke-test distribution: {error}"))?;
    for file_name in ["kg-capture-injector.exe", "kg_capture_hook.dll"] {
        fs::copy(
            root.join("target/i686-pc-windows-msvc/release")
                .join(file_name),
            smoke_distribution.join(file_name),
        )
        .map_err(|error| format!("stage {file_name} for smoke test: {error}"))?;
    }
    verify_machine(&smoke_distribution.join("kg-capture-injector.exe"), X86)?;
    verify_machine(&smoke_distribution.join("kg_capture_hook.dll"), X86)?;

    let fixture = root.join("target/i686-pc-windows-msvc/release/kg-capture-fixture.exe");
    smoke_test_fixture(&root, &fixture, &smoke_distribution)
}

fn smoke_test_fixture(root: &Path, fixture: &Path, distribution: &Path) -> Result<(), String> {
    let (server, endpoint) = IpcOneShotServer::<HookHandshake>::new()
        .map_err(|error| format!("create smoke-test IPC server: {error}"))?;
    let nonce = SessionNonce([0x5a; 16]);
    let pid_file = root.join("target/kg-capture-smoke.pid");
    let log_directory = root.join("target/kg-capture-smoke-logs");
    fs::create_dir_all(&log_directory)
        .map_err(|error| format!("create smoke-test log directory: {error}"))?;
    let injector_log = log_directory.join("injector.log");
    let hook_log = log_directory.join("hook.log");
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(&injector_log);
    let _ = fs::remove_file(&hook_log);
    let (accept_sender, accept_receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("kg-capture-smoke-handshake".into())
        .spawn(move || {
            let _ = accept_sender.send(server.accept());
        })
        .map_err(|error| format!("start smoke-test IPC accept thread: {error}"))?;

    let status = hidden_command(&distribution.join("kg-capture-injector.exe"))
        .env("KG_CAPTURE_FIXTURE", "1")
        .env("KG_CAPTURE_INJECTOR_LOG_FILE", &injector_log)
        .env("RUST_LOG", "kg_capture=info")
        .args([
            "--launch",
            &fixture.to_string_lossy(),
            "--dll",
            &distribution.join("kg_capture_hook.dll").to_string_lossy(),
            "--ipc",
            &endpoint,
            "--nonce",
            &"5a".repeat(16),
            "--pid-file",
            &pid_file.to_string_lossy(),
            "--hook-log",
            &hook_log.to_string_lossy(),
        ])
        .status()
        .map_err(|error| format!("run launch-and-inject smoke test: {error}"))?;
    let fixture_process = SmokeFixture::from_pid_file(pid_file)?;
    if !status.success() {
        return Err(format!("launch-and-inject smoke test failed: {status}"));
    }

    let (_, handshake) = accept_receiver
        .recv_timeout(Duration::from_secs(10))
        .map_err(|error| format!("smoke-test hook handshake timed out: {error}"))?
        .map_err(|error| format!("accept smoke-test hook connection: {error}"))?;
    if handshake.hello.session_nonce != nonce {
        return Err("smoke-test handshake nonce mismatch".into());
    }
    if handshake.hello.process_id != fixture_process.process_id {
        return Err("smoke-test handshake came from an unexpected process".into());
    }
    handshake
        .command_sender
        .send(HostCommand::StartCapture)
        .map_err(|error| format!("start smoke-test capture: {error}"))?;

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut timeline_id = None;
    let mut playback = false;
    while Instant::now() < deadline && (timeline_id.is_none() || !playback) {
        match handshake.event_receiver.try_recv() {
            Ok(HookEvent::Timeline(timeline)) => {
                if timeline.lines.len() < 3
                    || timeline.lines.iter().any(|line| line.text.is_empty())
                {
                    return Err("hook emitted an invalid lyric timeline".into());
                }
                timeline_id = Some(timeline.id);
            }
            Ok(HookEvent::Playback(position))
                if timeline_id == Some(position.timeline_id)
                    && position.current_line.is_some()
                    && (0.0..=1.0).contains(&position.line_progress) =>
            {
                playback = true;
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = handshake.command_sender.send(HostCommand::Shutdown);
    if timeline_id.is_none() || !playback {
        return Err(format!(
            "semantic smoke test timed out (timeline={}, playback={playback})",
            timeline_id.is_some()
        ));
    }
    thread::sleep(Duration::from_millis(100));
    require_log_text(&injector_log, "kg_capture_start succeeded")?;
    require_log_text(&hook_log, "fixture mode selected")?;
    require_log_text(&hook_log, "semantic capture started")?;

    println!("suspended launch, x86 injection, semantic lyric IPC, and diagnostic logs passed");
    Ok(())
}

fn require_log_text(path: &Path, expected: &str) -> Result<(), String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("read diagnostic log {}: {error}", path.display()))?;
    if contents.contains(expected) {
        Ok(())
    } else {
        Err(format!(
            "diagnostic log {} did not contain {expected:?}",
            path.display()
        ))
    }
}

struct SmokeFixture {
    process_id: u32,
    pid_file: PathBuf,
}

impl SmokeFixture {
    fn from_pid_file(pid_file: PathBuf) -> Result<Self, String> {
        let process_id = fs::read_to_string(&pid_file)
            .map_err(|error| format!("read smoke-test PID file: {error}"))?
            .trim()
            .parse()
            .map_err(|error| format!("parse smoke-test process ID: {error}"))?;
        Ok(Self {
            process_id,
            pid_file,
        })
    }
}

impl Drop for SmokeFixture {
    fn drop(&mut self) {
        // This PID is written by the injector for the fixture created by this
        // test, so cleanup cannot target an unrelated user process.
        let _ = hidden_command(Path::new("taskkill"))
            .args(["/PID", &self.process_id.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_file(&self.pid_file);
    }
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

fn build_and_stage() -> Result<(), String> {
    build_release_artifacts()?;
    let distribution = workspace_root().join("dist");
    stage_artifacts(&distribution)?;
    verify_distribution(&distribution)?;
    println!("staged release artifacts in {}", distribution.display());
    Ok(())
}

fn build_release_artifacts() -> Result<(), String> {
    let builds = [
        ("kg-capture-app", "x86_64-pc-windows-msvc"),
        ("kg-capture-hook", "i686-pc-windows-msvc"),
        ("kg-capture-injector", "i686-pc-windows-msvc"),
    ];

    for (package, target) in builds {
        run_cargo(
            &format!("build {package} for {target}"),
            &[
                "build",
                "--release",
                "--package",
                package,
                "--target",
                target,
            ],
        )?;
    }

    Ok(())
}

fn stage_artifacts(destination: &Path) -> Result<(), String> {
    let root = workspace_root();
    fs::create_dir_all(destination)
        .map_err(|error| format!("create {}: {error}", destination.display()))?;
    let artifacts = [
        (
            root.join("target/x86_64-pc-windows-msvc/release/kg-capture.exe"),
            destination.join("kg-capture.exe"),
        ),
        (
            root.join("target/i686-pc-windows-msvc/release/kg-capture-injector.exe"),
            destination.join("kg-capture-injector.exe"),
        ),
        (
            root.join("target/i686-pc-windows-msvc/release/kg_capture_hook.dll"),
            destination.join("kg_capture_hook.dll"),
        ),
    ];
    for (source, destination) in artifacts {
        fs::copy(&source, &destination).map_err(|error| {
            format!(
                "stage {} as {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
    }

    Ok(())
}

fn run_cargo(context: &str, arguments: &[&str]) -> Result<(), String> {
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(arguments)
        .status()
        .map_err(|error| format!("{context}: could not run Cargo: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{context}: Cargo exited with {status}"))
    }
}

fn verify_distribution(distribution: &Path) -> Result<(), String> {
    let expected = [
        ("kg-capture.exe", X64),
        ("kg-capture-injector.exe", X86),
        ("kg_capture_hook.dll", X86),
    ];
    for (file_name, expected_machine) in expected {
        let path = distribution.join(file_name);
        verify_machine(&path, expected_machine)?;
    }
    Ok(())
}

fn verify_machine(path: &Path, expected_machine: u16) -> Result<(), String> {
    let actual =
        pe_machine(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if actual == expected_machine {
        Ok(())
    } else {
        Err(format!(
            "{} has PE machine 0x{actual:04x}, expected 0x{expected_machine:04x}",
            path.display()
        ))
    }
}

fn pe_machine(path: &Path) -> io::Result<u16> {
    let bytes = fs::read(path)?;
    if bytes.get(..2) != Some(b"MZ") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing MZ header",
        ));
    }
    let pe_offset_bytes: [u8; 4] = bytes
        .get(0x3c..0x40)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing PE offset"))?
        .try_into()
        .expect("slice length checked");
    let pe_offset = u32::from_le_bytes(pe_offset_bytes) as usize;
    if bytes.get(pe_offset..pe_offset + 4) != Some(b"PE\0\0") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing PE signature",
        ));
    }
    let machine: [u8; 2] = bytes
        .get(pe_offset + 4..pe_offset + 6)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing COFF header"))?
        .try_into()
        .expect("slice length checked");
    Ok(u16::from_le_bytes(machine))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask must live at crates/kg-capture-xtask")
        .to_owned()
}
