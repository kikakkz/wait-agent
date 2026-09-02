use std::env;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::SUCCESS,
    }
}

fn run() -> io::Result<()> {
    let event = env::args().nth(1).unwrap_or_default();
    let signal_socket = env::var("WAITAGENT_SIGNAL_SOCKET").unwrap_or_default();
    let socket_name = env::var("WAITAGENT_SOCKET_NAME").unwrap_or_default();
    let session_name = env::var("WAITAGENT_TARGET_SESSION_NAME").unwrap_or_default();
    let session_id = env::var("WAITAGENT_SESSION_ID")
        .or_else(|_| env::var("WAITAGENT_PANE_ID"))
        .unwrap_or_default();
    let token = env::var("WAITAGENT_AGENT_SIGNAL_TOKEN").unwrap_or_default();
    let mut agent = env::var("WAITAGENT_AGENT_NAME").unwrap_or_else(|_| "codex".to_string());
    if agent.is_empty() {
        agent = "codex".to_string();
    }

    if event.is_empty()
        || signal_socket.is_empty()
        || socket_name.is_empty()
        || session_name.is_empty()
        || session_id.is_empty()
        || token.is_empty()
    {
        return Ok(());
    }

    let mut payload = String::new();
    io::stdin().read_to_string(&mut payload)?;
    let payload = normalize_payload(&payload);
    let message = build_signal_json(
        &event,
        &socket_name,
        &session_name,
        &session_id,
        &token,
        &agent,
        &payload,
    );

    #[cfg(unix)]
    {
        UnixDatagram::unbound()?.send_to(message.as_bytes(), signal_socket)?;
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileA, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            OPEN_EXISTING,
        };

        let target_c = std::ffi::CString::new(signal_socket.as_str())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid target"))?;
        // SAFETY: `target_c` is a valid null-terminated pipe name.
        let handle = unsafe {
            CreateFileA(
                target_c.as_ptr() as *const u8,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let bytes = message.into_bytes();
        let mut written = 0u32;
        // SAFETY: `handle` is valid and `bytes` outlives the call.
        let write_ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        } != 0;
        // SAFETY: `handle` was created above and is no longer used after this.
        unsafe {
            CloseHandle(handle);
        }
        if !write_ok {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

fn normalize_payload(payload: &str) -> String {
    if payload.trim().is_empty() {
        "null".to_string()
    } else if serde_json::from_str::<serde_json::Value>(payload).is_ok() {
        payload.to_string()
    } else {
        json_string(payload)
    }
}

fn build_signal_json(
    event: &str,
    socket_name: &str,
    session_name: &str,
    session_id: &str,
    token: &str,
    agent: &str,
    payload: &str,
) -> String {
    format!(
        "{{\"version\":1,\"agent\":{},\"event\":{},\"socket\":{},\"session\":{},\"pane\":{},\"token\":{},\"payload\":{}}}",
        json_string(agent),
        json_string(event),
        json_string(socket_name),
        json_string(session_name),
        json_string(session_id),
        json_string(token),
        payload
    )
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{build_signal_json, json_string};

    #[test]
    fn json_string_escapes_control_characters() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }

    #[test]
    fn signal_json_includes_pane_field() {
        let json = build_signal_json("sig", "ratatui-1", "sess", "sess", "tok", "codex", "null");
        assert!(json.contains("\"pane\":\"sess\""));
    }

    #[test]
    fn signal_json_includes_session_field() {
        let json = build_signal_json("sig", "ratatui-1", "sess", "sess", "tok", "codex", "null");
        assert!(json.contains("\"session\":\"sess\""));
    }
}
