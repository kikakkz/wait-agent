//! A minimal async SMTP client for the magic-link mail (issue #131 v4),
//! hand-rolled on the existing tokio/rustls/base64 stack (plus `ring` and
//! `webpki-roots`, both already in the dependency tree — no new transitive
//! crates). The provider table only needs AUTH LOGIN over TLS-on-connect
//! (SMTPS) or STARTTLS, one MAIL/RCPT/DATA transaction, and CRLF framing
//! with dot-stuffing; anything richer belongs in a later revision.
//!
//! Component tests run against the scripted in-process stub in
//! [`stub`]; the web integration tests drive the same stub end to end.

use std::fmt;
use std::io;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::web::config::MailTls;

/// Upper bound on the whole conversation so a stuck server cannot hang a
/// login request forever.
const SMTP_IO_TIMEOUT: Duration = Duration::from_secs(15);

/// One plaintext email to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailMessage {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Errors of the SMTP conversation.
#[derive(Debug)]
pub enum SmtpError {
    Connect(io::Error),
    Tls(String),
    Timeout,
    UnexpectedReply { step: &'static str, reply: String },
    Utf8(String),
    Io(io::Error),
}

impl fmt::Display for SmtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => write!(f, "smtp connect failed: {error}"),
            Self::Tls(message) => write!(f, "smtp tls failed: {message}"),
            Self::Timeout => write!(f, "smtp conversation timed out"),
            Self::UnexpectedReply { step, reply } => {
                write!(f, "smtp {step} rejected: {reply}")
            }
            Self::Utf8(message) => write!(f, "smtp utf8 error: {message}"),
            Self::Io(error) => write!(f, "smtp io error: {error}"),
        }
    }
}

impl std::error::Error for SmtpError {}

impl From<io::Error> for SmtpError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Sends `message` through the endpoint. `auth_code` is the mailbox app
/// password / authorization code (v4).
pub async fn send(
    host: &str,
    port: u16,
    tls: MailTls,
    user: &str,
    auth_code: &str,
    message: &MailMessage,
) -> Result<(), SmtpError> {
    let tcp = tokio::time::timeout(SMTP_IO_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| SmtpError::Timeout)?
        .map_err(SmtpError::Connect)?;

    let mut stream = match tls {
        MailTls::Ssl => {
            let mut stream = SmtpStream::Tls(Box::new(wrap_tls(host, tcp).await?));
            greet(&mut stream).await?;
            stream
        }
        MailTls::None => {
            let mut stream = SmtpStream::Plain(tcp);
            greet(&mut stream).await?;
            stream
        }
        MailTls::StartTls => {
            let mut stream = SmtpStream::Plain(tcp);
            greet(&mut stream).await?;
            command(&mut stream, "STARTTLS", "starttls", 220).await?;
            let tcp = match stream {
                SmtpStream::Plain(tcp) => tcp,
                SmtpStream::Tls(_) => unreachable!("STARTTLS arm held a plaintext stream"),
            };
            let mut stream = SmtpStream::Tls(Box::new(wrap_tls(host, tcp).await?));
            // RFC 3207: the client MUST send EHLO again after the upgrade.
            ehlo(&mut stream).await?;
            stream
        }
    };

    auth_login(&mut stream, user, auth_code).await?;
    transfer(&mut stream, message).await?;
    let _ = command(&mut stream, "QUIT", "quit", 221).await;
    Ok(())
}

// --- transport ---

enum SmtpStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl SmtpStream {
    async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.write_all(bytes).await,
            Self::Tls(stream) => stream.write_all(bytes).await,
        }
    }

    async fn read_line(&mut self, buffer: &mut Vec<u8>) -> io::Result<usize> {
        loop {
            let byte = &mut [0u8; 1];
            let read = match self {
                Self::Plain(stream) => stream.read(byte).await?,
                Self::Tls(stream) => stream.read(byte).await?,
            };
            if read == 0 {
                return Ok(0);
            }
            let finished = byte[0] == b'\n';
            buffer.push(byte[0]);
            if finished {
                return Ok(buffer.len());
            }
        }
    }
}

async fn wrap_tls(
    host: &str,
    tcp: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, SmtpError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(std::sync::Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|error| SmtpError::Tls(error.to_string()))?;
    tokio::time::timeout(SMTP_IO_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| SmtpError::Timeout)?
        .map_err(|error| SmtpError::Tls(error.to_string()))
}

// --- conversation ---

/// Greeting plus the first EHLO, shared by the TLS-on-connect and
/// plaintext paths (STARTTLS does its own pre/post-upgrade sequence).
async fn greet(stream: &mut SmtpStream) -> Result<(), SmtpError> {
    expect(stream, "greeting", 220).await?;
    ehlo(stream).await
}

async fn read_reply(stream: &mut SmtpStream) -> Result<String, SmtpError> {
    let mut reply = String::new();
    loop {
        let mut line_bytes = Vec::new();
        let read = tokio::time::timeout(SMTP_IO_TIMEOUT, stream.read_line(&mut line_bytes))
            .await
            .map_err(|_| SmtpError::Timeout)??;
        if read == 0 {
            return Err(SmtpError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "smtp server closed the connection",
            )));
        }
        let line =
            String::from_utf8(line_bytes).map_err(|error| SmtpError::Utf8(error.to_string()))?;
        let finished = line.as_bytes().get(3) != Some(&b'-');
        reply.push_str(&line);
        if finished {
            return Ok(reply);
        }
    }
}

fn reply_code(reply: &str) -> Option<u32> {
    reply
        .as_bytes()
        .get(..3)
        .and_then(|digits| std::str::from_utf8(digits).ok())
        .and_then(|digits| digits.parse().ok())
}

async fn expect(
    stream: &mut SmtpStream,
    step: &'static str,
    code: u32,
) -> Result<String, SmtpError> {
    let reply = read_reply(stream).await?;
    if reply_code(&reply) != Some(code) {
        return Err(SmtpError::UnexpectedReply { step, reply });
    }
    Ok(reply)
}

async fn command(
    stream: &mut SmtpStream,
    command: &str,
    step: &'static str,
    code: u32,
) -> Result<String, SmtpError> {
    let line = format!("{command}\r\n");
    tokio::time::timeout(SMTP_IO_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .map_err(|_| SmtpError::Timeout)??;
    expect(stream, step, code).await
}

async fn ehlo(stream: &mut SmtpStream) -> Result<(), SmtpError> {
    command(stream, "EHLO waitagent", "ehlo", 250).await?;
    Ok(())
}

async fn auth_login(stream: &mut SmtpStream, user: &str, auth_code: &str) -> Result<(), SmtpError> {
    let step = "auth";
    command(stream, "AUTH LOGIN", step, 334).await?;
    let user_b64 = BASE64_STANDARD.encode(user.as_bytes());
    let line = format!("{user_b64}\r\n");
    tokio::time::timeout(SMTP_IO_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .map_err(|_| SmtpError::Timeout)??;
    expect(stream, step, 334).await?;
    let code_b64 = BASE64_STANDARD.encode(auth_code.as_bytes());
    let line = format!("{code_b64}\r\n");
    tokio::time::timeout(SMTP_IO_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .map_err(|_| SmtpError::Timeout)??;
    expect(stream, step, 235).await?;
    Ok(())
}

/// RFC 5321 DATA framing: CRLF line endings, dot-stuffing, `<CRLF>.<CRLF>`.
fn frame_data(message: &MailMessage) -> Vec<u8> {
    let mut out = String::with_capacity(message.body.len() + 64);
    out.push_str(&format!("From: <{}>\r\n", message.from));
    out.push_str(&format!("To: <{}>\r\n", message.to));
    out.push_str(&format!("Subject: {}\r\n", message.subject));
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("\r\n");
    for line in message.body.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out.into_bytes()
}

async fn transfer(stream: &mut SmtpStream, message: &MailMessage) -> Result<(), SmtpError> {
    command(
        stream,
        &format!("MAIL FROM:<{}>", message.from),
        "mail from",
        250,
    )
    .await?;
    command(stream, &format!("RCPT TO:<{}>", message.to), "rcpt to", 250).await?;
    command(stream, "DATA", "data", 354).await?;
    let framed = frame_data(message);
    tokio::time::timeout(SMTP_IO_TIMEOUT, stream.write_all(&framed))
        .await
        .map_err(|_| SmtpError::Timeout)??;
    expect(stream, "data end", 250).await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod stub;

#[cfg(test)]
mod tests {
    use super::stub::StubSmtp;
    use super::*;

    #[tokio::test]
    async fn sends_a_message_over_plain_smtp_against_the_stub() {
        let stub = StubSmtp::start().await;
        let message = MailMessage {
            from: "admin@example.com".to_string(),
            to: "admin@example.com".to_string(),
            subject: "waitagent magic link".to_string(),
            body: "open https://dash.example/auth/magic?token=abc\n\n-- waitagent".to_string(),
        };
        send(
            "127.0.0.1",
            stub.port(),
            MailTls::None,
            "admin@example.com",
            "auth-code",
            &message,
        )
        .await
        .expect("send should succeed");

        let transcript = stub.transcript();
        transcript.assert_auth("admin@example.com", "auth-code");
        transcript.assert_mail_from("admin@example.com");
        transcript.assert_rcpt_to("admin@example.com");
        let body = transcript.data_body().expect("a DATA body");
        assert!(body.contains("Subject: waitagent magic link"), "{body}");
        assert!(
            body.contains("https://dash.example/auth/magic?token=abc"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn dot_stuffed_lines_and_final_dot_are_framed_correctly() {
        let message = MailMessage {
            from: "a@b.c".to_string(),
            to: "a@b.c".to_string(),
            subject: "s".to_string(),
            body: ".leading dot\nnormal line\n.trailing too".to_string(),
        };
        let framed = String::from_utf8(frame_data(&message)).expect("framing is utf8");
        assert!(framed.contains("\r\n..leading dot\r\n"), "{framed}");
        assert!(framed.contains("\r\nnormal line\r\n"), "{framed}");
        assert!(framed.ends_with("\r\n.\r\n"), "{framed}");
    }

    #[tokio::test]
    async fn a_refusing_server_is_a_structured_error() {
        let mut stub = StubSmtp::start().await;
        stub.refuse_rcpt();
        let message = MailMessage {
            from: "a@b.c".to_string(),
            to: "a@b.c".to_string(),
            subject: "s".to_string(),
            body: "b".to_string(),
        };
        let error = send(
            "127.0.0.1",
            stub.port(),
            MailTls::None,
            "a@b.c",
            "x",
            &message,
        )
        .await
        .expect_err("a 550 RCPT must fail the send");
        match error {
            SmtpError::UnexpectedReply { step, reply } => {
                assert_eq!(step, "rcpt to");
                assert!(reply.starts_with("550"), "{reply}");
            }
            other => panic!("expected UnexpectedReply, got {other}"),
        }
    }
}
