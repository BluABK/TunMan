//! The OS-specific bits: the app icon, the single-instance guard, killing a
//! process tree, and reading the TCP connection table.
//!
//! Everything Windows-only has a non-Windows stub returning nothing, so the
//! rest of the app never needs a `cfg`.

use std::net::TcpListener;

/// Loopback port used as both a lock and a doorbell for the single-instance
/// guard. Deliberately not StreamArchiver's 47836 — two apps sharing a port
/// would each think the other was itself.
const SINGLE_INSTANCE_PORT: u16 = 47921;

/// Holds the loopback socket for the lifetime of the process. Bind it to a
/// *named* variable at the top of `main`; `let _ = ...` drops it immediately
/// and releases the lock.
pub struct InstanceGuard;

/// Take the single-instance lock, or `None` if another TunMan already holds it.
///
/// On success the listener moves into a background thread that treats every
/// incoming connection as "someone tried to launch me again, show the window".
/// The connection itself is the whole message; there is no payload.
///
/// **Never `try_clone` this listener.** On Windows the clone is created
/// inheritable, so it leaks into every child process spawned afterwards — and
/// since TunMan spawns long-lived `ssh.exe` children, a leaked handle would
/// keep the port bound after TunMan exits and the next launch would mistake its
/// own dead socket for a running instance. (StreamArchiver shipped exactly that
/// bug and had to move ports to escape the leaked handles.)
pub fn acquire_single_instance(
    on_second_launch: impl Fn() + Send + 'static,
) -> Option<InstanceGuard> {
    let listener = TcpListener::bind(("127.0.0.1", SINGLE_INSTANCE_PORT)).ok()?;
    std::thread::Builder::new()
        .name("instance-doorbell".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                drop(stream);
                on_second_launch();
            }
        })
        .ok()?;
    Some(InstanceGuard)
}

/// Ring the running instance's doorbell so it surfaces its window.
pub fn notify_running_instance() {
    let _ = std::net::TcpStream::connect(("127.0.0.1", SINGLE_INSTANCE_PORT));
}

/// TunMan's icon: a dark tunnel portal on cyan.
///
/// Procedural, so there is no asset to ship or lose. The shape and palette are
/// chosen to be unmistakable next to StreamArchiver's purple tile with a red
/// record dot — the two sit in the same tray, and a glanceable difference at
/// 16 px matters more here than detail.
pub fn app_icon_rgba() -> (Vec<u8>, u32, u32) {
    const N: u32 = 32;
    const CYAN: [u8; 4] = [0x22, 0xd3, 0xee, 0xff];
    const DARK: [u8; 4] = [0x0b, 0x16, 0x20, 0xff];
    const CORNER: f32 = 5.0;
    // Portal: a semicircle sitting on a rectangle, centred and open at the
    // bottom edge, so it still reads as an arch when scaled down.
    const AX: f32 = 16.0;
    const AY: f32 = 15.0;
    const AR: f32 = 7.0;
    const FLOOR: f32 = 27.0;

    let mut px = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let i = ((y * N + x) * 4) as usize;
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);

            // Rounded-corner mask: clamp the point into the inner rect and
            // measure how far outside it fell.
            let cx = fx.clamp(CORNER, N as f32 - CORNER);
            let cy = fy.clamp(CORNER, N as f32 - CORNER);
            if ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() > CORNER {
                continue; // transparent
            }

            let in_arch = if fy < AY {
                ((fx - AX).powi(2) + (fy - AY).powi(2)).sqrt() <= AR
            } else {
                (fx - AX).abs() <= AR && fy <= FLOOR
            };
            px[i..i + 4].copy_from_slice(if in_arch { &DARK } else { &CYAN });
        }
    }
    (px, N, N)
}

/// The same icon as a tray icon.
pub fn tray_icon_image() -> anyhow::Result<tray_icon::Icon> {
    let (rgba, w, h) = app_icon_rgba();
    Ok(tray_icon::Icon::from_rgba(rgba, w, h)?)
}

/// Where the Start Menu shortcut goes:
/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\TunMan.lnk`.
///
/// The per-user Start Menu, not the machine-wide one — writing to
/// `%ProgramData%` needs elevation, and TunMan is a per-user app.
pub fn start_menu_shortcut_path() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        std::path::Path::new(&appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("TunMan.lnk"),
    )
}

/// Create or overwrite the Start Menu shortcut, pointing at the running exe.
///
/// **Overwriting every time is the point.** TunMan is run from wherever it was
/// built or unpacked, and a shortcut left pointing at a moved or renamed binary
/// is worse than none — it fails silently from the Start Menu while the app
/// works fine when launched directly. Rewriting it on each run means the
/// shortcut always points at whatever is actually running.
#[cfg(windows)]
pub fn create_start_menu_shortcut() -> Result<std::path::PathBuf, String> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, Interface, PCWSTR};

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let lnk = start_menu_shortcut_path().ok_or("APPDATA is not set")?;
    if let Some(dir) = lnk.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }

    unsafe {
        // May return S_FALSE (already initialised on this thread), which is
        // success — only balance the uninit when it actually succeeded, or a
        // caller that had COM up first would have it torn down underneath.
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> Result<(), String> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("ShellLink: {e}"))?;
            link.SetPath(PCWSTR(HSTRING::from(exe.as_os_str()).as_ptr()))
                .map_err(|e| format!("SetPath: {e}"))?;
            if let Some(parent) = exe.parent() {
                link.SetWorkingDirectory(PCWSTR(HSTRING::from(parent.as_os_str()).as_ptr()))
                    .map_err(|e| format!("SetWorkingDirectory: {e}"))?;
            }
            link.SetDescription(PCWSTR(
                HSTRING::from("TunMan — SSH tunnels, mounts and sync").as_ptr(),
            ))
            .map_err(|e| format!("SetDescription: {e}"))?;
            let file: IPersistFile = link.cast().map_err(|e| format!("IPersistFile: {e}"))?;
            file.Save(PCWSTR(HSTRING::from(lnk.as_os_str()).as_ptr()), true)
                .map_err(|e| format!("Save: {e}"))?;
            Ok(())
        })();
        if com.is_ok() {
            CoUninitialize();
        }
        result?;
    }
    Ok(lnk)
}

#[cfg(not(windows))]
pub fn create_start_menu_shortcut() -> Result<std::path::PathBuf, String> {
    Err("Start Menu shortcuts are Windows-only".into())
}

/// Remove the shortcut. A missing one is success — the desired end state is
/// "there is no shortcut", and it already holds.
pub fn remove_start_menu_shortcut() -> Result<(), String> {
    let Some(lnk) = start_menu_shortcut_path() else { return Ok(()) };
    match std::fs::remove_file(&lnk) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("removing {}: {e}", lnk.display())),
    }
}

/// Whether the shortcut exists and points at the running exe.
///
/// Only the existence is checked, not the target: reading a `.lnk` back means
/// another round of COM for a cosmetic label, and the shortcut is rewritten on
/// every run anyway.
pub fn start_menu_shortcut_exists() -> bool {
    start_menu_shortcut_path().is_some_and(|p| p.exists())
}

/// One process from the OS process list.
#[derive(Clone, Debug)]
pub struct SnapProc {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
}

#[cfg(windows)]
mod imp {
    use super::SnapProc;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows::Win32::Networking::WinSock::AF_INET;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    /// A job object that kills everything in it when the last handle closes —
    /// which happens when this process dies, however it dies.
    ///
    /// `kill_on_drop` only runs if the runtime is dropped in an orderly way. A
    /// `taskkill /F`, a panic-abort or a crash skips every destructor, and the
    /// children survive: measured 2026-08-31, killing TunMan outright left an
    /// `rclone mount` running with its drive letter still claimed, which is
    /// exactly the orphan the whole design is meant to prevent. The job object
    /// is enforced by the kernel and needs no cooperation from us.
    static KILL_JOB: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();

    fn kill_job() -> Option<HANDLE> {
        let raw = KILL_JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(None, None).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .ok()?;
            Some(job.0 as usize)
        });
        raw.map(|h| HANDLE(h as *mut std::ffi::c_void))
    }

    /// Put a child in the kill-on-exit job.
    ///
    /// Best effort: a child that cannot be assigned still works, it just loses
    /// the guarantee that it dies with us.
    pub fn adopt_child(handle: isize) {
        let Some(job) = kill_job() else { return };
        unsafe {
            let _ = AssignProcessToJobObject(job, HANDLE(handle as *mut std::ffi::c_void));
        }
    }

    pub fn process_tree_snapshot() -> Vec<SnapProc> {
        let mut out = Vec::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return out;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                    out.push(SnapProc {
                        pid: entry.th32ProcessID,
                        parent: entry.th32ParentProcessID,
                        name: String::from_utf16_lossy(&entry.szExeFile[..len]),
                    });
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    /// Terminate `pid` and every descendant, children first.
    ///
    /// Children first is the point: killing the parent first can leave an
    /// orphan whose parent id now refers to a dead process, so a second pass
    /// can no longer find it. ssh spawns helpers (`ProxyCommand`, askpass), and
    /// a bare `child.kill()` leaves those running.
    pub fn kill_process_tree(pid: u32) {
        let snap = process_tree_snapshot();
        let mut order = vec![pid];
        let mut i = 0;
        while i < order.len() {
            let parent = order[i];
            for p in snap.iter().filter(|p| p.parent == parent && p.pid != parent) {
                if !order.contains(&p.pid) {
                    order.push(p.pid);
                }
            }
            i += 1;
        }
        for pid in order.into_iter().rev() {
            unsafe {
                if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid)
                    && h != HANDLE::default()
                {
                    let _ = TerminateProcess(h, 1);
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    /// One row of the system TCP table.
    pub struct TcpRow {
        pub local_port: u16,
        pub remote_addr: u32,
        pub remote_port: u16,
        pub pid: u32,
        pub state: u32,
    }

    /// Every IPv4 TCP connection with its owning process id — the same data
    /// `netstat -ano` prints.
    pub fn tcp_table() -> Vec<TcpRow> {
        let mut out = Vec::new();
        unsafe {
            let mut size: u32 = 0;
            // First call sizes the buffer; it is expected to "fail".
            let _ = GetExtendedTcpTable(
                None,
                &mut size,
                false,
                AF_INET.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if size == 0 {
                return out;
            }
            let mut buf = vec![0u8; size as usize];
            let rc = GetExtendedTcpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut size,
                false,
                AF_INET.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if rc != 0 {
                return out;
            }
            let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
            let rows =
                std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
            for r in rows {
                out.push(TcpRow {
                    local_port: ntohs(r.dwLocalPort),
                    remote_addr: r.dwRemoteAddr,
                    remote_port: ntohs(r.dwRemotePort),
                    pid: r.dwOwningPid,
                    state: r.dwState,
                });
            }
        }
        out
    }

    /// The table stores ports in network byte order in the low 16 bits of a
    /// `u32`, so they need swapping before they mean anything.
    fn ntohs(port: u32) -> u16 {
        (((port & 0xff) << 8) | ((port >> 8) & 0xff)) as u16
    }
}

#[cfg(not(windows))]
mod imp {
    use super::SnapProc;

    pub fn adopt_child(_handle: isize) {}

    pub fn process_tree_snapshot() -> Vec<SnapProc> {
        Vec::new()
    }

    pub fn kill_process_tree(pid: u32) {
        let _ = std::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
    }

    pub struct TcpRow {
        pub local_port: u16,
        pub remote_addr: u32,
        pub remote_port: u16,
        pub pid: u32,
        pub state: u32,
    }

    pub fn tcp_table() -> Vec<TcpRow> {
        Vec::new()
    }
}

pub use imp::{adopt_child, kill_process_tree, process_tree_snapshot, tcp_table};

/// `MIB_TCP_STATE_ESTAB` — the only state that means traffic can flow.
pub const TCP_ESTABLISHED: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    /// The icon has to be a valid RGBA buffer of the size it claims, or
    /// `Icon::from_rgba` rejects it at startup and the app comes up with no
    /// tray icon at all.
    #[test]
    fn the_icon_is_well_formed_and_not_blank() {
        let (px, w, h) = app_icon_rgba();
        assert_eq!(px.len(), (w * h * 4) as usize);
        assert!(px.chunks(4).any(|p| p[3] > 0), "icon is fully transparent");

        // Both the tile and the portal must be present — a solid square would
        // be indistinguishable from any other tray icon at 16 px.
        let colours: std::collections::HashSet<[u8; 4]> =
            px.chunks(4).filter(|p| p[3] > 0).map(|p| [p[0], p[1], p[2], p[3]]).collect();
        assert_eq!(colours.len(), 2, "expected exactly the tile and the portal");

        // Corners rounded off, so it doesn't read as a plain block.
        assert_eq!(px[3], 0, "top-left corner should be transparent");
    }
}
