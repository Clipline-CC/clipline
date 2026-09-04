use super::*;

pub(crate) fn process_loopback_stream_config() -> (u32, i64) {
    (
        AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        POLLING_BUFFER_DURATION_100NS,
    )
}

pub fn enumerate_output_processes(
    device_id: Option<&str>,
) -> Result<Vec<AudioProcessInfo>, CaptureError> {
    init_com()?;
    // SAFETY: standard endpoint activation/session enumeration; COM results
    // are checked and any allocated strings are freed.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(init)?;
        let device = endpoint_device(&enumerator, eRender, device_id, true).map_err(init)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).map_err(init)?;
        let session_enum = manager.GetSessionEnumerator().map_err(init)?;
        let process_snapshot = process_snapshot();
        let mut processes = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for index in 0..session_enum.GetCount().map_err(init)? {
            let Ok(session) = session_enum.GetSession(index) else {
                continue;
            };
            if session.GetState().ok() == Some(AudioSessionStateExpired) {
                continue;
            }
            let Ok(session2) = session.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let pid = session2.GetProcessId().unwrap_or_default();
            if pid == 0 {
                continue;
            }
            let display_name = session
                .GetDisplayName()
                .ok()
                .and_then(|raw| pwstr_to_optional_string_and_free(raw).ok().flatten());
            let session_process_path = process_image_path(pid).or_else(|| {
                process_snapshot
                    .get(&pid)
                    .and_then(|entry| entry.image_name.clone())
            });
            let capture_pid =
                process_group_root(pid, session_process_path.as_deref(), &process_snapshot);
            if !seen.insert(capture_pid) {
                continue;
            }
            let process_path = process_image_path(capture_pid)
                .or_else(|| {
                    (capture_pid == pid)
                        .then(|| session_process_path.clone())
                        .flatten()
                })
                .or_else(|| {
                    process_snapshot
                        .get(&capture_pid)
                        .and_then(|entry| entry.image_name.clone())
                });
            let process_name = process_path
                .as_deref()
                .and_then(process_name_from_path)
                .or_else(|| display_name.clone());
            let label = display_name
                .filter(|name| !name.trim().is_empty())
                .or_else(|| process_name.clone())
                .unwrap_or_else(|| format!("Process {capture_pid}"));
            processes.push(AudioProcessInfo {
                pid: capture_pid,
                label,
                process_name,
                process_path,
            });
        }
        drop_duplicate_process_tree_ancestors(&mut processes, &process_snapshot);
        processes.sort_by(|a, b| {
            a.label
                .to_lowercase()
                .cmp(&b.label.to_lowercase())
                .then_with(|| a.pid.cmp(&b.pid))
        });
        Ok(processes)
    }
}

#[derive(Default)]
struct ProcessLoopbackActivationState {
    completed: Mutex<bool>,
    ready: Condvar,
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ProcessLoopbackActivation {
    state: Arc<ProcessLoopbackActivationState>,
}

impl IAgileObject_Impl for ProcessLoopbackActivation_Impl {}

#[allow(non_snake_case)]
impl IActivateAudioInterfaceCompletionHandler_Impl for ProcessLoopbackActivation_Impl {
    fn ActivateCompleted(
        &self,
        _activateoperation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> WindowsResult<()> {
        let mut guard = self.state.completed.lock().expect("activation mutex");
        *guard = true;
        self.state.ready.notify_one();
        Ok(())
    }
}

pub(crate) fn activate_process_loopback_client(pid: u32) -> Result<IAudioClient, CaptureError> {
    let state = Arc::new(ProcessLoopbackActivationState::default());
    let handler: IActivateAudioInterfaceCompletionHandler = ProcessLoopbackActivation {
        state: Arc::clone(&state),
    }
    .into();

    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let params_size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    // SAFETY: CoTaskMemAlloc returns an allocation suitable for PROPVARIANT
    // VT_BLOB ownership. The bytes copied are exactly AUDIOCLIENT_ACTIVATION_PARAMS.
    let params_blob = unsafe { CoTaskMemAlloc(params_size) };
    if params_blob.is_null() {
        return Err(CaptureError::Init(
            "WASAPI process loopback activation params allocation failed".into(),
        ));
    }
    // SAFETY: params_blob is a valid params_size allocation and params is live.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&params as *const AUDIOCLIENT_ACTIVATION_PARAMS).cast::<u8>(),
            params_blob.cast::<u8>(),
            params_size,
        );
    }
    let mut variant = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: params_size as u32,
                        pBlobData: params_blob.cast::<u8>(),
                    },
                },
            }),
        },
    };

    // SAFETY: the activation parameter PROPVARIANT owns its blob payload and is
    // valid for the duration of ActivateAudioInterfaceAsync.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &handler,
        )
    };
    let operation = match operation {
        Ok(operation) => operation,
        Err(error) => {
            // SAFETY: clears the VT_BLOB payload allocated with CoTaskMemAlloc.
            let _ = unsafe { PropVariantClear(&mut variant) };
            return Err(init(error));
        }
    };

    let deadline = Instant::now() + PROCESS_LOOPBACK_ACTIVATION_TIMEOUT;
    let mut guard = state.completed.lock().expect("activation mutex");
    loop {
        if *guard {
            drop(guard);
            let mut activate_result = HRESULT(0);
            let mut activated_interface = None;
            // SAFETY: the operation has signaled completion. The HRESULT and
            // returned interface are checked before use.
            if let Err(error) = unsafe {
                operation.GetActivateResult(&mut activate_result, &mut activated_interface)
            } {
                // SAFETY: clears the owned activation blob before returning.
                let _ = unsafe { PropVariantClear(&mut variant) };
                return Err(CaptureError::Init(format!(
                    "WASAPI GetActivateResult: {error}"
                )));
            }
            if let Err(error) = activate_result.ok() {
                // SAFETY: clears the owned activation blob before returning.
                let _ = unsafe { PropVariantClear(&mut variant) };
                return Err(CaptureError::Init(format!(
                    "WASAPI activation result: {error}"
                )));
            }
            let client = match activated_interface
                .ok_or_else(|| CaptureError::Init("WASAPI: activation returned no client".into()))
                .and_then(|unknown| {
                    unknown
                        .cast::<IAudioClient>()
                        .map_err(|e| CaptureError::Init(format!("WASAPI activation cast: {e}")))
                }) {
                Ok(client) => client,
                Err(error) => {
                    // SAFETY: clears the owned activation blob before returning.
                    let _ = unsafe { PropVariantClear(&mut variant) };
                    return Err(error);
                }
            };
            // SAFETY: activation is complete, so the owned activation blob can be released.
            let _ = unsafe { PropVariantClear(&mut variant) };
            return Ok(client);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            // SAFETY: clears the VT_BLOB payload allocated with CoTaskMemAlloc.
            let _ = unsafe { PropVariantClear(&mut variant) };
            return Err(process_loopback_activation_timeout(pid));
        };
        let (next_guard, timeout) = state
            .ready
            .wait_timeout(guard, remaining)
            .expect("activation result condvar");
        guard = next_guard;
        if timeout.timed_out() && !*guard {
            // SAFETY: clears the VT_BLOB payload allocated with CoTaskMemAlloc.
            let _ = unsafe { PropVariantClear(&mut variant) };
            return Err(process_loopback_activation_timeout(pid));
        }
    }
}

fn process_loopback_activation_timeout(pid: u32) -> CaptureError {
    CaptureError::OperationTimeout {
        operation: format!("WASAPI process loopback activation for pid {pid}"),
        after: PROCESS_LOOPBACK_ACTIVATION_TIMEOUT,
    }
}

pub(crate) fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    // SAFETY: the process handle is closed before return, and GetProcessTimes
    // writes into four initialized FILETIME values owned by this function.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let result = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(ProcessIdentity {
            creation_time: (u64::from(creation.dwHighDateTime) << 32)
                | u64::from(creation.dwLowDateTime),
        })
    }
}

pub(crate) fn process_image_path(pid: u32) -> Option<String> {
    // SAFETY: the process handle is closed before return, and the query buffer
    // is valid for the duration of the call.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        (!path.trim().is_empty()).then_some(path)
    }
}

pub(crate) fn process_snapshot() -> std::collections::HashMap<u32, ProcessSnapshotEntry> {
    let mut processes = std::collections::HashMap::new();
    // SAFETY: snapshot handle is closed before return; PROCESSENTRY32W is
    // initialized with the required size before ToolHelp reads into it.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return processes;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..PROCESSENTRY32W::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;
                if pid != 0 {
                    let fallback_name = utf16z_from_buf(&entry.szExeFile);
                    processes.insert(
                        pid,
                        ProcessSnapshotEntry {
                            parent_pid: entry.th32ParentProcessID,
                            image_name: (!fallback_name.trim().is_empty()).then_some(fallback_name),
                        },
                    );
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    processes
}

pub(crate) fn process_group_root(
    pid: u32,
    process_path: Option<&str>,
    snapshot: &std::collections::HashMap<u32, ProcessSnapshotEntry>,
) -> u32 {
    let mut current_pid = pid;
    let mut current_path = process_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .or_else(|| {
            snapshot
                .get(&pid)
                .and_then(|entry| entry.image_name.clone())
        });

    for parent_pid in process_parent_pids(pid, snapshot) {
        let Some(path) = current_path.as_deref() else {
            break;
        };
        let Some(parent) = snapshot.get(&parent_pid) else {
            break;
        };
        let Some(parent_path) = parent.image_name.as_deref() else {
            break;
        };
        if !same_process_image(path, parent_path) {
            break;
        }
        current_pid = parent_pid;
        current_path = Some(parent_path.to_string());
    }

    current_pid
}

pub(crate) fn drop_duplicate_process_tree_ancestors(
    processes: &mut Vec<AudioProcessInfo>,
    snapshot: &std::collections::HashMap<u32, ProcessSnapshotEntry>,
) {
    // Keep the child app's split track label and drop launcher parents whose
    // process-tree capture would duplicate the child. Parent-owned launcher
    // sounds remain available in the mixed Output Audio safety track.
    let duplicate_ancestors: std::collections::HashSet<u32> = processes
        .iter()
        .filter(|candidate| {
            processes.iter().any(|other| {
                candidate.pid != other.pid
                    && process_is_ancestor(candidate.pid, other.pid, snapshot)
                    && process_images_differ(candidate, other, snapshot)
            })
        })
        .map(|process| process.pid)
        .collect();
    processes.retain(|process| !duplicate_ancestors.contains(&process.pid));
}

fn process_is_ancestor(
    ancestor_pid: u32,
    descendant_pid: u32,
    snapshot: &std::collections::HashMap<u32, ProcessSnapshotEntry>,
) -> bool {
    process_parent_pids(descendant_pid, snapshot).contains(&ancestor_pid)
}

fn process_parent_pids(
    pid: u32,
    snapshot: &std::collections::HashMap<u32, ProcessSnapshotEntry>,
) -> Vec<u32> {
    let mut parent_pids = Vec::new();
    let mut current_pid = pid;
    let mut visited = std::collections::HashSet::from([pid]);
    while let Some(current) = snapshot.get(&current_pid) {
        let parent_pid = current.parent_pid;
        if parent_pid == 0 || !visited.insert(parent_pid) {
            break;
        }
        parent_pids.push(parent_pid);
        current_pid = parent_pid;
    }
    parent_pids
}

fn process_images_differ(
    a: &AudioProcessInfo,
    b: &AudioProcessInfo,
    snapshot: &std::collections::HashMap<u32, ProcessSnapshotEntry>,
) -> bool {
    match (
        process_image_for(a.pid, a.process_path.as_deref(), snapshot),
        process_image_for(b.pid, b.process_path.as_deref(), snapshot),
    ) {
        (Some(a_path), Some(b_path)) => !same_process_image(a_path, b_path),
        _ => {
            let Some(a_name) = process_identity_name(a, snapshot) else {
                return false;
            };
            let Some(b_name) = process_identity_name(b, snapshot) else {
                return false;
            };
            !a_name.eq_ignore_ascii_case(&b_name)
        }
    }
}

fn process_image_for<'a>(
    pid: u32,
    path: Option<&'a str>,
    snapshot: &'a std::collections::HashMap<u32, ProcessSnapshotEntry>,
) -> Option<&'a str> {
    path.or_else(|| {
        snapshot
            .get(&pid)
            .and_then(|entry| entry.image_name.as_deref())
    })
}

fn process_identity_name(
    process: &AudioProcessInfo,
    snapshot: &std::collections::HashMap<u32, ProcessSnapshotEntry>,
) -> Option<String> {
    process_image_for(process.pid, process.process_path.as_deref(), snapshot)
        .and_then(process_name_from_path)
        .or_else(|| {
            process
                .process_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
}

fn same_process_image(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    match (process_name_from_path(a), process_name_from_path(b)) {
        (Some(a_name), Some(b_name)) => a_name.eq_ignore_ascii_case(&b_name),
        _ => false,
    }
}

pub(crate) fn process_name_from_path(path: &str) -> Option<String> {
    let file_name = Path::new(path)
        .file_stem()
        .or_else(|| Path::new(path).file_name())?
        .to_string_lossy();
    let trimmed = file_name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::RelativeClock;
    use crate::traits::AudioSource;

    #[test]
    fn process_identity_rejects_a_reused_pid() {
        let pid = std::process::id();
        let identity = process_identity(pid).expect("query current process identity");
        assert!(identity.matches(pid));
        assert!(EndpointTarget::ProcessOutput { pid, identity }.process_identity_matches());

        let reused = ProcessIdentity {
            creation_time: identity.creation_time.wrapping_add(1),
        };
        assert!(!reused.matches(pid));
        assert!(!EndpointTarget::ProcessOutput {
            pid,
            identity: reused,
        }
        .process_identity_matches());
    }

    #[test]
    fn process_name_from_path_uses_executable_stem() {
        assert_eq!(
            process_name_from_path(r"C:\Program Files\Discord\Discord.exe").as_deref(),
            Some("Discord")
        );
        assert_eq!(process_name_from_path("").as_deref(), None);
    }

    #[test]
    fn process_group_root_collapses_same_executable_children() {
        let snapshot = std::collections::HashMap::from([
            (
                10724,
                ProcessSnapshotEntry {
                    parent_pid: 1000,
                    image_name: Some("Discord.exe".into()),
                },
            ),
            (
                18736,
                ProcessSnapshotEntry {
                    parent_pid: 10724,
                    image_name: Some("Discord.exe".into()),
                },
            ),
            (
                20732,
                ProcessSnapshotEntry {
                    parent_pid: 10724,
                    image_name: Some("Discord.exe".into()),
                },
            ),
        ]);

        assert_eq!(
            process_group_root(
                18736,
                Some(r"C:\Users\dain\AppData\Local\Discord\Discord.exe"),
                &snapshot
            ),
            10724
        );
        assert_eq!(
            process_group_root(
                20732,
                Some(r"C:\Users\dain\AppData\Local\Discord\Discord.exe"),
                &snapshot
            ),
            10724
        );
    }

    #[test]
    fn process_group_root_stops_at_different_executable_parent() {
        let snapshot = std::collections::HashMap::from([
            (
                10,
                ProcessSnapshotEntry {
                    parent_pid: 1,
                    image_name: Some("Launcher.exe".into()),
                },
            ),
            (
                20,
                ProcessSnapshotEntry {
                    parent_pid: 10,
                    image_name: Some("Game.exe".into()),
                },
            ),
        ]);

        assert_eq!(
            process_group_root(20, Some(r"C:\Games\Game.exe"), &snapshot),
            20
        );
    }

    #[test]
    fn process_candidates_drop_launcher_parent_when_child_also_has_audio() {
        let snapshot = std::collections::HashMap::from([
            (
                10,
                ProcessSnapshotEntry {
                    parent_pid: 1,
                    image_name: Some("steam.exe".into()),
                },
            ),
            (
                20,
                ProcessSnapshotEntry {
                    parent_pid: 10,
                    image_name: Some("SlayTheSpire2.exe".into()),
                },
            ),
        ]);
        let mut processes = vec![
            AudioProcessInfo {
                pid: 10,
                label: "steam".into(),
                process_name: Some("steam".into()),
                process_path: Some(r"C:\Program Files\Steam\steam.exe".into()),
            },
            AudioProcessInfo {
                pid: 20,
                label: "SlayTheSpire2".into(),
                process_name: Some("SlayTheSpire2".into()),
                process_path: Some(r"C:\Games\SlayTheSpire2.exe".into()),
            },
        ];

        drop_duplicate_process_tree_ancestors(&mut processes, &snapshot);

        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].label, "SlayTheSpire2");
    }

    #[test]
    fn process_candidates_drop_launcher_parent_when_parent_path_is_unknown() {
        let snapshot = std::collections::HashMap::from([
            (
                10,
                ProcessSnapshotEntry {
                    parent_pid: 1,
                    image_name: None,
                },
            ),
            (
                20,
                ProcessSnapshotEntry {
                    parent_pid: 10,
                    image_name: Some("SlayTheSpire2.exe".into()),
                },
            ),
        ]);
        let mut processes = vec![
            AudioProcessInfo {
                pid: 10,
                label: "steam".into(),
                process_name: Some("steam".into()),
                process_path: None,
            },
            AudioProcessInfo {
                pid: 20,
                label: "SlayTheSpire2".into(),
                process_name: Some("SlayTheSpire2".into()),
                process_path: Some(r"C:\Games\SlayTheSpire2.exe".into()),
            },
        ];

        drop_duplicate_process_tree_ancestors(&mut processes, &snapshot);

        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].label, "SlayTheSpire2");
    }

    #[test]
    fn process_loopback_uses_pull_mode_with_one_second_of_headroom() {
        let (flags, buffer_duration_100ns) = process_loopback_stream_config();

        assert_ne!(flags & AUDCLNT_STREAMFLAGS_LOOPBACK, 0);
        assert_ne!(flags & AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, 0);
        assert_eq!(
            flags & windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            "pull mode must not register an event that no capture thread waits on"
        );
        assert_eq!(buffer_duration_100ns, 10_000_000);
    }

    #[test]
    fn process_loopback_pull_mode_starts_polls_and_stops() {
        if std::env::var_os("CI").is_some() || !process_loopback_available() {
            eprintln!("SKIP: process loopback needs a supported interactive Windows session");
            return;
        }
        let clock = RelativeClock::new(crate::windows::qpc_now_ticks_100ns().unwrap());
        let mut source = match WasapiLoopback::start_process_output(clock, std::process::id(), 1.0)
        {
            Ok(source) => source,
            Err(error) => {
                eprintln!("SKIP: process loopback unavailable: {error}");
                return;
            }
        };

        std::thread::sleep(Duration::from_millis(100));
        source
            .poll_packets(f64::MAX)
            .expect("pull-mode process loopback poll");
        drop(source);
    }

    #[test]
    fn process_loopback_activation_timeout_is_typed() {
        let error = process_loopback_activation_timeout(42);
        assert!(error.is_timeout());
        assert!(matches!(
            error,
            CaptureError::OperationTimeout { after, .. }
                if after == PROCESS_LOOPBACK_ACTIVATION_TIMEOUT
        ));
    }
}
