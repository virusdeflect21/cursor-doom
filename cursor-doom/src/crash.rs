//! Last-resort crash reporting: names the faulting module so a native fault in
//! the C engine can be told apart from a bug in the Rust layer.

use std::io::Write;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::{
    RtlCaptureStackBackTrace, SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

extern "C" {
    /// MSVC CRT: flush every stream, so Doom's buffered printf output is not
    /// lost when the process dies.
    fn _flushall() -> i32;
}

pub fn install() {
    unsafe {
        SetUnhandledExceptionFilter(Some(filter));
    }
}

fn module_of(address: usize) -> String {
    if address == 0 {
        return "?".into();
    }
    unsafe {
        let mut module = HMODULE::default();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(address as *const u16),
            &mut module,
        );
        if ok.is_err() {
            return format!("0x{address:X} (unknown module)");
        }
        let mut buffer = [0u16; 260];
        let len = GetModuleFileNameW(Some(module), &mut buffer);
        let name = String::from_utf16_lossy(&buffer[..len as usize]);
        format!("{name}+0x{:X}", address - module.0 as usize)
    }
}

fn record(text: &str) {
    eprintln!("[cursor-doom] {text}");
    let path = crate::status::app_dir().join("crash.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{text}");
    }
}

unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    let _ = _flushall();
    let _ = std::io::Write::flush(&mut std::io::stdout());
    if !info.is_null() {
        let pointers = &*info;
        let record_ptr = pointers.ExceptionRecord;
        if !record_ptr.is_null() {
            let exception = &*record_ptr;
            let code = exception.ExceptionCode.0 as u32;
            let address = exception.ExceptionAddress as usize;
            record(&format!(
                "CRASH: exception 0x{code:08X} at {} ({} parameters)",
                module_of(address),
                exception.NumberParameters
            ));
            for index in 0..exception.NumberParameters.min(4) as usize {
                record(&format!(
                    "  param[{index}] = 0x{:X}",
                    exception.ExceptionInformation[index]
                ));
            }
            let mut frames = [std::ptr::null_mut(); 32];
            let captured = RtlCaptureStackBackTrace(0, &mut frames, None);
            record(&format!("  stack ({captured} frames):"));
            let base = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .map(|m| m.0 as usize)
                .unwrap_or(0);
            for (index, frame) in frames[..captured as usize].iter().enumerate() {
                let address = *frame as usize;
                record(&format!(
                    "    #{index} exe+0x{:X} ({})",
                    address.wrapping_sub(base),
                    module_of(address)
                ));
            }
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}
