use std::env;
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::mem::{MaybeUninit, size_of, transmute};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kg_capture_protocol::{HookBootstrap, SessionNonce};
use thiserror::Error;
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, HANDLE, HMODULE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_CONTROL_X86, FlushInstructionCache, GetThreadContext, ReadProcessMemory,
    WriteProcessMemory,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows::Win32::System::LibraryLoader::{
    DONT_RESOLVE_DLL_REFERENCES, GetModuleHandleW, GetProcAddress, LoadLibraryExW,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
    PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx, VirtualProtectEx,
};
use windows::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, CreateRemoteThread, GetExitCodeThread, IsWow64Process2,
    LPTHREAD_START_ROUTINE, OpenProcess, PROCESS_BASIC_INFORMATION, PROCESS_CREATE_THREAD,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE, ResumeThread, STARTUPINFOW, SuspendThread, TerminateProcess,
    WaitForSingleObject,
};
use windows::core::{Error as WindowsError, PCSTR, PCWSTR, PWSTR};

const REMOTE_CALL_TIMEOUT_MS: u32 = 15_000;

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

fn main() {
    std::panic::set_hook(Box::new(|information| {
        injector_log(LogLevel::Error, format_args!("PANIC {information}"));
    }));
    injector_log(
        LogLevel::Info,
        format_args!(
            "injector starting pid={} arch={}",
            std::process::id(),
            std::env::consts::ARCH
        ),
    );
    if let Err(error) = run() {
        let message = format!("kg-capture-injector: {error}");
        injector_log(LogLevel::Error, format_args!("{message}"));
        if let Some(path) = env::var_os("KG_CAPTURE_INJECTOR_STATUS_FILE") {
            let _ = std::fs::write(path, &message);
        }
        let _ = writeln!(std::io::stderr(), "{message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), InjectorError> {
    if cfg!(not(target_arch = "x86")) {
        return Err(InjectorError::WrongInjectorArchitecture);
    }

    let arguments = Arguments::parse(env::args().skip(1))?;
    injector_log(
        LogLevel::Debug,
        format_args!(
            "arguments parsed dll={} hook_log={}",
            arguments.dll.display(),
            arguments.hook_log.display()
        ),
    );
    let canonical_dll = arguments
        .dll
        .canonicalize()
        .map_err(|source| InjectorError::Io {
            context: "resolve DLL path",
            source,
        })?;

    let bootstrap = HookBootstrap::new(
        &arguments.endpoint,
        arguments.nonce,
        &arguments.hook_log.to_string_lossy(),
    )
    .map_err(|source| InjectorError::Bootstrap(source.to_string()))?;

    match arguments.target {
        Target::Existing(process_id) => {
            let access = PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_READ
                | PROCESS_VM_WRITE;
            let process =
                OwnedHandle::new(unsafe { OpenProcess(access, false, process_id) }.map_err(
                    |source| InjectorError::Windows {
                        context: "open target process",
                        source,
                    },
                )?);
            validate_x86_target(process.raw())?;
            unsafe { inject(process.raw(), process_id, &canonical_dll, &bootstrap) }
        }
        Target::Launch(executable) => {
            injector_log(
                LogLevel::Info,
                format_args!("launching target {}", executable.display()),
            );
            let mut child = LaunchedProcess::suspended(&executable, &arguments.launch_arguments)?;
            injector_log(
                LogLevel::Debug,
                format_args!("target created suspended pid={}", child.process_id),
            );
            validate_x86_target(child.process.raw())?;
            let mut entrypoint_gate = EntrypointGate::arm(child.process.raw())?;
            child.run_loader_to_entrypoint(entrypoint_gate.address())?;
            entrypoint_gate.restore()?;
            injector_log(
                LogLevel::Info,
                format_args!("injecting before target entry point executes"),
            );
            unsafe {
                inject(
                    child.process.raw(),
                    child.process_id,
                    &canonical_dll,
                    &bootstrap,
                )?;
            }
            child.start()?;
            injector_log(
                LogLevel::Info,
                format_args!("target primary thread resumed"),
            );
            if let Some(pid_file) = &arguments.pid_file {
                std::fs::write(pid_file, child.process_id.to_string()).map_err(|source| {
                    InjectorError::Io {
                        context: "write launched-process PID file",
                        source,
                    }
                })?;
            }
            child.keep_running();
            injector_log(
                LogLevel::Info,
                format_args!(
                    "injection complete; target resumed pid={}",
                    child.process_id
                ),
            );
            let _ = writeln!(std::io::stdout(), "launched process {}", child.process_id);
            Ok(())
        }
    }
}

unsafe fn inject(
    process: HANDLE,
    process_id: u32,
    dll: &Path,
    bootstrap: &HookBootstrap,
) -> Result<(), InjectorError> {
    let mut dll_path: Vec<u16> = dll.as_os_str().encode_wide().collect();
    dll_path.push(0);
    let dll_bytes =
        unsafe { std::slice::from_raw_parts(dll_path.as_ptr().cast::<u8>(), dll_path.len() * 2) };
    let remote_dll_path = RemoteAllocation::write(process, dll_bytes)?;

    let remote_kernel32 = remote_module_base(process_id, "kernel32.dll")?;
    let local_kernel32 =
        unsafe { GetModuleHandleW(windows::core::w!("kernel32.dll")) }.map_err(|source| {
            InjectorError::Windows {
                context: "resolve local kernel32.dll",
                source,
            }
        })?;
    let local_load_library =
        unsafe { GetProcAddress(local_kernel32, PCSTR(c"LoadLibraryW".as_ptr().cast::<u8>())) }
            .ok_or(InjectorError::MissingExport("LoadLibraryW"))? as usize;
    let load_library_rva = local_load_library - local_kernel32.0 as usize;
    let remote_load_library = remote_kernel32 + load_library_rva;
    injector_log(
        LogLevel::Debug,
        format_args!(
            "calling remote LoadLibraryW kernel32=0x{remote_kernel32:08x} function=0x{remote_load_library:08x}"
        ),
    );

    let remote_module =
        unsafe { call_remote(process, remote_load_library, remote_dll_path.pointer())? };
    if remote_module == 0 {
        return Err(InjectorError::RemoteLoadFailed);
    }

    let local_dll = LocalModule::load_without_initialization(dll)?;
    let local_start = unsafe {
        GetProcAddress(
            local_dll.raw(),
            PCSTR(c"kg_capture_start".as_ptr().cast::<u8>()),
        )
    }
    .ok_or(InjectorError::MissingExport("kg_capture_start"))? as usize;
    let start_rva = local_start - local_dll.raw().0 as usize;
    let remote_start = remote_module as usize + start_rva;
    injector_log(
        LogLevel::Debug,
        format_args!(
            "hook loaded base=0x{remote_module:08x}; kg_capture_start=0x{remote_start:08x}"
        ),
    );

    let bootstrap_bytes = unsafe {
        std::slice::from_raw_parts(
            (bootstrap as *const HookBootstrap).cast::<u8>(),
            size_of::<HookBootstrap>(),
        )
    };
    let remote_bootstrap = RemoteAllocation::write(process, bootstrap_bytes)?;
    let start_result = unsafe { call_remote(process, remote_start, remote_bootstrap.pointer())? };
    if start_result != 0 {
        return Err(InjectorError::RemoteStartFailed(start_result));
    }

    let _ = writeln!(
        std::io::stdout(),
        "injected {} into process {process_id}",
        dll.display()
    );
    injector_log(
        LogLevel::Info,
        format_args!(
            "kg_capture_start succeeded dll={} pid={process_id}",
            dll.display()
        ),
    );
    Ok(())
}

fn injector_log(level: LogLevel, arguments: std::fmt::Arguments<'_>) {
    if level < configured_log_level() {
        return;
    }
    let Some(path) = env::var_os("KG_CAPTURE_INJECTOR_LOG_FILE") else {
        return;
    };
    if let Some(parent) = Path::new(&path).parent() {
        let _ = fs::create_dir_all(parent);
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

fn validate_x86_target(process: HANDLE) -> Result<(), InjectorError> {
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    unsafe { IsWow64Process2(process, &mut process_machine, Some(&mut native_machine)) }.map_err(
        |source| InjectorError::Windows {
            context: "query target architecture",
            source,
        },
    )?;

    let is_x86 = process_machine == IMAGE_FILE_MACHINE_I386
        || (process_machine == IMAGE_FILE_MACHINE_UNKNOWN
            && native_machine == IMAGE_FILE_MACHINE_I386);
    if !is_x86 {
        return Err(InjectorError::WrongTargetArchitecture {
            process_machine,
            native_machine,
        });
    }
    Ok(())
}

fn remote_module_base(process_id: u32, expected_name: &str) -> Result<usize, InjectorError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match try_remote_module_base(process_id, expected_name) {
            Ok(address) => return Ok(address),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn try_remote_module_base(process_id: u32, expected_name: &str) -> Result<usize, InjectorError> {
    let snapshot = OwnedHandle::new(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, process_id) }
            .map_err(|source| InjectorError::Windows {
                context: "snapshot target modules",
                source,
            })?,
    );

    let mut entry = MODULEENTRY32W {
        dwSize: size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe { Module32FirstW(snapshot.raw(), &mut entry) }.map_err(|source| {
        InjectorError::Windows {
            context: "enumerate target modules",
            source,
        }
    })?;

    loop {
        let length = entry
            .szModule
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(entry.szModule.len());
        let name = String::from_utf16_lossy(&entry.szModule[..length]);
        if name.eq_ignore_ascii_case(expected_name) {
            return Ok(entry.modBaseAddr as usize);
        }
        if unsafe { Module32NextW(snapshot.raw(), &mut entry) }.is_err() {
            break;
        }
    }

    Err(InjectorError::MissingRemoteModule(expected_name.into()))
}

unsafe fn call_remote(
    process: HANDLE,
    function_address: usize,
    parameter: *mut c_void,
) -> Result<u32, InjectorError> {
    let start_routine: LPTHREAD_START_ROUTINE = unsafe { transmute(function_address) };
    let thread = OwnedHandle::new(
        unsafe {
            CreateRemoteThread(
                process,
                None,
                0,
                start_routine,
                Some(parameter.cast_const()),
                0,
                None,
            )
        }
        .map_err(|source| InjectorError::Windows {
            context: "create remote thread",
            source,
        })?,
    );

    let wait = unsafe { WaitForSingleObject(thread.raw(), REMOTE_CALL_TIMEOUT_MS) };
    if wait == WAIT_TIMEOUT {
        return Err(InjectorError::RemoteCallTimeout);
    }
    if wait != WAIT_OBJECT_0 {
        return Err(InjectorError::Windows {
            context: "wait for remote thread",
            source: WindowsError::from_thread(),
        });
    }

    let mut exit_code = 0;
    unsafe { GetExitCodeThread(thread.raw(), &mut exit_code) }.map_err(|source| {
        InjectorError::Windows {
            context: "read remote thread result",
            source,
        }
    })?;
    Ok(exit_code)
}

struct RemoteAllocation {
    process: HANDLE,
    pointer: *mut c_void,
}

impl RemoteAllocation {
    fn write(process: HANDLE, bytes: &[u8]) -> Result<Self, InjectorError> {
        let pointer = unsafe {
            VirtualAllocEx(
                process,
                None,
                bytes.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if pointer.is_null() {
            return Err(InjectorError::Windows {
                context: "allocate target memory",
                source: WindowsError::from_thread(),
            });
        }

        if let Err(source) = unsafe {
            WriteProcessMemory(process, pointer, bytes.as_ptr().cast(), bytes.len(), None)
        } {
            unsafe {
                let _ = VirtualFreeEx(process, pointer, 0, MEM_RELEASE);
            }
            return Err(InjectorError::Windows {
                context: "write target memory",
                source,
            });
        }

        Ok(Self { process, pointer })
    }

    fn pointer(&self) -> *mut c_void {
        self.pointer
    }
}

impl Drop for RemoteAllocation {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFreeEx(self.process, self.pointer, 0, MEM_RELEASE);
        }
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct LocalModule(HMODULE);

impl LocalModule {
    fn load_without_initialization(path: &Path) -> Result<Self, InjectorError> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let module =
            unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, DONT_RESOLVE_DLL_REFERENCES) }
                .map_err(|source| InjectorError::Windows {
                    context: "inspect hook DLL exports",
                    source,
                })?;
        Ok(Self(module))
    }

    fn raw(&self) -> HMODULE {
        self.0
    }
}

impl Drop for LocalModule {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

struct Arguments {
    target: Target,
    dll: PathBuf,
    endpoint: String,
    nonce: SessionNonce,
    pid_file: Option<PathBuf>,
    launch_arguments: Vec<String>,
    hook_log: PathBuf,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, InjectorError> {
        let values: Vec<String> = arguments.collect();
        let value = |name: &'static str| -> Result<&str, InjectorError> {
            let position = values
                .iter()
                .position(|argument| argument == name)
                .ok_or(InjectorError::MissingArgument(name))?;
            values
                .get(position + 1)
                .map(String::as_str)
                .ok_or(InjectorError::MissingArgument(name))
        };

        let optional_value = |name: &str| -> Option<&str> {
            values
                .iter()
                .position(|argument| argument == name)
                .and_then(|position| values.get(position + 1))
                .map(String::as_str)
        };
        let repeated_values = |name: &str| -> Vec<String> {
            values
                .iter()
                .enumerate()
                .filter(|(_, argument)| argument.as_str() == name)
                .filter_map(|(position, _)| values.get(position + 1))
                .cloned()
                .collect()
        };
        let target = if let Some(executable) = optional_value("--launch") {
            Target::Launch(PathBuf::from(executable))
        } else {
            let process_id = value("--pid")?
                .parse()
                .map_err(|_| InjectorError::InvalidArgument("--pid"))?;
            Target::Existing(process_id)
        };
        let nonce = parse_nonce(value("--nonce")?)?;
        Ok(Self {
            target,
            dll: PathBuf::from(value("--dll")?),
            endpoint: value("--ipc")?.to_owned(),
            nonce,
            pid_file: optional_value("--pid-file").map(PathBuf::from),
            launch_arguments: repeated_values("--launch-arg"),
            hook_log: optional_value("--hook-log")
                .map(PathBuf::from)
                .unwrap_or_default(),
        })
    }
}

enum Target {
    Existing(u32),
    Launch(PathBuf),
}

struct LaunchedProcess {
    process: OwnedHandle,
    primary_thread: OwnedHandle,
    process_id: u32,
    keep_running: bool,
}

impl LaunchedProcess {
    fn suspended(executable: &Path, arguments: &[String]) -> Result<Self, InjectorError> {
        let executable = absolute_path(executable)?;
        if !executable.is_file() {
            return Err(InjectorError::Io {
                context: "inspect target executable",
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} is not a file", executable.display()),
                ),
            });
        }
        let mut executable_wide: Vec<u16> = executable.as_os_str().encode_wide().collect();
        executable_wide.push(0);
        let mut command_line = launch_command_line(&executable, arguments);
        let current_directory = executable
            .parent()
            .ok_or(InjectorError::TargetHasNoDirectory)?;
        let mut current_directory_wide: Vec<u16> =
            current_directory.as_os_str().encode_wide().collect();
        current_directory_wide.push(0);

        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut information = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR(executable_wide.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_SUSPENDED,
                None,
                PCWSTR(current_directory_wide.as_ptr()),
                &startup,
                &mut information,
            )
        }
        .map_err(|source| InjectorError::Windows {
            context: "launch target process suspended",
            source,
        })?;

        Ok(Self {
            process: OwnedHandle::new(information.hProcess),
            primary_thread: OwnedHandle::new(information.hThread),
            process_id: information.dwProcessId,
            keep_running: false,
        })
    }

    fn start(&mut self) -> Result<(), InjectorError> {
        let previous_count = unsafe { ResumeThread(self.primary_thread.raw()) };
        if previous_count == u32::MAX {
            return Err(InjectorError::Windows {
                context: "resume target process",
                source: WindowsError::from_thread(),
            });
        }
        Ok(())
    }

    fn run_loader_to_entrypoint(&mut self, entrypoint: usize) -> Result<(), InjectorError> {
        injector_log(
            LogLevel::Debug,
            format_args!("entry-point gate armed at 0x{entrypoint:08x}; resuming Windows loader"),
        );
        self.start()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            thread::sleep(Duration::from_millis(1));
            let previous_count = unsafe { SuspendThread(self.primary_thread.raw()) };
            if previous_count == u32::MAX {
                return Err(InjectorError::Windows {
                    context: "suspend target at entry point",
                    source: WindowsError::from_thread(),
                });
            }

            let mut context = CONTEXT {
                ContextFlags: CONTEXT_CONTROL_X86,
                ..Default::default()
            };
            if let Err(source) = unsafe {
                GetThreadContext(self.primary_thread.raw(), std::ptr::addr_of_mut!(context))
            } {
                let _ = unsafe { ResumeThread(self.primary_thread.raw()) };
                return Err(InjectorError::Windows {
                    context: "read target instruction pointer",
                    source,
                });
            }
            if context.Eip as usize == entrypoint {
                injector_log(
                    LogLevel::Debug,
                    format_args!(
                        "Windows loader complete; primary thread stopped at 0x{entrypoint:08x}"
                    ),
                );
                return Ok(());
            }

            let resumed_count = unsafe { ResumeThread(self.primary_thread.raw()) };
            if resumed_count == u32::MAX {
                return Err(InjectorError::Windows {
                    context: "resume target while waiting for entry point",
                    source: WindowsError::from_thread(),
                });
            }
            if Instant::now() >= deadline {
                return Err(InjectorError::EntrypointTimeout(entrypoint));
            }
        }
    }

    fn keep_running(&mut self) {
        self.keep_running = true;
    }
}

struct EntrypointGate {
    process: HANDLE,
    address: usize,
    original: [u8; 2],
    restored: bool,
}

impl EntrypointGate {
    fn arm(process: HANDLE) -> Result<Self, InjectorError> {
        let image_base = process_image_base(process)?;
        let dos_magic: u16 = read_remote_value(process, image_base)?;
        if dos_magic != 0x5a4d {
            return Err(InjectorError::InvalidTargetImage("missing MZ header"));
        }
        let pe_offset: u32 = read_remote_value(process, image_base + 0x3c)?;
        let pe_header = image_base + pe_offset as usize;
        let pe_signature: u32 = read_remote_value(process, pe_header)?;
        if pe_signature != 0x0000_4550 {
            return Err(InjectorError::InvalidTargetImage("missing PE signature"));
        }
        let optional_magic: u16 = read_remote_value(process, pe_header + 24)?;
        if optional_magic != 0x010b {
            return Err(InjectorError::InvalidTargetImage("target is not PE32"));
        }
        let entrypoint_rva: u32 = read_remote_value(process, pe_header + 40)?;
        let address = image_base + entrypoint_rva as usize;
        let original: [u8; 2] = read_remote_value(process, address)?;
        write_remote_code(process, address, &[0xeb, 0xfe])?;
        injector_log(
            LogLevel::Debug,
            format_args!(
                "patched target entry point image=0x{image_base:08x} rva=0x{entrypoint_rva:08x} original={:02x}{:02x}",
                original[0], original[1]
            ),
        );
        Ok(Self {
            process,
            address,
            original,
            restored: false,
        })
    }

    fn address(&self) -> usize {
        self.address
    }

    fn restore(&mut self) -> Result<(), InjectorError> {
        write_remote_code(self.process, self.address, &self.original)?;
        self.restored = true;
        injector_log(
            LogLevel::Debug,
            format_args!("restored target entry point at 0x{:08x}", self.address),
        );
        Ok(())
    }
}

fn process_image_base(process: HANDLE) -> Result<usize, InjectorError> {
    type NtQueryInformationProcess =
        unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

    let ntdll = unsafe { GetModuleHandleW(windows::core::w!("ntdll.dll")) }.map_err(|source| {
        InjectorError::Windows {
            context: "resolve local ntdll.dll",
            source,
        }
    })?;
    let address = unsafe {
        GetProcAddress(
            ntdll,
            PCSTR(c"NtQueryInformationProcess".as_ptr().cast::<u8>()),
        )
    }
    .ok_or(InjectorError::MissingExport("NtQueryInformationProcess"))?;
    let query: NtQueryInformationProcess = unsafe { transmute(address) };
    let mut information = PROCESS_BASIC_INFORMATION::default();
    let status = unsafe {
        query(
            process,
            0,
            std::ptr::addr_of_mut!(information).cast(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        return Err(InjectorError::Native {
            context: "query target PEB",
            status,
        });
    }
    let peb = information.PebBaseAddress as usize;
    let image_base: usize = read_remote_value(process, peb + 8)?;
    if image_base == 0 {
        return Err(InjectorError::InvalidTargetImage("PEB image base is null"));
    }
    Ok(image_base)
}

fn read_remote_value<T: Copy>(process: HANDLE, address: usize) -> Result<T, InjectorError> {
    let mut value = MaybeUninit::<T>::uninit();
    unsafe {
        ReadProcessMemory(
            process,
            address as *const c_void,
            value.as_mut_ptr().cast(),
            size_of::<T>(),
            None,
        )
    }
    .map_err(|source| InjectorError::Windows {
        context: "read target image",
        source,
    })?;
    Ok(unsafe { value.assume_init() })
}

fn write_remote_code(process: HANDLE, address: usize, bytes: &[u8]) -> Result<(), InjectorError> {
    let mut old_protection = PAGE_PROTECTION_FLAGS::default();
    unsafe {
        VirtualProtectEx(
            process,
            address as *const c_void,
            bytes.len(),
            PAGE_EXECUTE_READWRITE,
            std::ptr::addr_of_mut!(old_protection),
        )
    }
    .map_err(|source| InjectorError::Windows {
        context: "make target entry point writable",
        source,
    })?;
    let write_result = unsafe {
        WriteProcessMemory(
            process,
            address as *mut c_void,
            bytes.as_ptr().cast(),
            bytes.len(),
            None,
        )
    };
    let mut ignored = PAGE_PROTECTION_FLAGS::default();
    let restore_result = unsafe {
        VirtualProtectEx(
            process,
            address as *const c_void,
            bytes.len(),
            old_protection,
            std::ptr::addr_of_mut!(ignored),
        )
    };
    write_result.map_err(|source| InjectorError::Windows {
        context: "patch target entry point",
        source,
    })?;
    restore_result.map_err(|source| InjectorError::Windows {
        context: "restore target entry-point protection",
        source,
    })?;
    unsafe { FlushInstructionCache(process, Some(address as *const c_void), bytes.len()) }
        .map_err(|source| InjectorError::Windows {
            context: "flush target instruction cache",
            source,
        })?;
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, InjectorError> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|source| InjectorError::Io {
                context: "resolve target executable",
                source,
            })
    }
}

fn launch_command_line(executable: &Path, arguments: &[String]) -> Vec<u16> {
    let mut command_line = quote_windows_argument(&executable.to_string_lossy());
    for argument in arguments {
        command_line.push(' ');
        command_line.push_str(&quote_windows_argument(argument));
    }
    command_line.encode_utf16().chain([0]).collect()
}

fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(character);
            backslashes = 0;
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

impl Drop for LaunchedProcess {
    fn drop(&mut self) {
        if !self.keep_running {
            unsafe {
                let _ = TerminateProcess(self.process.raw(), 1);
            }
        }
    }
}

fn parse_nonce(value: &str) -> Result<SessionNonce, InjectorError> {
    if value.len() != 32 {
        return Err(InjectorError::InvalidArgument("--nonce"));
    }
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| InjectorError::InvalidArgument("--nonce"))?;
    }
    Ok(SessionNonce(bytes))
}

#[derive(Debug, Error)]
enum InjectorError {
    #[error("this executable must be built for i686-pc-windows-msvc")]
    WrongInjectorArchitecture,
    #[error(
        "target is not an x86 process (process={process_machine:?}, native={native_machine:?})"
    )]
    WrongTargetArchitecture {
        process_machine: IMAGE_FILE_MACHINE,
        native_machine: IMAGE_FILE_MACHINE,
    },
    #[error("missing required argument {0}")]
    MissingArgument(&'static str),
    #[error("invalid value for argument {0}")]
    InvalidArgument(&'static str),
    #[error("target executable has no parent directory")]
    TargetHasNoDirectory,
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{context}: {source}")]
    Windows {
        context: &'static str,
        #[source]
        source: WindowsError,
    },
    #[error("invalid bootstrap: {0}")]
    Bootstrap(String),
    #[error("target module not found: {0}")]
    MissingRemoteModule(String),
    #[error("export not found: {0}")]
    MissingExport(&'static str),
    #[error("LoadLibraryW returned NULL in the target process")]
    RemoteLoadFailed,
    #[error("remote call timed out")]
    RemoteCallTimeout,
    #[error("target did not reach entry point 0x{0:08x} before timeout")]
    EntrypointTimeout(usize),
    #[error("invalid target image: {0}")]
    InvalidTargetImage(&'static str),
    #[error("{context}: NTSTATUS 0x{status:08x}")]
    Native { context: &'static str, status: i32 },
    #[error("kg_capture_start failed with code {0}")]
    RemoteStartFailed(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_parser_accepts_hex() {
        assert_eq!(
            parse_nonce("000102030405060708090a0b0c0d0e0f").unwrap(),
            SessionNonce([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
        );
    }

    #[test]
    fn launch_command_line_matches_windows_quoting_rules() {
        let command_line = launch_command_line(
            Path::new(r"C:\Program Files (x86)\Tencent\WeSing\WeSing.exe"),
            &["/DeskTop".into(), "argument with spaces".into()],
        );
        let command_line = String::from_utf16(&command_line[..command_line.len() - 1]).unwrap();
        assert_eq!(
            command_line,
            r#""C:\Program Files (x86)\Tencent\WeSing\WeSing.exe" /DeskTop "argument with spaces""#
        );
    }
}
