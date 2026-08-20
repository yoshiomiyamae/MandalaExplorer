//! Per-thread COM initialisation, shared by the decoders that need it.
//!
//! Both Media Foundation and WIC refuse to hand out an interface on a thread
//! that has not joined an apartment, and decoding happens on whichever worker
//! thread picked the job up. Doing it per thread and tearing it down with the
//! thread means no caller has to remember.

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

/// Puts the calling thread into the multithreaded apartment, once.
pub(crate) fn ensure_thread_com() {
    thread_local! {
        static COM_GUARD: ComGuard = ComGuard::new();
    }
    COM_GUARD.with(|_| {});
}

struct ComGuard;

impl ComGuard {
    fn new() -> Self {
        unsafe {
            // Already initialized on this thread is fine; the UI framework may
            // well have gotten there first.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        Self
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
