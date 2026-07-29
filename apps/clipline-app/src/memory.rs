use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{
    VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_PRIVATE,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, K32QueryWorkingSetEx, PROCESS_MEMORY_COUNTERS,
    PROCESS_MEMORY_COUNTERS_EX2, PSAPI_WORKING_SET_EX_INFORMATION,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct MemoryStatus {
    /// Whole-tree total. Retained with its original meaning for one release so
    /// an older frontend against a newer backend is not silently wrong.
    pub private_working_set_bytes: u64,
    /// The Rust process alone — the memory this codebase actually allocates.
    pub process_private_working_set_bytes: u64,
    /// Every descendant summed. Deliberately **not** labelled "WebView2": the
    /// walk also picks up the long-lived `ffmpeg.exe` child when the CPU encoder
    /// path is active.
    pub children_private_working_set_bytes: u64,
}

fn memory_status_from_parts(root: u64, children: impl IntoIterator<Item = u64>) -> MemoryStatus {
    let children_total = children
        .into_iter()
        .fold(0u64, |total, bytes| total.saturating_add(bytes));
    MemoryStatus {
        private_working_set_bytes: root.saturating_add(children_total),
        process_private_working_set_bytes: root,
        children_private_working_set_bytes: children_total,
    }
}

const MEMORY_SAMPLE_CACHE_TTL: Duration = Duration::from_secs(1);
const MEMORY_QUERY_ACCESS: u32 = PROCESS_QUERY_LIMITED_INFORMATION;
const LEGACY_MEMORY_QUERY_ACCESS: u32 = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ;

struct CachedMemorySample {
    completed_at: Instant,
    result: Result<MemoryStatus, String>,
}

pub struct MemorySampler {
    cache_ttl: Duration,
    state: tokio::sync::Mutex<Option<CachedMemorySample>>,
}

impl Default for MemorySampler {
    fn default() -> Self {
        Self::with_cache_ttl(MEMORY_SAMPLE_CACHE_TTL)
    }
}

impl MemorySampler {
    fn with_cache_ttl(cache_ttl: Duration) -> Self {
        Self {
            cache_ttl,
            state: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn sample(&self) -> Result<MemoryStatus, String> {
        self.sample_with(current_process_tree_memory).await
    }

    async fn sample_with(
        &self,
        measure: impl FnOnce() -> Result<MemoryStatus, String> + Send + 'static,
    ) -> Result<MemoryStatus, String> {
        let mut state = self.state.lock().await;
        if let Some(cached) = state.as_ref() {
            if cached.completed_at.elapsed() < self.cache_ttl {
                return cached.result.clone();
            }
        }
        let result = tauri::async_runtime::spawn_blocking(measure)
            .await
            .map_err(|error| format!("memory sampler task failed: {error}"))?;
        *state = Some(CachedMemorySample {
            completed_at: Instant::now(),
            result: result.clone(),
        });
        result
    }
}

pub fn current_process_tree_memory() -> Result<MemoryStatus, String> {
    let current_pid = unsafe { GetCurrentProcessId() };
    let root = query_process_private_working_set(unsafe { GetCurrentProcess() })?;

    // One walk backs every field, so the split cannot drift between them and
    // the sampler's cache TTL still covers a single snapshot.
    let children = child_process_ids(current_pid)?
        .into_iter()
        .filter_map(|pid| query_process_private_working_set_for_pid(pid).ok());

    Ok(memory_status_from_parts(root, children))
}

fn child_process_ids(root_pid: u32) -> Result<Vec<u32>, String> {
    Ok(child_process_ids_from_entries(
        root_pid,
        &process_snapshot()?,
    ))
}

fn process_snapshot() -> Result<Vec<ProcessEntry>, String> {
    let snapshot = Snapshot::new()?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { zeroed() }
    };
    let mut processes = Vec::new();

    let mut ok = unsafe { Process32FirstW(snapshot.handle, &mut entry) };
    while ok != 0 {
        processes.push(ProcessEntry {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            name: process_name(&entry),
        });
        ok = unsafe { Process32NextW(snapshot.handle, &mut entry) };
    }

    Ok(processes)
}

fn child_process_ids_from_entries(root_pid: u32, entries: &[ProcessEntry]) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<ProcessEntry>> = HashMap::new();
    for entry in entries {
        children
            .entry(entry.parent_pid)
            .or_default()
            .push(entry.clone());
    }

    let mut out = Vec::new();
    let mut seen = HashSet::from([root_pid]);
    let mut stack = children.remove(&root_pid).unwrap_or_default();
    while let Some(process) = stack.pop() {
        if !seen.insert(process.pid) {
            continue;
        }
        if process.name != "conhost.exe" {
            out.push(process.pid);
        }
        if let Some(grandchildren) = children.remove(&process.pid) {
            stack.extend(grandchildren);
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessEntry {
    pid: u32,
    parent_pid: u32,
    name: String,
}

fn process_name(entry: &PROCESSENTRY32W) -> String {
    process_name_from_wide(&entry.szExeFile)
}

fn process_name_from_wide(name: &[u16]) -> String {
    let len = name.iter().position(|ch| *ch == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..len]).to_ascii_lowercase()
}

fn query_process_private_working_set_for_pid(pid: u32) -> Result<u64, String> {
    let primary = with_process_handle(pid, MEMORY_QUERY_ACCESS, |handle| {
        query_process_private_working_set_ex2(handle)
    });
    query_with_fallback(primary, || {
        let page_size = page_size()?;
        with_process_handle(pid, LEGACY_MEMORY_QUERY_ACCESS, |handle| {
            process_private_working_set_bytes(handle, page_size)
        })
    })
}

fn with_process_handle(
    pid: u32,
    access: u32,
    query: impl FnOnce(HANDLE) -> Result<u64, String>,
) -> Result<u64, String> {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return Err(format!(
            "open process {pid} for memory query: {}",
            std::io::Error::last_os_error()
        ));
    }
    let result = query(handle);
    unsafe {
        CloseHandle(handle);
    }
    result
}

fn query_process_private_working_set(handle: HANDLE) -> Result<u64, String> {
    query_with_fallback(query_process_private_working_set_ex2(handle), || {
        process_private_working_set_bytes(handle, page_size()?)
    })
}

fn query_process_private_working_set_ex2(handle: HANDLE) -> Result<u64, String> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            handle,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX2).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return Err(format!(
            "query process memory counters: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(counters.PrivateWorkingSetSize as u64)
}

fn query_with_fallback(
    primary: Result<u64, String>,
    fallback: impl FnOnce() -> Result<u64, String>,
) -> Result<u64, String> {
    match primary {
        Ok(bytes) => Ok(bytes),
        Err(primary_error) => fallback().map_err(|fallback_error| {
            format!("{primary_error}; legacy working set query: {fallback_error}")
        }),
    }
}

fn process_private_working_set_bytes(handle: HANDLE, page_size: usize) -> Result<u64, String> {
    let mut addr = 0usize;
    let mut total_pages = 0u64;
    let mut saw_region = false;

    loop {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
        let read = unsafe {
            VirtualQueryEx(
                handle,
                addr as *const _,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if read == 0 {
            if !saw_region {
                return Err(format!(
                    "query process memory map: {}",
                    std::io::Error::last_os_error()
                ));
            }
            break;
        }
        saw_region = true;

        let base = info.BaseAddress as usize;
        let size = info.RegionSize;
        if info.State == MEM_COMMIT && info.Type == MEM_PRIVATE && size > 0 {
            total_pages =
                total_pages.saturating_add(resident_private_pages(handle, base, size, page_size)?);
        }

        let next = base.saturating_add(size);
        if next <= addr {
            break;
        }
        addr = next;
    }

    Ok(total_pages.saturating_mul(page_size as u64))
}

fn resident_private_pages(
    handle: HANDLE,
    base: usize,
    size: usize,
    page_size: usize,
) -> Result<u64, String> {
    const WS_VALID: usize = 1;
    const WS_SHARED: usize = 1 << 15;
    const CHUNK_PAGES: usize = 4096;

    let pages = size.div_ceil(page_size);
    if pages == 0 {
        return Ok(0);
    }

    let mut resident = 0u64;
    let mut page = 0usize;
    while page < pages {
        let count = (pages - page).min(CHUNK_PAGES);
        let mut query: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..count)
            .map(|i| PSAPI_WORKING_SET_EX_INFORMATION {
                VirtualAddress: (base + (page + i) * page_size) as *mut _,
                VirtualAttributes: Default::default(),
            })
            .collect();
        let bytes = query
            .len()
            .checked_mul(size_of::<PSAPI_WORKING_SET_EX_INFORMATION>())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| "working set query chunk is too large".to_string())?;

        let ok = unsafe { K32QueryWorkingSetEx(handle, query.as_mut_ptr().cast(), bytes) };
        if ok == 0 {
            return Err(format!(
                "query process working set: {}",
                std::io::Error::last_os_error()
            ));
        }
        resident += query
            .iter()
            .filter(|entry| {
                let flags = unsafe { entry.VirtualAttributes.Flags };
                flags & WS_VALID != 0 && flags & WS_SHARED == 0
            })
            .count() as u64;
        page += count;
    }

    Ok(resident)
}

fn page_size() -> Result<usize, String> {
    let mut info: SYSTEM_INFO = unsafe { zeroed() };
    unsafe {
        GetSystemInfo(&mut info);
    };
    if info.dwPageSize == 0 {
        return Err("could not read system page size".into());
    }
    Ok(info.dwPageSize as usize)
}

struct Snapshot {
    handle: HANDLE,
}

impl Snapshot {
    fn new() -> Result<Self, String> {
        let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if handle == INVALID_HANDLE_VALUE {
            return Err("could not snapshot process list".into());
        }
        Ok(Self { handle })
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn proc(pid: u32, parent_pid: u32, name: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid,
            name: name.to_string(),
        }
    }

    fn sorted(mut ids: Vec<u32>) -> Vec<u32> {
        ids.sort_unstable();
        ids
    }

    #[test]
    fn memory_sampling_requires_only_limited_query_rights() {
        assert_eq!(MEMORY_QUERY_ACCESS, PROCESS_QUERY_LIMITED_INFORMATION);
    }

    #[test]
    fn status_separates_the_root_process_from_its_descendants() {
        let status = memory_status_from_parts(100, [20, 30]);

        assert_eq!(status.process_private_working_set_bytes, 100);
        assert_eq!(status.children_private_working_set_bytes, 50);
    }

    #[test]
    fn status_keeps_the_summed_field_for_backward_compatibility() {
        // The frontend shipped reading `private_working_set_bytes` as the tree
        // total; keep that meaning so an older UI against a newer backend is
        // not silently wrong.
        let status = memory_status_from_parts(100, [20, 30]);

        assert_eq!(status.private_working_set_bytes, 150);
    }

    #[test]
    fn status_without_descendants_reports_only_the_root() {
        let status = memory_status_from_parts(100, []);

        assert_eq!(status.children_private_working_set_bytes, 0);
        assert_eq!(status.private_working_set_bytes, 100);
        assert_eq!(status.process_private_working_set_bytes, 100);
    }

    #[test]
    fn status_saturates_instead_of_overflowing() {
        let status = memory_status_from_parts(u64::MAX, [1, 1]);

        assert_eq!(status.children_private_working_set_bytes, 2);
        assert_eq!(status.private_working_set_bytes, u64::MAX);
    }

    #[test]
    fn current_process_private_working_set_is_available_without_vm_read() {
        let bytes = query_process_private_working_set_for_pid(std::process::id())
            .expect("the current process should expose extended memory counters");

        assert!(bytes > 0);
    }

    #[test]
    fn unavailable_ex2_query_uses_legacy_fallback() {
        let fallback_called = std::cell::Cell::new(false);

        let bytes = query_with_fallback(Err("EX2 unavailable".into()), || {
            fallback_called.set(true);
            Ok(42)
        })
        .unwrap();

        assert_eq!(bytes, 42);
        assert!(fallback_called.get());
    }

    #[test]
    fn successful_ex2_query_skips_legacy_fallback() {
        let fallback_called = std::cell::Cell::new(false);

        let bytes = query_with_fallback(Ok(42), || {
            fallback_called.set(true);
            Ok(7)
        })
        .unwrap();

        assert_eq!(bytes, 42);
        assert!(!fallback_called.get());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_sampler_coalesces_concurrent_measurements() {
        let sampler = Arc::new(MemorySampler::with_cache_ttl(Duration::from_secs(1)));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let sampler = Arc::clone(&sampler);
            let calls = Arc::clone(&calls);
            tasks.push(tokio::spawn(async move {
                sampler
                    .sample_with(move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(25));
                        Ok(memory_status_from_parts(42, []))
                    })
                    .await
            }));
        }

        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap().private_working_set_bytes, 42);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn memory_sampler_caches_failures_but_retries_after_expiry() {
        let sampler = MemorySampler::with_cache_ttl(Duration::from_millis(1));
        let calls = Arc::new(AtomicUsize::new(0));

        let first_calls = Arc::clone(&calls);
        assert_eq!(
            sampler
                .sample_with(move || {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    Err("sample failed".to_string())
                })
                .await,
            Err("sample failed".to_string())
        );
        let cached_calls = Arc::clone(&calls);
        assert_eq!(
            sampler
                .sample_with(move || {
                    cached_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(memory_status_from_parts(7, []))
                })
                .await,
            Err("sample failed".to_string())
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
        let retry_calls = Arc::clone(&calls);
        assert_eq!(
            sampler
                .sample_with(move || {
                    retry_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(memory_status_from_parts(7, []))
                })
                .await
                .unwrap()
                .private_working_set_bytes,
            7
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn child_process_ids_walks_descendants_and_skips_conhost() {
        let ids = child_process_ids_from_entries(
            10,
            &[
                proc(20, 10, "msedgewebview2.exe"),
                proc(30, 20, "renderer.exe"),
                proc(40, 10, "conhost.exe"),
                proc(50, 40, "debug-child.exe"),
                proc(60, 99, "unrelated.exe"),
            ],
        );

        assert_eq!(sorted(ids), vec![20, 30, 50]);
    }

    #[test]
    fn child_process_ids_deduplicates_cycles() {
        let ids = child_process_ids_from_entries(
            1,
            &[
                proc(2, 1, "child.exe"),
                proc(3, 2, "grandchild.exe"),
                proc(2, 3, "reused-pid.exe"),
            ],
        );

        assert_eq!(sorted(ids), vec![2, 3]);
    }

    #[test]
    fn process_name_from_wide_stops_at_nul_and_lowercases() {
        let mut raw: Vec<u16> = "ClipLine.EXE".encode_utf16().collect();
        raw.push(0);
        raw.extend("ignored.exe".encode_utf16());

        assert_eq!(process_name_from_wide(&raw), "clipline.exe");
    }

    #[test]
    fn process_name_from_wide_accepts_unterminated_names() {
        let raw: Vec<u16> = "WebView2.EXE".encode_utf16().collect();

        assert_eq!(process_name_from_wide(&raw), "webview2.exe");
    }
}
