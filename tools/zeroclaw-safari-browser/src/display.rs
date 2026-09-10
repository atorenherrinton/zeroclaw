//! One OS-owned activity assertion, refreshed only by requested Safari work.
//! This wakes a sleeping display; it cannot authenticate or unlock a session.

#[cfg(target_os = "macos")]
mod native {
    use anyhow::{Result, bail};
    use std::{cell::Cell, ffi::c_void, ptr};

    type CFStringRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            text: *const std::ffi::c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(value: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionDeclareUserActivity(
            name: CFStringRef,
            user_type: u32,
            assertion: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion: u32) -> i32;
    }

    #[derive(Default)]
    pub struct DisplayActivity {
        // IOPMLib requires callers to pass the last returned ID on refresh.
        // The OS owns expiration and current lock/display state, not this ID.
        assertion: Cell<u32>,
    }

    impl DisplayActivity {
        pub fn wake(&self) -> Result<()> {
            // SAFETY: static NUL-terminated UTF-8, default allocator, and a
            // valid out-pointer. The retained CFString is released exactly once.
            let name = unsafe {
                CFStringCreateWithCString(
                    ptr::null(),
                    c"ZeroClaw Safari browser activity".as_ptr(),
                    0x0800_0100,
                )
            };
            if name.is_null() {
                bail!("Unable to allocate the Safari display activity name");
            }
            let mut assertion = self.assertion.get();
            // kIOPMUserActiveLocal = 0 wakes the local display. It does not
            // synthesize input, change idle/lock settings, or grant permissions.
            let status = unsafe {
                let status = IOPMAssertionDeclareUserActivity(name, 0, &mut assertion);
                CFRelease(name);
                status
            };
            if status != 0 {
                bail!("macOS rejected the Safari display wake request ({status})");
            }
            self.assertion.set(assertion);
            Ok(())
        }

        pub fn release(&self) -> Result<()> {
            let assertion = self.assertion.get();
            if assertion != 0 {
                // SAFETY: this object owns the assertion returned by IOPMLib.
                let status = unsafe { IOPMAssertionRelease(assertion) };
                if status != 0 {
                    bail!("macOS could not release Safari display activity ({status})");
                }
                self.assertion.set(0);
            }
            Ok(())
        }
    }

    impl Drop for DisplayActivity {
        fn drop(&mut self) {
            if let Err(error) = self.release() {
                eprintln!("Safari display activity cleanup failed: {error}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[ignore = "Wakes the physical display; run explicitly on a test Mac"]
        fn native_wake_refresh_and_release() {
            let activity = DisplayActivity::default();
            activity.wake().unwrap();
            assert_ne!(activity.assertion.get(), 0);
            activity.wake().unwrap();
            activity.release().unwrap();
            assert_eq!(activity.assertion.get(), 0);
            activity.release().unwrap();
        }
    }
}

#[cfg(target_os = "macos")]
pub use native::DisplayActivity;

#[cfg(not(target_os = "macos"))]
#[derive(Default)]
pub struct DisplayActivity;

#[cfg(not(target_os = "macos"))]
impl DisplayActivity {
    pub fn wake(&self) -> anyhow::Result<()> {
        anyhow::bail!("Safari display wake requires macOS")
    }

    pub fn release(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
