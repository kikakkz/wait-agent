//! A scripted in-process SMTP server for the auth tests: speaks just
//! enough ESMTP (greeting, multiline EHLO, AUTH LOGIN, MAIL/RCPT/DATA with
//! dot-unstuffing, QUIT) to exercise the client end to end, and records the
//! conversation for assertions. Deliberately plaintext — it only ever
//! listens on loopback ephemeral ports inside tests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One recorded SMTP conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    pub auth_user: Option<String>,
    pub auth_code: Option<String>,
    pub mail_from: Option<String>,
    pub rcpt_to: Option<String>,
    pub data: Option<String>,
    pub quit: bool,
}

impl Transcript {
    pub fn assert_auth(&self, user: &str, code: &str) {
        assert_eq!(
            self.auth_user.as_deref(),
            Some(user),
            "AUTH LOGIN user: {:?}",
            self.auth_user
        );
        assert_eq!(
            self.auth_code.as_deref(),
            Some(code),
            "AUTH LOGIN password: {:?}",
            self.auth_code
        );
    }

    pub fn assert_mail_from(&self, from: &str) {
        assert_eq!(self.mail_from.as_deref(), Some(from));
    }

    pub fn assert_rcpt_to(&self, to: &str) {
        assert_eq!(self.rcpt_to.as_deref(), Some(to));
    }

    pub fn data_body(&self) -> Option<&str> {
        self.data.as_deref()
    }
}

/// A running stub server. Drop stops accepting; spawned tasks end when
/// their connections close.
pub struct StubSmtp {
    port: u16,
    transcript: Arc<Mutex<Transcript>>,
    refuse_rcpt: Arc<AtomicBool>,
}

impl StubSmtp {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stub smtp should bind");
        let port = listener.local_addr().expect("addr").port();
        let transcript = Arc::new(Mutex::new(Transcript::default()));
        let refuse_rcpt = Arc::new(AtomicBool::new(false));
        let task_transcript = transcript.clone();
        let task_refuse = refuse_rcpt.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let transcript = task_transcript.clone();
                let refuse = task_refuse.clone();
                tokio::spawn(async move {
                    serve_connection(&mut stream, transcript, refuse).await;
                });
            }
        });
        Self {
            port,
            transcript,
            refuse_rcpt,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Makes the next RCPT TO answers 550 (a refused mailbox).
    pub fn refuse_rcpt(&mut self) {
        self.refuse_rcpt.store(true, Ordering::SeqCst);
    }

    pub fn transcript(&self) -> Transcript {
        self.transcript
            .lock()
            .expect("stub transcript lock poisoned")
            .clone()
    }

    /// Waits until a DATA body landed (the integration tests drive the
    /// whole flow through the real HTTP handler, so this is normally
    /// already true by the time the POST answers).
    pub async fn wait_for_data(&self) -> Transcript {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let transcript = self.transcript();
            if transcript.data.is_some() {
                return transcript;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stub smtp should receive a DATA body"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn serve_connection(
    stream: &mut tokio::net::TcpStream,
    transcript: Arc<Mutex<Transcript>>,
    refuse_rcpt: Arc<AtomicBool>,
) {
    send_reply(stream, b"220 stub ESMTP\r\n").await;
    let mut line = String::new();
    loop {
        line.clear();
        let read = read_crlf_line(stream, &mut line).await;
        if read.is_err() || line.is_empty() {
            return;
        }
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            send_reply(stream, b"250-stub greets you\r\n250 AUTH LOGIN\r\n").await;
        } else if upper.starts_with("AUTH LOGIN") {
            send_reply(stream, b"334 VXNlcm5hbWU6\r\n").await;
            let user_b64 = match read_crlf_line(stream, &mut line).await {
                Ok(_) => line.trim().to_string(),
                Err(_) => return,
            };
            send_reply(stream, b"334 UGFzc3dvcmQ6\r\n").await;
            let code_b64 = match read_crlf_line(stream, &mut line).await {
                Ok(_) => line.trim().to_string(),
                Err(_) => return,
            };
            {
                let mut guard = transcript.lock().expect("transcript lock");
                guard.auth_user = BASE64_STANDARD
                    .decode(user_b64.as_bytes())
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                guard.auth_code = BASE64_STANDARD
                    .decode(code_b64.as_bytes())
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok());
            }
            send_reply(stream, b"235 authenticated\r\n").await;
        } else if upper.starts_with("MAIL FROM:") {
            let rest = &line["MAIL FROM:".len()..];
            transcript.lock().expect("transcript lock").mail_from =
                Some(rest.trim().trim_matches('<').trim_matches('>').to_string());
            send_reply(stream, b"250 ok\r\n").await;
        } else if upper.starts_with("RCPT TO:") {
            if refuse_rcpt.load(Ordering::SeqCst) {
                send_reply(stream, b"550 mailbox refused\r\n").await;
            } else {
                let rest = &line["RCPT TO:".len()..];
                transcript.lock().expect("transcript lock").rcpt_to =
                    Some(rest.trim().trim_matches('<').trim_matches('>').to_string());
                send_reply(stream, b"250 ok\r\n").await;
            }
        } else if upper.starts_with("DATA") {
            send_reply(stream, b"354 end with <CRLF>.<CRLF>\r\n").await;
            let mut body = String::new();
            loop {
                match read_crlf_line(stream, &mut line).await {
                    Ok(_) => {}
                    Err(_) => return,
                }
                if line == "." {
                    break;
                }
                let unstuffed = line.strip_prefix('.').unwrap_or(&line);
                body.push_str(unstuffed);
                body.push('\n');
            }
            transcript.lock().expect("transcript lock").data = Some(body);
            send_reply(stream, b"250 queued\r\n").await;
        } else if upper.starts_with("QUIT") {
            transcript.lock().expect("transcript lock").quit = true;
            send_reply(stream, b"221 bye\r\n").await;
            return;
        } else if upper.starts_with("RSET") || upper.starts_with("NOOP") {
            send_reply(stream, b"250 ok\r\n").await;
        } else if upper.starts_with("STARTTLS") {
            send_reply(stream, b"454 TLS not available on the stub\r\n").await;
        } else {
            send_reply(stream, b"502 command not implemented\r\n").await;
        }
    }
}

async fn read_crlf_line(
    stream: &mut tokio::net::TcpStream,
    out: &mut String,
) -> Result<(), std::io::Error> {
    out.clear();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        if byte[0] == b'\n' {
            while out.ends_with('\r') {
                out.pop();
            }
            return Ok(());
        }
        out.push(byte[0] as char);
    }
}

async fn send_reply(stream: &mut tokio::net::TcpStream, bytes: &[u8]) {
    let _ = stream.write_all(bytes).await;
}
