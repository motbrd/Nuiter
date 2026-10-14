//! Puts child processes in a kill-on-close job, so the model server and reader helpers never outlive Nuiter.

use std::sync::OnceLock;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};

struct Job(HANDLE);
// SAFETY: a job handle may be used from any thread.
unsafe impl Send for Job {}
// SAFETY: as for Send: the handle is only passed to AssignProcessToJobObject, which any thread may call.
unsafe impl Sync for Job {}

fn job() -> Option<&'static Job> {
    static JOB: OnceLock<Option<Job>> = OnceLock::new();
    JOB.get_or_init(|| {
        // SAFETY: creates an unnamed job owned (and closed at exit) by this process.
        unsafe {
            let h = CreateJobObjectW(None, None).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .ok()?;
            Some(Job(h))
        }
    })
    .as_ref()
}

pub fn assign(process: HANDLE) {
    if let Some(j) = job() {
        // SAFETY: both handles are valid; failure (already in another job) is harmless.
        unsafe {
            let _ = AssignProcessToJobObject(j.0, process);
        }
    }
}
