//! x86 DLL loaded into WeSing. It extracts lyric model state rather than pixels.

use std::cell::Cell;
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::mem::{size_of, transmute};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ipc_channel::ipc::{self, IpcSender};
use kg_capture_protocol::{
    HookBootstrap, HookEvent, HookHandshake, HookHello, HostCommand, LyricLine, LyricSource,
    LyricTimeline, LyricWord, PROTOCOL_VERSION, PlaybackPosition,
};
use retour::GenericDetour;
use windows::Win32::Foundation::{HINSTANCE, TRUE};
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::core::BOOL;

type RenderUpdateFn = unsafe extern "system" fn(*mut c_void, f32) -> i32;

const STANDARD_RTTI: &[u8] = b".?AVCLyricRenderWnd@@\0";
const LIVE_RTTI: &[u8] = b".?AVCLyricRenderWndForLiveShow@@\0";
const UPDATE_PROLOGUE: &[u8] = &[0x55, 0x8b, 0xec, 0x83, 0xe4, 0xf8, 0x83, 0xec, 0x54, 0x53];
const MAX_LINES: usize = 2_000;
const MAX_WORDS_PER_LINE: usize = 256;
const MAX_WORD_UTF16: usize = 1_024;

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

static STANDARD_UPDATE: OnceLock<GenericDetour<RenderUpdateFn>> = OnceLock::new();
static LIVE_UPDATE: OnceLock<GenericDetour<RenderUpdateFn>> = OnceLock::new();
static EVENT_QUEUE: OnceLock<SyncSender<HookEvent>> = OnceLock::new();
static CAPTURE_STATE: OnceLock<Mutex<CaptureState>> = OnceLock::new();
static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);
static NEXT_TIMELINE_ID: AtomicU64 = AtomicU64::new(1);
static FIXTURE_TIMELINE_SENT: AtomicBool = AtomicBool::new(false);
static STANDARD_CALLBACKS: AtomicU64 = AtomicU64::new(0);
static LIVE_CALLBACKS: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_FAILURES: AtomicU64 = AtomicU64::new(0);
static QUEUE_FAILURES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(instance: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let _ = DisableThreadLibraryCalls(instance.into());
        }
    }
    TRUE
}

/// Starts IPC initialization outside `DllMain`.
///
/// # Safety
/// `parameter` must point to a readable [`HookBootstrap`] in this process.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn kg_capture_start(parameter: *mut c_void) -> u32 {
    if parameter.is_null() {
        return 1;
    }
    let bootstrap = unsafe { parameter.cast::<HookBootstrap>().read() };
    if let Ok(path) = bootstrap.log_path()
        && !path.is_empty()
    {
        initialize_log(path);
    }
    hook_log(
        LogLevel::Info,
        format_args!(
            "kg_capture_start pid={} protocol={} expected={} bootstrap_size={}",
            std::process::id(),
            bootstrap.protocol_version,
            PROTOCOL_VERSION,
            size_of::<HookBootstrap>()
        ),
    );
    if bootstrap.protocol_version != PROTOCOL_VERSION {
        hook_log(LogLevel::Error, format_args!("protocol mismatch"));
        return 2;
    }
    let endpoint = match bootstrap.endpoint() {
        Ok(value) => value.to_owned(),
        Err(error) => {
            hook_log(
                LogLevel::Error,
                format_args!("invalid IPC endpoint: {error}"),
            );
            return 3;
        }
    };
    let nonce = bootstrap.session_nonce;
    match thread::Builder::new()
        .name("kg-capture-hook".into())
        .spawn(move || run_hook(endpoint, nonce))
    {
        Ok(_) => 0,
        Err(error) => {
            hook_log(LogLevel::Error, format_args!("start hook thread: {error}"));
            4
        }
    }
}

/// Semantic fixture entry point used only by `cargo xtask smoke`.
#[unsafe(no_mangle)]
pub extern "system" fn kg_capture_fixture_emit(position_ms: u32) -> u32 {
    if std::env::var_os("KG_CAPTURE_FIXTURE").is_none() || !HOOKS_ACTIVE.load(Ordering::Acquire) {
        return 1;
    }
    emit_fixture(position_ms as f32);
    0
}

fn run_hook(endpoint: String, nonce: kg_capture_protocol::SessionNonce) {
    hook_log(
        LogLevel::Debug,
        format_args!("connecting IPC endpoint_len={}", endpoint.len()),
    );
    let bootstrap = match IpcSender::<HookHandshake>::connect(endpoint) {
        Ok(sender) => sender,
        Err(error) => {
            hook_log(
                LogLevel::Error,
                format_args!("connect bootstrap IPC: {error}"),
            );
            return;
        }
    };
    let (command_sender, command_receiver) = match ipc::channel::<HostCommand>() {
        Ok(channel) => channel,
        Err(_) => return,
    };
    let (event_sender, event_receiver) = match ipc::channel::<HookEvent>() {
        Ok(channel) => channel,
        Err(_) => return,
    };
    let (queue_sender, queue_receiver) = mpsc::sync_channel::<HookEvent>(64);
    if EVENT_QUEUE.set(queue_sender).is_err() {
        let _ = event_sender.send(HookEvent::Error("hook DLL was initialized twice".into()));
        return;
    }
    let worker_sender = event_sender.clone();
    if thread::Builder::new()
        .name("kg-capture-events".into())
        .spawn(move || {
            while let Ok(event) = queue_receiver.recv() {
                if worker_sender.send(event).is_err() {
                    break;
                }
            }
        })
        .is_err()
    {
        let _ = event_sender.send(HookEvent::Error("could not start event worker".into()));
        return;
    }
    let _ = CAPTURE_STATE.set(Mutex::new(CaptureState::default()));

    let handshake = HookHandshake {
        hello: HookHello {
            protocol_version: PROTOCOL_VERSION,
            process_id: std::process::id(),
            session_nonce: nonce,
        },
        command_sender,
        event_receiver,
    };
    if bootstrap.send(handshake).is_err() {
        hook_log(LogLevel::Error, format_args!("send IPC handshake failed"));
        return;
    }
    hook_log(LogLevel::Info, format_args!("IPC handshake sent"));
    command_loop(command_receiver, event_sender);
    hook_log(LogLevel::Info, format_args!("command loop stopped"));
}

fn command_loop(
    receiver: ipc_channel::ipc::IpcReceiver<HostCommand>,
    sender: ipc_channel::ipc::IpcSender<HookEvent>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            HostCommand::StartCapture => match install_hooks() {
                Ok(warning) => {
                    HOOKS_ACTIVE.store(true, Ordering::Release);
                    hook_log(LogLevel::Info, format_args!("semantic capture started"));
                    if let Some(warning) = warning {
                        let _ = sender.send(HookEvent::Warning(warning));
                    }
                    let _ = sender.send(HookEvent::CaptureStarted);
                }
                Err(error) => {
                    hook_log(LogLevel::Error, format_args!("install hooks: {error}"));
                    let _ = sender.send(HookEvent::Error(error));
                }
            },
            HostCommand::StopCapture => {
                hook_log(LogLevel::Info, format_args!("stop requested"));
                HOOKS_ACTIVE.store(false, Ordering::Release);
                disable_hooks();
                let _ = sender.send(HookEvent::CaptureStopped);
            }
            HostCommand::Ping { sequence } => {
                let _ = sender.send(HookEvent::Pong { sequence });
            }
            HostCommand::Shutdown => {
                hook_log(LogLevel::Info, format_args!("shutdown requested"));
                HOOKS_ACTIVE.store(false, Ordering::Release);
                disable_hooks();
                break;
            }
        }
    }
    HOOKS_ACTIVE.store(false, Ordering::Release);
    disable_hooks();
}

fn install_hooks() -> Result<Option<String>, String> {
    if std::env::var_os("KG_CAPTURE_FIXTURE").is_some() {
        hook_log(LogLevel::Info, format_args!("fixture mode selected"));
        FIXTURE_TIMELINE_SENT.store(false, Ordering::Release);
        return Ok(Some("semantic fixture mode".into()));
    }

    hook_log(LogLevel::Info, format_args!("waiting for KSongsUI.dll"));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let module = loop {
        if let Ok(module) = unsafe { GetModuleHandleW(windows::core::w!("KSongsUI.dll")) } {
            break module;
        }
        if std::time::Instant::now() >= deadline {
            return Err(
                "KSongsUI.dll was not loaded; open a lyrics view and start synchronization again"
                    .into(),
            );
        }
        thread::sleep(Duration::from_millis(50));
    };

    hook_log(
        LogLevel::Debug,
        format_args!("KSongsUI.dll base=0x{:08x}", module.0 as usize),
    );
    let image = unsafe { PeImage::from_module(module.0 as usize) }?;
    hook_log(
        LogLevel::Debug,
        format_args!(
            "PE image size=0x{:x} sections={}",
            image.size,
            image.sections.len()
        ),
    );
    image.log_sections();
    let standard = unsafe { image.find_virtual_method(STANDARD_RTTI, 4) }?;
    let live = unsafe { image.find_virtual_method(LIVE_RTTI, 4) }?;
    hook_log(
        LogLevel::Debug,
        format_args!("lyric update targets standard=0x{standard:08x} live=0x{live:08x}"),
    );
    unsafe {
        verify_update_target(standard)?;
        verify_update_target(live)?;
    }

    if STANDARD_UPDATE.get().is_none() {
        let detour = unsafe {
            GenericDetour::new(
                transmute::<usize, RenderUpdateFn>(standard),
                standard_update_hook,
            )
        }
        .map_err(|error| format!("create standard lyric detour: {error}"))?;
        STANDARD_UPDATE
            .set(detour)
            .map_err(|_| "standard lyric detour initialized concurrently".to_owned())?;
    }
    if LIVE_UPDATE.get().is_none() {
        let detour = unsafe {
            GenericDetour::new(transmute::<usize, RenderUpdateFn>(live), live_update_hook)
        }
        .map_err(|error| format!("create live-show lyric detour: {error}"))?;
        LIVE_UPDATE
            .set(detour)
            .map_err(|_| "live lyric detour initialized concurrently".to_owned())?;
    }
    for (name, detour) in [
        ("standard", STANDARD_UPDATE.get()),
        ("live-show", LIVE_UPDATE.get()),
    ] {
        if let Some(detour) = detour
            && !detour.is_enabled()
        {
            unsafe { detour.enable() }
                .map_err(|error| format!("enable {name} lyric detour: {error}"))?;
            hook_log(LogLevel::Info, format_args!("{name} lyric detour enabled"));
        }
    }
    Ok(None)
}

fn disable_hooks() {
    for detour in [STANDARD_UPDATE.get(), LIVE_UPDATE.get()]
        .into_iter()
        .flatten()
    {
        if detour.is_enabled() {
            let _ = unsafe { detour.disable() };
        }
    }
}

unsafe extern "system" fn standard_update_hook(object: *mut c_void, position: f32) -> i32 {
    log_callback("standard", &STANDARD_CALLBACKS, object, position);
    let result = unsafe {
        STANDARD_UPDATE
            .get()
            .expect("standard detour initialized")
            .call(object, position)
    };
    capture_render_state(
        object,
        position,
        RenderLayout::STANDARD,
        LyricSource::Standard,
    );
    result
}

unsafe extern "system" fn live_update_hook(object: *mut c_void, position: f32) -> i32 {
    log_callback("live-show", &LIVE_CALLBACKS, object, position);
    let result = unsafe {
        LIVE_UPDATE
            .get()
            .expect("live-show detour initialized")
            .call(object, position)
    };
    capture_render_state(object, position, RenderLayout::LIVE, LyricSource::LiveShow);
    result
}

fn capture_render_state(
    object: *mut c_void,
    position_ms: f32,
    layout: RenderLayout,
    source: LyricSource,
) {
    if !HOOKS_ACTIVE.load(Ordering::Acquire) || object.is_null() || !position_ms.is_finite() {
        return;
    }
    IN_HOOK.with(|inside| {
        if inside.replace(true) {
            return;
        }
        let _reset = ResetCell(inside);
        let Some(state) = CAPTURE_STATE.get() else {
            return;
        };
        let Ok(mut state) = state.try_lock() else {
            return;
        };
        let Some(snapshot) = (unsafe { read_snapshot(object.cast(), layout, source) }) else {
            let failures = SNAPSHOT_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            if failures <= 5 || failures.is_multiple_of(1_000) {
                hook_log(
                    LogLevel::Warn,
                    format_args!(
                        "snapshot read failed count={failures} source={source:?} object=0x{:08x}",
                        object as usize
                    ),
                );
            }
            return;
        };

        let identity = (source, snapshot.lines_begin, snapshot.lines_end);
        if state.identity != Some(identity) {
            hook_log(
                LogLevel::Debug,
                format_args!(
                    "timeline identity changed source={source:?} begin=0x{:08x} end=0x{:08x} entries={}",
                    snapshot.lines_begin,
                    snapshot.lines_end,
                    snapshot.lines_end.saturating_sub(snapshot.lines_begin) / 4
                ),
            );
            let id = NEXT_TIMELINE_ID.fetch_add(1, Ordering::Relaxed);
            if let Some(lines) = unsafe { read_timeline(snapshot.lines_begin, snapshot.lines_end) }
                && !lines.is_empty()
            {
                state.identity = Some(identity);
                state.timeline_id = id;
                hook_log(
                    LogLevel::Info,
                    format_args!("timeline extracted id={id} lines={}", lines.len()),
                );
                queue(HookEvent::Timeline(LyricTimeline { id, source, lines }));
            } else {
                hook_log(
                    LogLevel::Warn,
                    format_args!("timeline extraction returned no lines"),
                );
            }
        }
        if state.timeline_id == 0 {
            return;
        }
        let current_line = u32::try_from(snapshot.current_line)
            .ok()
            .filter(|_| snapshot.current_line >= 0);
        queue(HookEvent::Playback(PlaybackPosition {
            timeline_id: state.timeline_id,
            observed_at_micros: timestamp_micros(),
            position_ms,
            current_line,
            line_progress: snapshot.line_progress.clamp(0.0, 1.0),
        }));
    });
}

fn emit_fixture(position_ms: f32) {
    const DURATION: f32 = 2_400.0;
    let lines = fixture_lines();
    if !FIXTURE_TIMELINE_SENT.swap(true, Ordering::AcqRel) {
        queue(HookEvent::Timeline(LyricTimeline {
            id: 1,
            source: LyricSource::Fixture,
            lines: lines.clone(),
        }));
    }
    let total = DURATION * lines.len() as f32;
    let position = position_ms % total;
    let line = (position / DURATION).floor() as u32;
    queue(HookEvent::Playback(PlaybackPosition {
        timeline_id: 1,
        observed_at_micros: timestamp_micros(),
        position_ms: position,
        current_line: Some(line),
        line_progress: (position % DURATION) / DURATION,
    }));
}

fn fixture_lines() -> Vec<LyricLine> {
    [
        ["把爱", "留在", "身边"].as_slice(),
        ["窗外", "有个", "蓝蓝的天"].as_slice(),
        ["落叶", "那一瞬间", "记得"].as_slice(),
    ]
    .iter()
    .enumerate()
    .map(|(line_index, words)| {
        let start = line_index as f32 * 2_400.0;
        let duration = 2_400.0 / words.len() as f32;
        LyricLine {
            index: line_index as u32,
            text: words.concat(),
            start_ms: start,
            duration_ms: 2_400.0,
            words: words
                .iter()
                .enumerate()
                .map(|(word_index, text)| LyricWord {
                    text: (*text).into(),
                    start_ms: start + duration * word_index as f32,
                    duration_ms: duration,
                })
                .collect(),
        }
    })
    .collect()
}

fn queue(event: HookEvent) {
    if let Some(sender) = EVENT_QUEUE.get()
        && let Err(error) = sender.try_send(event)
    {
        let failures = QUEUE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
        if failures <= 5 || failures.is_multiple_of(1_000) {
            hook_log(
                LogLevel::Warn,
                format_args!("event queue send failed count={failures}: {error}"),
            );
        }
    }
}

fn log_callback(name: &str, counter: &AtomicU64, object: *mut c_void, position: f32) {
    let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 3 || count.is_multiple_of(1_000) {
        hook_log(
            LogLevel::Debug,
            format_args!(
                "{name} callback count={count} object=0x{:08x} position={position}",
                object as usize
            ),
        );
    }
}

fn initialize_log(path: &str) {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
}

fn hook_log(level: LogLevel, arguments: std::fmt::Arguments<'_>) {
    if level < configured_log_level() {
        return;
    }
    let Some(file) = LOG_FILE.get() else {
        return;
    };
    let Ok(mut file) = file.lock() else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let _ = writeln!(
        file,
        "{timestamp} {} pid={} tid={:?} {arguments}",
        level.label(),
        std::process::id(),
        thread::current().id()
    );
    let _ = file.flush();
}

fn configured_log_level() -> LogLevel {
    static LEVEL: OnceLock<LogLevel> = OnceLock::new();
    *LEVEL.get_or_init(|| minimum_log_level(std::env::var("RUST_LOG").ok().as_deref()))
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

fn timestamp_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct CaptureState {
    identity: Option<(LyricSource, usize, usize)>,
    timeline_id: u64,
}

struct ResetCell<'a>(&'a Cell<bool>);

impl Drop for ResetCell<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[derive(Clone, Copy)]
struct RenderLayout {
    current_line: usize,
    line_progress: usize,
    lines_begin: usize,
    lines_end: usize,
}

impl RenderLayout {
    const STANDARD: Self = Self {
        current_line: 0x2c,
        line_progress: 0x30,
        lines_begin: 0xb8,
        lines_end: 0xbc,
    };
    const LIVE: Self = Self {
        current_line: 0x100,
        line_progress: 0x104,
        lines_begin: 0x190,
        lines_end: 0x194,
    };
}

struct RenderSnapshot {
    current_line: i32,
    line_progress: f32,
    lines_begin: usize,
    lines_end: usize,
}

unsafe fn read_snapshot(
    object: *const u8,
    layout: RenderLayout,
    _source: LyricSource,
) -> Option<RenderSnapshot> {
    let required = layout.lines_end.checked_add(size_of::<usize>())?;
    if !readable_range(object as usize, required) {
        return None;
    }
    let current_line = unsafe { read_at::<i32>(object, layout.current_line) };
    let line_progress = unsafe { read_at::<f32>(object, layout.line_progress) };
    let lines_begin = unsafe { read_at::<usize>(object, layout.lines_begin) };
    let lines_end = unsafe { read_at::<usize>(object, layout.lines_end) };
    if !line_progress.is_finite() || lines_end < lines_begin {
        return None;
    }
    Some(RenderSnapshot {
        current_line,
        line_progress,
        lines_begin,
        lines_end,
    })
}

unsafe fn read_timeline(begin: usize, end: usize) -> Option<Vec<LyricLine>> {
    let byte_length = end.checked_sub(begin)?;
    if !byte_length.is_multiple_of(size_of::<usize>()) {
        return None;
    }
    let count = byte_length / size_of::<usize>();
    if count == 0 || count > MAX_LINES || !readable_range(begin, byte_length) {
        return None;
    }
    let mut lines = Vec::with_capacity(count);
    let mut rejected = 0usize;
    for index in 0..count {
        let line_pointer = unsafe { ptr::read_unaligned((begin + index * 4) as *const usize) };
        let Some(line) = (unsafe { read_line(line_pointer, index as u32) }) else {
            rejected += 1;
            continue;
        };
        if !line.text.is_empty() {
            lines.push(line);
        }
    }
    hook_log(
        LogLevel::Debug,
        format_args!(
            "timeline memory parsed entries={count} accepted={} rejected={rejected}",
            lines.len()
        ),
    );
    Some(lines)
}

unsafe fn read_line(address: usize, index: u32) -> Option<LyricLine> {
    const LINE_HEADER: usize = 0x10;
    if address == 0 || !readable_range(address, LINE_HEADER) {
        return None;
    }
    let line = address as *const u8;
    let start_ms = unsafe { read_at::<f32>(line, 0) };
    let duration_ms = unsafe { read_at::<f32>(line, 4) };
    let words_begin = unsafe { read_at::<usize>(line, 8) };
    let words_end = unsafe { read_at::<usize>(line, 12) };
    if !valid_time(start_ms) || !valid_time(duration_ms) || words_end < words_begin {
        return None;
    }
    let byte_length = words_end.checked_sub(words_begin)?;
    if !byte_length.is_multiple_of(4) {
        return None;
    }
    let word_count = byte_length / 4;
    if word_count > MAX_WORDS_PER_LINE || !readable_range(words_begin, byte_length) {
        return None;
    }
    let mut words = Vec::with_capacity(word_count);
    for word_index in 0..word_count {
        let word_pointer =
            unsafe { ptr::read_unaligned((words_begin + word_index * 4) as *const usize) };
        if let Some(word) = unsafe { read_word(word_pointer) } {
            words.push(word);
        }
    }
    let text = words.iter().map(|word| word.text.as_str()).collect();
    Some(LyricLine {
        index,
        text,
        start_ms,
        duration_ms,
        words,
    })
}

unsafe fn read_word(address: usize) -> Option<LyricWord> {
    if address == 0 || !readable_range(address, 12) {
        return None;
    }
    let word = address as *const u8;
    let text_pointer = unsafe { read_at::<usize>(word, 0) };
    let start_ms = unsafe { read_at::<f32>(word, 4) };
    let duration_ms = unsafe { read_at::<f32>(word, 8) };
    if !valid_time(start_ms) || !valid_time(duration_ms) {
        return None;
    }
    let text = unsafe { read_utf16(text_pointer) }?;
    Some(LyricWord {
        text,
        start_ms,
        duration_ms,
    })
}

unsafe fn read_utf16(address: usize) -> Option<String> {
    if address == 0 {
        return None;
    }
    let readable = readable_prefix(address)?.min(MAX_WORD_UTF16 * 2);
    let units = readable / 2;
    if units == 0 {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(address as *const u16, units) };
    let length = slice.iter().position(|unit| *unit == 0).unwrap_or(units);
    if length == 0 || length == units {
        return None;
    }
    String::from_utf16(&slice[..length]).ok()
}

fn valid_time(value: f32) -> bool {
    value.is_finite() && value.abs() <= 100_000_000.0
}

unsafe fn read_at<T: Copy>(base: *const u8, offset: usize) -> T {
    unsafe { ptr::read_unaligned(base.add(offset).cast::<T>()) }
}

fn readable_range(address: usize, length: usize) -> bool {
    if length == 0 {
        return true;
    }
    readable_prefix(address).is_some_and(|available| available >= length)
}

fn readable_prefix(address: usize) -> Option<usize> {
    let mut information = MEMORY_BASIC_INFORMATION::default();
    let written = unsafe {
        VirtualQuery(
            Some(address as *const c_void),
            &mut information,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if written == 0 || information.State != MEM_COMMIT {
        return None;
    }
    let protection = information.Protect.0;
    if protection & PAGE_GUARD.0 != 0 || protection & PAGE_NOACCESS.0 != 0 {
        return None;
    }
    let base_protection = protection & 0xff;
    let readable = [
        PAGE_READONLY.0,
        PAGE_READWRITE.0,
        PAGE_WRITECOPY.0,
        PAGE_EXECUTE_READ.0,
        PAGE_EXECUTE_READWRITE.0,
        PAGE_EXECUTE_WRITECOPY.0,
    ]
    .contains(&base_protection);
    if !readable {
        return None;
    }
    let base = information.BaseAddress as usize;
    let end = base.checked_add(information.RegionSize)?;
    (address >= base && address < end).then_some(end - address)
}

unsafe fn verify_update_target(address: usize) -> Result<(), String> {
    if !readable_range(address, UPDATE_PROLOGUE.len()) {
        return Err("lyric update target is not readable".into());
    }
    let actual = unsafe { std::slice::from_raw_parts(address as *const u8, UPDATE_PROLOGUE.len()) };
    if actual != UPDATE_PROLOGUE {
        return Err(format!(
            "unsupported KSongsUI.dll lyric update implementation at 0x{address:08x}; bytes={}",
            hex_bytes(actual)
        ));
    }
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            output.push(' ');
        }
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[derive(Clone, Copy)]
struct PeSection {
    name: [u8; 8],
    address: usize,
    length: usize,
    readable: bool,
    executable: bool,
}

struct PeImage {
    base: usize,
    size: usize,
    sections: Vec<PeSection>,
}

impl PeImage {
    unsafe fn from_module(base: usize) -> Result<Self, String> {
        if base == 0 || !readable_range(base, 0x100) {
            return Err("KSongsUI.dll has an invalid image base".into());
        }
        let bytes = base as *const u8;
        if unsafe { read_at::<u16>(bytes, 0) } != 0x5a4d {
            return Err("KSongsUI.dll is missing its DOS header".into());
        }
        let pe_offset = unsafe { read_at::<u32>(bytes, 0x3c) } as usize;
        if pe_offset > 0x1000 || !readable_range(base, pe_offset + 0x100) {
            return Err("KSongsUI.dll has an invalid PE offset".into());
        }
        if unsafe { read_at::<u32>(bytes, pe_offset) } != 0x0000_4550 {
            return Err("KSongsUI.dll is missing its PE signature".into());
        }
        let section_count = unsafe { read_at::<u16>(bytes, pe_offset + 6) } as usize;
        let optional_size = unsafe { read_at::<u16>(bytes, pe_offset + 20) } as usize;
        let optional = pe_offset + 24;
        let image_size = unsafe { read_at::<u32>(bytes, optional + 56) } as usize;
        let section_table = optional + optional_size;
        if section_count == 0
            || section_count > 96
            || image_size == 0
            || !readable_range(base, section_table + section_count * 40)
        {
            return Err("KSongsUI.dll has invalid section metadata".into());
        }
        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let header = section_table + index * 40;
            let name = unsafe { read_at::<[u8; 8]>(bytes, header) };
            let virtual_size = unsafe { read_at::<u32>(bytes, header + 8) } as usize;
            let virtual_address = unsafe { read_at::<u32>(bytes, header + 12) } as usize;
            let raw_size = unsafe { read_at::<u32>(bytes, header + 16) } as usize;
            let characteristics = unsafe { read_at::<u32>(bytes, header + 36) };
            let length = virtual_size.max(raw_size);
            let Some(address) = base.checked_add(virtual_address) else {
                continue;
            };
            if length == 0 || virtual_address.saturating_add(length) > image_size {
                continue;
            }
            sections.push(PeSection {
                name,
                address,
                length,
                readable: characteristics & 0x4000_0000 != 0,
                executable: characteristics & 0x2000_0000 != 0,
            });
        }
        Ok(Self {
            base,
            size: image_size,
            sections,
        })
    }

    unsafe fn find_virtual_method(&self, rtti_name: &[u8], slot: usize) -> Result<usize, String> {
        let name = self
            .find_bytes(rtti_name)
            .ok_or_else(|| format!("RTTI class {} was not found", display_rtti(rtti_name)))?;
        let type_descriptor = name
            .checked_sub(8)
            .ok_or_else(|| "invalid RTTI type descriptor".to_owned())?;
        hook_log(
            LogLevel::Debug,
            format_args!(
                "RTTI {} name=0x{name:08x} type_descriptor=0x{type_descriptor:08x}",
                display_rtti(rtti_name)
            ),
        );
        let mut candidates = Vec::new();
        for type_reference in self.find_all_u32(type_descriptor as u32) {
            let Some(locator) = type_reference.checked_sub(12) else {
                continue;
            };
            if !self.contains(locator, 20)
                || unsafe { ptr::read_unaligned(locator as *const u32) } != 0
                || unsafe { ptr::read_unaligned((locator + 12) as *const u32) }
                    != type_descriptor as u32
            {
                continue;
            }
            for vtable_reference in self.find_all_u32(locator as u32) {
                let Some(method_pointer) = vtable_reference.checked_add(4 + slot * 4) else {
                    continue;
                };
                if !self.contains(method_pointer, 4) {
                    continue;
                }
                let target = unsafe { ptr::read_unaligned(method_pointer as *const u32) } as usize;
                if self.executable(target) && !candidates.contains(&target) {
                    candidates.push(target);
                }
            }
        }
        hook_log(
            LogLevel::Debug,
            format_args!(
                "RTTI {} virtual method candidates={} slot={slot}",
                display_rtti(rtti_name),
                candidates.len()
            ),
        );
        candidates
            .iter()
            .copied()
            .find(|target| unsafe {
                readable_range(*target, UPDATE_PROLOGUE.len())
                    && std::slice::from_raw_parts(*target as *const u8, UPDATE_PROLOGUE.len())
                        == UPDATE_PROLOGUE
            })
            .or_else(|| (candidates.len() == 1).then_some(candidates[0]))
            .ok_or_else(|| "RTTI virtual method target was not found".to_owned())
    }

    fn find_bytes(&self, needle: &[u8]) -> Option<usize> {
        self.find_all_bytes(needle).into_iter().next()
    }

    fn find_all_u32(&self, value: u32) -> Vec<usize> {
        self.find_all_bytes(&value.to_le_bytes())
    }

    fn find_all_bytes(&self, needle: &[u8]) -> Vec<usize> {
        let mut matches = Vec::new();
        if needle.is_empty() {
            return matches;
        }
        for section in self.sections.iter().filter(|section| section.readable) {
            let end = section.address.saturating_add(section.length);
            let mut cursor = section.address;
            while cursor < end {
                let Some(available) = readable_prefix(cursor) else {
                    cursor = cursor.saturating_add(0x1000).min(end);
                    continue;
                };
                let length = available.min(end - cursor);
                if length >= needle.len() {
                    let bytes = unsafe { std::slice::from_raw_parts(cursor as *const u8, length) };
                    let mut offset = 0;
                    while let Some(found) = find_subslice(&bytes[offset..], needle) {
                        let absolute_offset = offset + found;
                        matches.push(cursor + absolute_offset);
                        offset = absolute_offset + 1;
                        if offset + needle.len() > bytes.len() {
                            break;
                        }
                    }
                }
                cursor = cursor.saturating_add(length.max(1));
            }
        }
        matches
    }

    fn log_sections(&self) {
        for section in &self.sections {
            let name_end = section
                .name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(section.name.len());
            let name = String::from_utf8_lossy(&section.name[..name_end]);
            let first_region = readable_prefix(section.address).unwrap_or(0);
            hook_log(
                LogLevel::Debug,
                format_args!(
                    "PE section {name} address=0x{:08x} length=0x{:x} readable={} executable={} first_region=0x{first_region:x}",
                    section.address, section.length, section.readable, section.executable
                ),
            );
        }
    }

    fn executable(&self, address: usize) -> bool {
        self.sections.iter().any(|section| {
            section.executable
                && address >= section.address
                && address < section.address.saturating_add(section.length)
        })
    }

    fn contains(&self, address: usize, length: usize) -> bool {
        address >= self.base
            && address
                .checked_add(length)
                .is_some_and(|end| end <= self.base.saturating_add(self.size))
            && readable_range(address, length)
    }
}

fn display_rtti(name: &[u8]) -> String {
    String::from_utf8_lossy(name.strip_suffix(&[0]).unwrap_or(name)).into_owned()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
