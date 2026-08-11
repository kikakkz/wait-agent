use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};

// Netlink connector constants.
const NETLINK_CONNECTOR: libc::c_int = 11;
const CN_IDX_PROC: u32 = 0x1;
const CN_VAL_PROC: u32 = 0x1;
const PROC_CN_MCAST_LISTEN: u32 = 0x1;

// proc_event.what values.
const PROC_EVENT_FORK: u32 = 0x0000_0001;
const PROC_EVENT_EXEC: u32 = 0x0000_0002;
const PROC_EVENT_UID: u32 = 0x0000_0004;
const PROC_EVENT_GID: u32 = 0x0000_0040;
const PROC_EVENT_SID: u32 = 0x0000_0080;
const PROC_EVENT_EXIT: u32 = 0x8000_0000;

/// Linux netlink connector process event source.
///
/// This type no longer spawns its own thread. The caller is expected to poll
/// [`Self::fd`] in a unified event loop and call [`Self::read_events`] when the
/// socket becomes readable.
pub(crate) struct LinuxProcessEventSource {
    socket_fd: OwnedFd,
}

impl LinuxProcessEventSource {
    /// Create and subscribe the netlink proc connector socket.
    pub fn new() -> Result<Self, LifecycleError> {
        let socket_fd = create_netlink_socket()?;
        subscribe_to_proc_events(&socket_fd)?;
        Ok(Self { socket_fd })
    }

    /// Read all currently available proc events from the socket into `out`.
    ///
    /// Returns the number of events appended to `out`. A return value of zero
    /// means the socket was readable but no complete events were available.
    pub fn read_events(&self, out: &mut Vec<ProcessEvent>) -> Result<usize, LifecycleError> {
        let mut buf = vec![0u8; 4096];
        let mut total = 0usize;

        loop {
            // SAFETY: `recv` on a valid netlink socket into a valid buffer.
            let n = unsafe {
                libc::recv(
                    self.socket_fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };

            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::Interrupted
                {
                    break;
                }
                return Err(LifecycleError::Io("netlink recv failed".to_string(), err));
            }
            if n == 0 {
                break;
            }

            let before = out.len();
            parse_netlink_buffer(&buf[..n as usize], out);
            total += out.len() - before;
        }

        Ok(total)
    }
}

impl AsRawFd for LinuxProcessEventSource {
    fn as_raw_fd(&self) -> RawFd {
        self.socket_fd.as_raw_fd()
    }
}

fn create_netlink_socket() -> Result<OwnedFd, LifecycleError> {
    // SAFETY: `socket` is a standard libc call with well-defined arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            NETLINK_CONNECTOR,
        )
    };
    if fd < 0 {
        return Err(LifecycleError::Io(
            "failed to create netlink connector socket".to_string(),
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: fd was just returned by a successful socket() call.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // Increase the receive buffer to reduce the chance of the kernel dropping
    // proc events under burst load. If the call fails we still proceed.
    let rcvbuf_size: libc::c_int = 1024 * 1024;
    unsafe {
        libc::setsockopt(
            owned.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf_size as *const _ as *const libc::c_void,
            std::mem::size_of_val(&rcvbuf_size) as libc::socklen_t,
        );
    }

    // SAFETY: `sockaddr_nl` is a C struct with no meaningful invariants for
    // unused padding fields; zeroing is the standard initialization pattern.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = 0; // kernel selects pid
    addr.nl_groups = 0;

    // SAFETY: `bind` on a freshly created netlink socket with a valid sockaddr.
    let rc = unsafe {
        libc::bind(
            owned.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(LifecycleError::Io(
            "failed to bind netlink connector socket".to_string(),
            std::io::Error::last_os_error(),
        ));
    }

    Ok(owned)
}

#[repr(C)]
struct CbId {
    idx: u32,
    val: u32,
}

/// Fixed-size view of the `struct cn_msg` header sent by the kernel.
/// The real kernel struct has a flexible `data[0]` member; we keep a 4-byte
/// placeholder so we can compute the header size and then read the variable
/// payload at the correct offset.
#[repr(C)]
struct CnMsg {
    id: CbId,
    seq: u32,
    ack: u32,
    len: u16,
    flags: u16,
    data: [u8; 4],
}

const CN_MSG_HEADER_SIZE: usize = std::mem::size_of::<CnMsg>() - std::mem::size_of::<[u8; 4]>();

fn subscribe_to_proc_events(fd: &OwnedFd) -> Result<(), LifecycleError> {
    let msg = CnMsg {
        id: CbId {
            idx: CN_IDX_PROC,
            val: CN_VAL_PROC,
        },
        seq: 0,
        ack: 0,
        len: 4,
        flags: 0,
        data: PROC_CN_MCAST_LISTEN.to_ne_bytes(),
    };

    let nlmsg_len = std::mem::size_of::<libc::nlmsghdr>() + std::mem::size_of::<CnMsg>();
    let nlmsg = libc::nlmsghdr {
        nlmsg_len: nlmsg_len as u32,
        nlmsg_type: libc::NLMSG_DONE as u16,
        nlmsg_flags: 0,
        nlmsg_seq: 0,
        nlmsg_pid: 0,
    };

    let mut iov = [
        libc::iovec {
            iov_base: &nlmsg as *const _ as *mut libc::c_void,
            iov_len: std::mem::size_of_val(&nlmsg),
        },
        libc::iovec {
            iov_base: &msg as *const _ as *mut libc::c_void,
            iov_len: std::mem::size_of_val(&msg),
        },
    ];

    // SAFETY: `sockaddr_nl` is a C struct with no meaningful invariants for
    // unused padding fields; zeroing is the standard initialization pattern.
    let mut dest: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    dest.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    dest.nl_pid = 0;
    dest.nl_groups = 0;

    let msghdr = libc::msghdr {
        msg_name: &dest as *const _ as *mut libc::c_void,
        msg_namelen: std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        msg_iov: iov.as_mut_ptr(),
        msg_iovlen: iov.len(),
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };

    // SAFETY: `sendmsg` on a valid netlink socket with a valid message.
    let rc = unsafe { libc::sendmsg(fd.as_raw_fd(), &msghdr, 0) };
    if rc < 0 {
        return Err(LifecycleError::Io(
            "failed to subscribe to netlink proc events".to_string(),
            std::io::Error::last_os_error(),
        ));
    }

    log_netlink_capability();
    Ok(())
}

/// Linux capability number for `CAP_NET_ADMIN`.
const CAP_NET_ADMIN: usize = 12;

/// Read effective capabilities from `/proc/self/status` and log whether
/// `CAP_NET_ADMIN` is set. Without this capability the netlink proc connector
/// may silently fail to deliver fork/exec/exit events.
fn log_netlink_capability() {
    let has_cap = read_proc_self_status_cap_eff()
        .map(|cap_eff| (cap_eff & (1u64 << CAP_NET_ADMIN)) != 0)
        .unwrap_or(false);

    if has_cap {
        ERROR_LOG.log("[process-monitor-linux] CAP_NET_ADMIN present; netlink proc events should be delivered".to_string());
    } else {
        ERROR_LOG.log_error(
            "[process-monitor-linux] CAP_NET_ADMIN missing; run: sudo setcap cap_net_admin+ep /path/to/waitagent".to_string(),
        );
    }
}

fn read_proc_self_status_cap_eff() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("CapEff:") {
            return u64::from_str_radix(value.trim(), 16).ok();
        }
    }
    None
}

#[repr(C)]
struct ProcEvent {
    what: u32,
    cpu: u32,
    timestamp_ns: u64,
    event_data: [u8; 16],
}

#[repr(C)]
struct ForkEvent {
    parent_pid: libc::pid_t,
    parent_tgid: libc::pid_t,
    child_pid: libc::pid_t,
    child_tgid: libc::pid_t,
}

#[repr(C)]
struct ExecEvent {
    process_pid: libc::pid_t,
    process_tgid: libc::pid_t,
}

#[repr(C)]
struct ExitEvent {
    process_pid: libc::pid_t,
    process_tgid: libc::pid_t,
    exit_code: u32,
    signal: u32,
}

fn parse_netlink_buffer(buf: &[u8], out: &mut Vec<ProcessEvent>) {
    let mut offset = 0;
    while offset + std::mem::size_of::<libc::nlmsghdr>() <= buf.len() {
        // SAFETY: We checked that the buffer is large enough for an nlmsghdr.
        let nlh = unsafe { &*(buf.as_ptr().add(offset) as *const libc::nlmsghdr) };
        let msg_len = nlh.nlmsg_len as usize;
        if msg_len < std::mem::size_of::<libc::nlmsghdr>() || offset + msg_len > buf.len() {
            break;
        }

        if nlh.nlmsg_type as u32 == libc::NLMSG_DONE as u32 {
            // Subscription ack or done message.
        } else if nlh.nlmsg_type as u32 == libc::NLMSG_ERROR as u32 {
            ERROR_LOG.log("[process-monitor-linux] netlink error message".to_string());
        } else {
            let cn_offset = offset + std::mem::size_of::<libc::nlmsghdr>();
            let cn_header_end = cn_offset + CN_MSG_HEADER_SIZE;
            if cn_header_end <= offset + msg_len {
                // SAFETY: Bounds checked above; we only read the fixed cn_msg header.
                let cn = unsafe { &*(buf.as_ptr().add(cn_offset) as *const CnMsg) };
                if cn.id.idx == CN_IDX_PROC && cn.id.val == CN_VAL_PROC {
                    let data_offset = cn_header_end;
                    let data_len = cn.len as usize;
                    if data_offset + data_len <= offset + msg_len
                        && data_len >= std::mem::size_of::<ProcEvent>()
                    {
                        // SAFETY: Bounds checked above and data is large enough for a proc_event.
                        let proc_event =
                            unsafe { &*(buf.as_ptr().add(data_offset) as *const ProcEvent) };
                        if let Some(event) = translate_proc_event(proc_event) {
                            out.push(event);
                        }
                    }
                }
            }
        }

        let aligned_len = align_netlink(msg_len);
        if aligned_len == 0 {
            break;
        }
        offset += aligned_len;
    }
}

fn align_netlink(len: usize) -> usize {
    len.div_ceil(4) * 4
}

fn translate_proc_event(proc_event: &ProcEvent) -> Option<ProcessEvent> {
    match proc_event.what {
        PROC_EVENT_FORK => {
            // SAFETY: event_data is the fork union member when what == FORK.
            let ev = unsafe { &*(proc_event.event_data.as_ptr() as *const ForkEvent) };
            Some(ProcessEvent::Fork {
                parent_pid: ev.parent_pid as u32,
                child_pid: ev.child_pid as u32,
            })
        }
        PROC_EVENT_EXEC => {
            // SAFETY: event_data is the exec union member when what == EXEC.
            let ev = unsafe { &*(proc_event.event_data.as_ptr() as *const ExecEvent) };
            // Netlink exec events do not carry argv. The command name will be
            // updated later from /proc when the ProcessMonitor processes the event.
            Some(ProcessEvent::Exec {
                pid: ev.process_pid as u32,
                argv0: String::new(),
                argv: Vec::new(),
            })
        }
        PROC_EVENT_EXIT => {
            // SAFETY: event_data is the exit union member when what == EXIT.
            let ev = unsafe { &*(proc_event.event_data.as_ptr() as *const ExitEvent) };
            Some(ProcessEvent::Exit {
                pid: ev.process_pid as u32,
                exit_code: if ev.signal == 0 {
                    Some(ev.exit_code as i32)
                } else {
                    None
                },
            })
        }
        PROC_EVENT_UID | PROC_EVENT_GID | PROC_EVENT_SID => None,
        _ => None,
    }
}
