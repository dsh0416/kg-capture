use std::mem::transmute;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::PCSTR;

type EmitFn = unsafe extern "system" fn(u32) -> u32;

fn main() {
    let emit = wait_for_hook();
    let started = Instant::now();
    loop {
        let position_ms = started.elapsed().as_millis() as u32;
        unsafe {
            emit(position_ms);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_hook() -> EmitFn {
    loop {
        if let Ok(module) = unsafe { GetModuleHandleW(windows::core::w!("kg_capture_hook.dll")) }
            && let Some(address) = unsafe {
                GetProcAddress(
                    module,
                    PCSTR(c"kg_capture_fixture_emit".as_ptr().cast::<u8>()),
                )
            }
        {
            return unsafe { transmute::<unsafe extern "system" fn() -> isize, EmitFn>(address) };
        }
        thread::sleep(Duration::from_millis(10));
    }
}
