//! Keeps the computer awake while the bot runs unattended, e.g. overnight.
//!
//! On macOS this holds IOKit power assertions, the same mechanism as
//! `caffeinate`. Holding one also keeps App Nap from throttling the app while
//! its window is hidden. On Windows it sets the engine thread's execution
//! state, so it must be created and dropped on the same thread.

/// How much of the computer to keep awake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Awake {
    /// Normal sleep settings.
    Off,
    /// No idle sleep; the screen can still turn off.
    System,
    /// No idle sleep and the screen stays on, so the browser streaming
    /// Pocket Option is never throttled for being out of sight.
    Display,
}

impl Awake {
    pub fn name(self) -> &'static str {
        match self {
            Awake::Off => "off",
            Awake::System => "system",
            Awake::Display => "display",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Awake::Off),
            "system" => Some(Awake::System),
            "display" => Some(Awake::Display),
            _ => None,
        }
    }
}

/// Released on drop.
pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    ids: Vec<u32>,
}

impl KeepAwake {
    /// `None` when the system refuses, or on systems this doesn't support.
    pub fn start(level: Awake) -> Option<Self> {
        if level == Awake::Off {
            return None;
        }
        imp::start(level)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Awake, KeepAwake};
    use std::ffi::{CString, c_char, c_void};

    type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    fn assert(kind: &str) -> Option<u32> {
        let kind = CString::new(kind).ok()?;
        let name = CString::new("Daytrade Assist is trading unattended").ok()?;
        // SAFETY: both strings are NUL-terminated and outlive the calls; the
        // CFStrings are created here and released once the assertion holds them.
        unsafe {
            let kind = CFStringCreateWithCString(std::ptr::null(), kind.as_ptr(), UTF8);
            let name = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), UTF8);
            let mut id = 0;
            let ok = !kind.is_null() && !name.is_null() && IOPMAssertionCreateWithName(kind, LEVEL_ON, name, &mut id) == 0;
            for s in [kind, name] {
                if !s.is_null() {
                    CFRelease(s);
                }
            }
            ok.then_some(id)
        }
    }

    pub fn start(level: Awake) -> Option<KeepAwake> {
        let mut ids = vec![assert("PreventUserIdleSystemSleep")?];
        if level == Awake::Display {
            match assert("PreventUserIdleDisplaySleep") {
                Some(id) => ids.push(id),
                None => {
                    drop(KeepAwake { ids });
                    return None;
                }
            }
        }
        Some(KeepAwake { ids })
    }

    impl Drop for KeepAwake {
        fn drop(&mut self) {
            for &id in &self.ids {
                // SAFETY: each id came from a successful IOPMAssertionCreateWithName.
                unsafe {
                    IOPMAssertionRelease(id);
                }
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{Awake, KeepAwake};

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    pub fn start(level: Awake) -> Option<KeepAwake> {
        let display = if level == Awake::Display { ES_DISPLAY_REQUIRED } else { 0 };
        // SAFETY: plain flags in, previous state out.
        let ok = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | display) } != 0;
        ok.then_some(KeepAwake {})
    }

    impl Drop for KeepAwake {
        fn drop(&mut self) {
            // SAFETY: as above; clears the flags this thread set.
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::{Awake, KeepAwake};

    pub fn start(_: Awake) -> Option<KeepAwake> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for a in [Awake::Off, Awake::System, Awake::Display] {
            assert_eq!(Awake::parse(a.name()), Some(a));
        }
        assert_eq!(Awake::parse("on"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn holds_and_releases_an_assertion() {
        let guard = KeepAwake::start(Awake::System).expect("macOS refused the power assertion");
        assert_eq!(guard.ids.len(), 1);
        drop(guard);
        assert!(KeepAwake::start(Awake::Off).is_none());
    }
}
