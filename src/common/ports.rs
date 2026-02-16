//! Port detection — finds listening TCP ports for given PIDs.
//!
//! On macOS, uses libproc to inspect file descriptors per PID.
//! On other platforms, returns empty results.

/// A listening TCP port bound by a specific process.
#[derive(Debug, Clone)]
pub struct ListeningPort {
    pub port: u16,
    #[allow(dead_code)]
    pub pid: u32,
    pub process_name: String,
}

/// Get all listening TCP ports for the given set of PIDs.
#[cfg(target_os = "macos")]
pub fn get_listening_ports_for_pids(pids: &[u32], sys: &sysinfo::System) -> Vec<ListeningPort> {
    use libproc::libproc::file_info::{pidfdinfo, ListFDs};
    use libproc::libproc::net_info::SocketFDInfo;
    use libproc::libproc::proc_pid::{listpidinfo, pidinfo};
    use libproc::libproc::task_info::TaskAllInfo;

    // Constants for matching libproc enum discriminants
    const PROC_FD_TYPE_SOCKET: u32 = 2;
    const SOCKET_INFO_KIND_TCP: i32 = 2;
    const TCP_STATE_LISTEN: i32 = 1;

    let mut ports = Vec::new();
    let mut seen_ports = std::collections::HashSet::new();

    for &pid in pids {
        let ipid = pid as i32;

        // Get task info to know how many FDs to expect
        let nfiles = match pidinfo::<TaskAllInfo>(ipid, 0) {
            Ok(info) => info.pbsd.pbi_nfiles as usize,
            Err(_) => continue,
        };

        // List all file descriptors
        let fds = match listpidinfo::<ListFDs>(ipid, nfiles) {
            Ok(fds) => fds,
            Err(_) => continue,
        };

        // Get process name from sysinfo
        let process_name = sys
            .process(sysinfo::Pid::from_u32(pid))
            .map(|p| p.name().to_string_lossy().to_string())
            .unwrap_or_default();

        for fd in &fds {
            // Only look at socket FDs (ProcFDType::Socket == 2)
            if fd.proc_fdtype != PROC_FD_TYPE_SOCKET {
                continue;
            }

            // Get socket details
            let socket_info = match pidfdinfo::<SocketFDInfo>(ipid, fd.proc_fd) {
                Ok(info) => info,
                Err(_) => continue,
            };

            // Check if it's a TCP socket (SocketInfoKind::Tcp == 2)
            if socket_info.psi.soi_kind != SOCKET_INFO_KIND_TCP {
                continue;
            }

            // Access TCP-specific info (union field, requires unsafe)
            let tcp_info = unsafe { socket_info.psi.soi_proto.pri_tcp };

            // Check for LISTEN state (TcpSIState::Listen == 1)
            if tcp_info.tcpsi_state != TCP_STATE_LISTEN {
                continue;
            }

            // Extract local port (stored in network byte order, convert to host)
            let local_port = u16::from_be(tcp_info.tcpsi_ini.insi_lport as u16);

            // Skip port 0 and dedup
            if local_port == 0 || !seen_ports.insert(local_port) {
                continue;
            }

            ports.push(ListeningPort {
                port: local_port,
                pid,
                process_name: process_name.clone(),
            });
        }
    }

    // Sort by port number for stable display
    ports.sort_by_key(|p| p.port);
    ports
}

/// Stub for non-macOS platforms — returns empty results.
#[cfg(not(target_os = "macos"))]
pub fn get_listening_ports_for_pids(_pids: &[u32], _sys: &sysinfo::System) -> Vec<ListeningPort> {
    Vec::new()
}
