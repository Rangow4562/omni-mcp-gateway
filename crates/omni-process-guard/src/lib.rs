#![allow(unsafe_code)]
//! Isolated OS process-tree containment for `omni-mcp`.
//!
//! This is the ONLY crate in the workspace permitted to use `unsafe` FFI:
//! - On Windows: creates a Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//!   and assigns the spawned child process to it via `windows-sys`.
//! - On Unix: tracks the child's process group ID (`pgid`) and terminates the entire
//!   process group via `killpg(pgid, SIGKILL)` on drop or explicit termination.

use std::process::{Child, Command};

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct ProcessGuard {
        job: HANDLE,
    }

    // HANDLE is an opaque kernel object handle safe to send/drop across threads.
    unsafe impl Send for ProcessGuard {}
    unsafe impl Sync for ProcessGuard {}

    impl ProcessGuard {
        pub fn configure_command(_cmd: &mut Command) {}

        pub fn attach_child(child: &Child) -> Result<Self, String> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(format!(
                        "CreateJobObjectW failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }

                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    let err = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(format!("SetInformationJobObject failed: {}", err));
                }

                let proc_handle = child.as_raw_handle() as HANDLE;
                let assigned = AssignProcessToJobObject(job, proc_handle);
                if assigned == 0 {
                    let err = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(format!("AssignProcessToJobObject failed: {}", err));
                }

                Ok(Self { job })
            }
        }

        pub fn terminate(&mut self) {
            if !self.job.is_null() {
                unsafe {
                    let _ = TerminateJobObject(self.job, 1);
                    let _ = CloseHandle(self.job);
                }
                self.job = std::ptr::null_mut();
            }
        }
    }

    impl Drop for ProcessGuard {
        fn drop(&mut self) {
            self.terminate();
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    pub struct ProcessGuard {
        pgid: libc::pid_t,
    }

    impl ProcessGuard {
        pub fn configure_command(cmd: &mut Command) {
            cmd.process_group(0);
        }

        pub fn attach_child(child: &Child) -> Result<Self, String> {
            let pid = child.id() as libc::pid_t;
            if pid <= 0 {
                return Err("Invalid child PID for process group containment".to_string());
            }
            Ok(Self { pgid: pid })
        }

        pub fn terminate(&mut self) {
            if self.pgid > 0 {
                unsafe {
                    let _ = libc::killpg(self.pgid, libc::SIGKILL);
                }
                self.pgid = 0;
            }
        }
    }

    impl Drop for ProcessGuard {
        fn drop(&mut self) {
            self.terminate();
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use std::process::{Child, Command};

    pub struct ProcessGuard;

    impl ProcessGuard {
        pub fn configure_command(_cmd: &mut Command) {}
        pub fn attach_child(_child: &Child) -> Result<Self, String> {
            Ok(Self)
        }
        pub fn terminate(&mut self) {}
    }
}

/// RAII handle that terminates the child process and all its descendants when dropped.
pub struct ProcessGuard(imp::ProcessGuard);

impl ProcessGuard {
    /// Configures `cmd` before spawning (e.g. `process_group(0)` on Unix).
    pub fn configure_command(cmd: &mut Command) {
        imp::ProcessGuard::configure_command(cmd);
    }

    /// Attaches an already-spawned `Child` to a containment boundary.
    pub fn attach_child(child: &Child) -> Result<Self, String> {
        imp::ProcessGuard::attach_child(child).map(Self)
    }

    /// Immediately terminates the contained process tree.
    pub fn terminate(&mut self) {
        self.0.terminate();
    }
}
