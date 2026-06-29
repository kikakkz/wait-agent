use std::env;
use std::io::{self, Read};
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
    let pane_id = env::var("WAITAGENT_PANE_ID").unwrap_or_default();
    let token = env::var("WAITAGENT_AGENT_SIGNAL_TOKEN").unwrap_or_default();
    let mut agent = env::var("WAITAGENT_AGENT_NAME").unwrap_or_else(|_| "codex".to_string());
    if agent.is_empty() {
        agent = "codex".to_string();
    }

    if event.is_empty()
        || signal_socket.is_empty()
        || socket_name.is_empty()
        || session_name.is_empty()
        || pane_id.is_empty()
        || token.is_empty()
    {
        return Ok(());
    }

    let mut payload = String::new();
    io::stdin().read_to_string(&mut payload)?;
    let payload = if payload.trim().is_empty() {
        "null".to_string()
    } else {
        payload
    };
    let message = format!(
        "{{\"version\":1,\"agent\":{},\"event\":{},\"socket\":{},\"session\":{},\"pane\":{},\"token\":{},\"payload\":{}}}",
        json_string(&agent),
        json_string(&event),
        json_string(&socket_name),
        json_string(&session_name),
        json_string(&pane_id),
        json_string(&token),
        payload
    );
    UnixDatagram::unbound()?.send_to(message.as_bytes(), signal_socket)?;
    Ok(())
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
    use super::json_string;

    #[test]
    fn json_string_escapes_control_characters() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
