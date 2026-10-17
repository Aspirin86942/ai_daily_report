//! CLI-scoped process-tree budget. Child worker jobs retain their tighter limits.
use crate::ReportError;
use serde_json::{json, Value};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{JobObjects::*, Threading::GetCurrentProcess},
};

#[cfg(windows)]
pub struct Budget(HANDLE);
#[cfg(windows)]
impl Budget {
    pub fn new(mib: u64) -> Result<Self, ReportError> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let budget = Self(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit = (mib * 1024 * 1024) as usize;
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
            || unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(budget)
    }
    pub fn sample(&self) -> Result<Value, ReportError> {
        let mut accounting: JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION =
            unsafe { std::mem::zeroed() };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        for (kind, pointer, size) in [
            (
                JobObjectBasicAndIoAccountingInformation,
                &mut accounting as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(&accounting),
            ),
            (
                JobObjectExtendedLimitInformation,
                &mut limits as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(&limits),
            ),
        ] {
            if unsafe {
                QueryInformationJobObject(self.0, kind, pointer, size as u32, std::ptr::null_mut())
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(
            json!({"scope":"collection_process_tree_including_exited_children","cpu_ms":(accounting.BasicInfo.TotalUserTime+accounting.BasicInfo.TotalKernelTime)/10000,"peak_commit_bytes":limits.PeakJobMemoryUsed,"read_bytes":accounting.IoInfo.ReadTransferCount,"write_bytes":accounting.IoInfo.WriteTransferCount,"processes":accounting.BasicInfo.TotalProcesses}),
        )
    }
}
#[cfg(windows)]
impl Drop for Budget {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
#[cfg(not(windows))]
pub struct Budget;
#[cfg(not(windows))]
impl Budget {
    pub fn new(_mib: u64) -> Result<Self, ReportError> {
        Err(super::error("ARCHIVE_RESOURCE_BUDGET_UNSUPPORTED"))
    }
    pub fn sample(&self) -> Result<Value, ReportError> {
        Ok(json!(null))
    }
}
