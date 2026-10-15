//! SSH connect proxy (issue #201): dial SSH through a SOCKS5 or HTTP
//! CONNECT proxy. The proxy address reuses the active remote-install proxy
//! profile (`all_proxy` preferred, `https_proxy` fallback — see
//! [`RemoteInstallProxyConfig::ssh_proxy`]).
//!
//! v1 scope: no proxy user/password authentication (an authenticated proxy
//! fails with a typed, actionable error) and no LAN/no_proxy exclusions;
//! the remote host name is always resolved by the proxy (SOCKS5
//! ATYP=domainname / HTTP CONNECT), so the authority never needs local DNS
//! for the target.

use std::fmt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_NO_AUTHENTICATION: u8 = 0x00;
const SOCKS5_COMMAND_CONNECT: u8 = 0x01;
const SOCKS5_ADDRESS_DOMAINNAME: u8 = 0x03;
/// SOCKS5 reply field is one byte; the longest supported domain is 255.
const MAX_DOMAIN_LEN: usize = 255;
/// HTTP CONNECT response header read cap (bytes).
const MAX_HTTP_RESPONSE_HEADER: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshProxyScheme {
    Socks5,
    Http,
}

impl SshProxyScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Socks5 => "socks5",
            Self::Http => "http",
        }
    }
}

/// Proxy endpoint for an SSH connect. Newtype over the parsed pieces so the
/// scheme is validated once and the dial sites never re-parse URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProxy {
    pub scheme: SshProxyScheme,
    pub host: String,
    pub port: u16,
}

impl SshProxy {
    /// Parses `scheme://host:port`. Accepted schemes: `socks5`, `socks5h`
    /// (resolved remotely, same as `socks5` here), `http`, `https` (plain
    /// HTTP CONNECT). Anything else — an unsupported scheme, a missing
    /// port, an empty host, or a full-width colon — is a typed error naming
    /// the offending value.
    pub fn parse(url: &str) -> Result<Self, SshProxyError> {
        if url.contains('：') {
            return Err(SshProxyError::InvalidUrl(format!(
                "proxy url `{url}` contains a full-width colon (：); use ASCII ':'"
            )));
        }
        let (scheme, rest) = url.split_once("://").ok_or_else(|| {
            SshProxyError::InvalidUrl(format!(
                "proxy url `{url}` lacks a scheme (expected socks5://, socks5h://, http:// or https://)"
            ))
        })?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "socks5" | "socks5h" => SshProxyScheme::Socks5,
            "http" | "https" => SshProxyScheme::Http,
            other => {
                return Err(SshProxyError::InvalidUrl(format!(
                    "unsupported proxy scheme `{other}` in `{url}` (expected socks5, socks5h, http or https)"
                )));
            }
        };
        let rest = rest.trim_end_matches('/');
        let (host, port_text) = rest.rsplit_once(':').ok_or_else(|| {
            SshProxyError::InvalidUrl(format!(
                "proxy url `{url}` lacks a port (expected {}://host:port)",
                scheme.as_str()
            ))
        })?;
        if host.is_empty() {
            return Err(SshProxyError::InvalidUrl(format!(
                "proxy url `{url}` has an empty host"
            )));
        }
        if host.contains(':') {
            return Err(SshProxyError::InvalidUrl(format!(
                "proxy url `{url}` has an IPv6 host, which issue #201 v1 does not support"
            )));
        }
        let port = port_text.parse::<u16>().map_err(|_| {
            SshProxyError::InvalidUrl(format!(
                "proxy url `{url}` has an invalid port `{port_text}`"
            ))
        })?;
        Ok(Self {
            scheme,
            host: host.to_string(),
            port,
        })
    }

    pub fn label(&self) -> String {
        format!("{}://{}:{}", self.scheme.as_str(), self.host, self.port)
    }
}

impl fmt::Display for SshProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

#[derive(Debug)]
pub enum SshProxyError {
    /// The configured proxy URL cannot be used for SSH connect dialing.
    InvalidUrl(String),
    /// TCP connect to the proxy itself failed.
    ConnectionFailed {
        proxy: String,
        source: std::io::Error,
    },
    /// The proxy handshake did not complete as the protocol requires.
    HandshakeFailed {
        proxy: String,
        target: String,
        detail: String,
    },
    /// The proxy answered but refused the tunnel.
    ProxyRefused {
        proxy: String,
        target: String,
        detail: String,
    },
}

impl fmt::Display for SshProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(message) => write!(f, "{message}"),
            Self::ConnectionFailed { proxy, source } => {
                write!(f, "cannot reach SSH proxy {proxy}: {source}")
            }
            Self::HandshakeFailed {
                proxy,
                target,
                detail,
            } => write!(
                f,
                "SSH proxy {proxy} handshake for {target} failed: {detail}"
            ),
            Self::ProxyRefused {
                proxy,
                target,
                detail,
            } => write!(
                f,
                "SSH proxy {proxy} refused the tunnel to {target}: {detail}"
            ),
        }
    }
}

impl std::error::Error for SshProxyError {}

/// Opens a TCP stream to `target_host:target_port` through `proxy`. The
/// target host name is sent to the proxy unresolved; DNS happens at the
/// proxy side. Runs on the caller's async context (the SSH executor builds a
/// per-exec current-thread runtime, issue #201 m07 note: no shared state, no
/// locks).
pub async fn open_proxy_stream(
    proxy: &SshProxy,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream, SshProxyError> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|source| SshProxyError::ConnectionFailed {
            proxy: proxy.label(),
            source,
        })?;
    match proxy.scheme {
        SshProxyScheme::Socks5 => {
            socks5_connect(&mut stream, proxy, target_host, target_port).await?;
        }
        SshProxyScheme::Http => {
            http_connect(&mut stream, proxy, target_host, target_port).await?;
        }
    }
    Ok(stream)
}

/// SOCKS5 no-authentication CONNECT. Frames are read with `read_exact` so a
/// short or torn read never masquerades as a parsed reply.
async fn socks5_connect<S>(
    stream: &mut S,
    proxy: &SshProxy,
    target_host: &str,
    target_port: u16,
) -> Result<(), SshProxyError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let target = format!("{target_host}:{target_port}");
    let handshake = async {
        // Greeting: VER 5, one method, no authentication.
        stream
            .write_all(&[SOCKS5_VERSION, 0x01, SOCKS5_NO_AUTHENTICATION])
            .await?;
        let mut greeting = [0_u8; 2];
        stream.read_exact(&mut greeting).await?;
        if greeting[0] != SOCKS5_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected SOCKS5 greeting version 0x{:02x}", greeting[0]),
            ));
        }
        if greeting[1] != SOCKS5_NO_AUTHENTICATION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "proxy demands authentication method 0x{:02x}; authenticated proxies are not supported (issue #201 v1)",
                    greeting[1]
                ),
            ));
        }
        // CONNECT with ATYP=domainname: the proxy resolves the target.
        let domain = target_host.as_bytes();
        if domain.len() > MAX_DOMAIN_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "target host name is {} bytes (max {MAX_DOMAIN_LEN})",
                    domain.len()
                ),
            ));
        }
        let mut request = Vec::with_capacity(7 + domain.len());
        request.extend_from_slice(&[
            SOCKS5_VERSION,
            SOCKS5_COMMAND_CONNECT,
            0x00,
            SOCKS5_ADDRESS_DOMAINNAME,
            domain.len() as u8,
        ]);
        request.extend_from_slice(domain);
        request.extend_from_slice(&target_port.to_be_bytes());
        stream.write_all(&request).await?;
        let mut head = [0_u8; 4];
        stream.read_exact(&mut head).await?;
        if head[0] != SOCKS5_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected SOCKS5 reply version 0x{:02x}", head[0]),
            ));
        }
        let reply_code = head[1];
        // Skip the bound address the proxy echoes back.
        match head[3] {
            0x01 => {
                let mut discard = [0_u8; 4 + 2];
                stream.read_exact(&mut discard).await?;
            }
            0x03 => {
                let mut len = [0_u8; 1];
                stream.read_exact(&mut len).await?;
                let mut discard = vec![0_u8; usize::from(len[0]) + 2];
                stream.read_exact(&mut discard).await?;
            }
            0x04 => {
                let mut discard = [0_u8; 16 + 2];
                stream.read_exact(&mut discard).await?;
            }
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected SOCKS5 reply address type 0x{other:02x}"),
                ));
            }
        }
        Ok(reply_code)
    };
    let reply_code = handshake.await.map_err(|error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            SshProxyError::ProxyRefused {
                proxy: proxy.label(),
                target: target.clone(),
                detail: error.to_string(),
            }
        } else {
            SshProxyError::HandshakeFailed {
                proxy: proxy.label(),
                target: target.clone(),
                detail: error.to_string(),
            }
        }
    })?;
    if reply_code != 0x00 {
        return Err(SshProxyError::ProxyRefused {
            proxy: proxy.label(),
            target,
            detail: format!(
                "SOCKS5 reply code 0x{reply_code:02x} ({})",
                socks5_reply_description(reply_code)
            ),
        });
    }
    Ok(())
}

fn socks5_reply_description(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused by the target",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

/// Plain HTTP CONNECT (no proxy auth in v1). The status line is parsed only
/// after the full header terminator arrives; a 407 gets an explicit,
/// actionable message.
async fn http_connect<S>(
    stream: &mut S,
    proxy: &SshProxy,
    target_host: &str,
    target_port: u16,
) -> Result<(), SshProxyError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let target = format!("{target_host}:{target_port}");
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    let header = async {
        stream.write_all(request.as_bytes()).await?;
        let mut bytes = Vec::with_capacity(256);
        let mut chunk = [0_u8; 512];
        while !header_complete(&bytes) {
            if bytes.len() >= MAX_HTTP_RESPONSE_HEADER {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP CONNECT response header exceeds the read cap",
                ));
            }
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "proxy closed the connection during the HTTP CONNECT response",
                ));
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        Ok(bytes)
    };
    let header = header
        .await
        .map_err(|error: std::io::Error| SshProxyError::HandshakeFailed {
            proxy: proxy.label(),
            target: target.clone(),
            detail: error.to_string(),
        })?;
    let text = String::from_utf8_lossy(&header);
    let status_line = text.lines().next().unwrap_or_default().trim();
    let mut parts = status_line.split_whitespace();
    let _version = parts.next();
    let code: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| SshProxyError::HandshakeFailed {
            proxy: proxy.label(),
            target: target.clone(),
            detail: format!("unparseable HTTP CONNECT status line `{status_line}`"),
        })?;
    if (200..300).contains(&code) {
        return Ok(());
    }
    let detail = if code == 407 {
        format!("HTTP {code} Proxy Authentication Required — authenticated proxies are not supported (issue #201 v1)")
    } else {
        format!("HTTP {code} from the proxy")
    };
    Err(SshProxyError::ProxyRefused {
        proxy: proxy.label(),
        target,
        detail,
    })
}

fn header_complete(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_supported_schemes() {
        for (url, scheme) in [
            ("socks5://127.0.0.1:7897", SshProxyScheme::Socks5),
            ("socks5h://127.0.0.1:7897", SshProxyScheme::Socks5),
            ("http://127.0.0.1:7890", SshProxyScheme::Http),
            ("https://127.0.0.1:7890", SshProxyScheme::Http),
        ] {
            let proxy = SshProxy::parse(url).unwrap_or_else(|error| panic!("{url}: {error}"));
            assert_eq!(proxy.scheme, scheme);
            assert_eq!(proxy.host, "127.0.0.1");
            assert_eq!(
                proxy.port,
                if proxy.scheme == SshProxyScheme::Socks5 {
                    7897
                } else {
                    7890
                }
            );
        }
        let upper = SshProxy::parse("SOCKS5://proxy.example:1080").unwrap();
        assert_eq!(upper.scheme, SshProxyScheme::Socks5);
        assert_eq!(upper.host, "proxy.example");
        let trimmed = SshProxy::parse("http://proxy.example:3128/").unwrap();
        assert_eq!(trimmed.port, 3128);
    }

    #[test]
    fn parse_rejects_bad_urls_with_typed_errors() {
        for (url, needle) in [
            ("ftp://127.0.0.1:21", "unsupported proxy scheme `ftp`"),
            ("127.0.0.1:7897", "lacks a scheme"),
            ("socks5://127.0.0.1", "lacks a port"),
            ("socks5://127.0.0.1:notaport", "invalid port `notaport`"),
            ("socks5://:7897", "empty host"),
            ("socks5://proxy.example：7897", "full-width colon"),
            ("http://[::1]:3128", "IPv6"),
        ] {
            let error = SshProxy::parse(url).unwrap_err();
            assert!(
                error.to_string().contains(needle),
                "{url}: expected `{needle}` in `{error}`"
            );
        }
    }

    #[test]
    fn socks5_request_frame_uses_domainname_and_remote_resolution() {
        // The greeting is fixed; the CONNECT frame must carry ATYP=domainname.
        let proxy = SshProxy::parse("socks5://127.0.0.1:7897").unwrap();
        assert_eq!(proxy.label(), "socks5://127.0.0.1:7897");
        // Build the frame the way socks5_connect does and assert the layout.
        let domain = b"47.87.131.29";
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, domain.len() as u8]);
        frame.extend_from_slice(domain);
        frame.extend_from_slice(&7475_u16.to_be_bytes());
        assert_eq!(&frame[..5], &[0x05, 0x01, 0x00, 0x03, 12]);
        assert_eq!(&frame[5..17], domain);
        assert_eq!(&frame[17..], &[0x1d, 0x33]);
    }

    #[test]
    fn socks5_reply_codes_have_actionable_descriptions() {
        assert_eq!(
            socks5_reply_description(0x05),
            "connection refused by the target"
        );
        assert_eq!(socks5_reply_description(0x04), "host unreachable");
        assert_eq!(socks5_reply_description(0xff), "unknown error");
    }

    #[tokio::test]
    async fn socks5_handshake_over_duplex_pipe() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let proxy = SshProxy::parse("socks5://127.0.0.1:7897").unwrap();
        let client_task = tokio::spawn({
            let proxy = proxy.clone();
            async move { socks5_connect(&mut client, &proxy, "example.com", 22).await }
        });
        // Greeting.
        let mut greeting = [0_u8; 3];
        server.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [0x05, 0x01, 0x00]);
        server.write_all(&[0x05, 0x00]).await.unwrap();
        // CONNECT request.
        let mut head = [0_u8; 5];
        server.read_exact(&mut head).await.unwrap();
        assert_eq!(&head[..4], &[0x05, 0x01, 0x00, 0x03]);
        let len = usize::from(head[4]);
        let mut body = vec![0_u8; len + 2];
        server.read_exact(&mut body).await.unwrap();
        assert_eq!(&body[..len], b"example.com");
        assert_eq!(&body[len..], &22_u16.to_be_bytes());
        // Success reply: ver, rep 0, rsv, ATYP ipv4 + bound address/port.
        server
            .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        client_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn socks5_refusal_and_truncation_are_typed() {
        let proxy = SshProxy::parse("socks5://127.0.0.1:7897").unwrap();
        // Refusal: rep 0x05.
        let (mut client, mut server) = tokio::io::duplex(1024);
        let task = tokio::spawn({
            let proxy = proxy.clone();
            async move { socks5_connect(&mut client, &proxy, "example.com", 22).await }
        });
        let mut greeting = [0_u8; 3];
        server.read_exact(&mut greeting).await.unwrap();
        server.write_all(&[0x05, 0x00]).await.unwrap();
        let mut request = vec![0_u8; 4];
        server.read_exact(&mut request).await.unwrap();
        let len = usize::from({
            let mut len = [0_u8; 1];
            server.read_exact(&mut len).await.unwrap();
            len[0]
        });
        let mut body = vec![0_u8; len + 2];
        server.read_exact(&mut body).await.unwrap();
        server
            .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let error = task.await.unwrap().unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("0x05") && message.contains("connection refused"),
            "{message}"
        );
        // Truncation: the pipe closes mid-handshake.
        let (mut client, mut server) = tokio::io::duplex(64);
        let task = tokio::spawn({
            let proxy = proxy.clone();
            async move { socks5_connect(&mut client, &proxy, "example.com", 22).await }
        });
        let mut greeting = [0_u8; 3];
        server.read_exact(&mut greeting).await.unwrap();
        drop(server);
        let error = task.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("handshake"),
            "truncation must surface as a handshake error: {error}"
        );
    }

    #[test]
    fn http_response_parsing_waits_for_the_full_header() {
        assert!(header_complete(b"HTTP/1.1 200 OK\r\n\r\n"));
        assert!(header_complete(
            b"HTTP/1.1 407 x\r\nProxy-Authenticate: y\r\n\r\n"
        ));
        assert!(!header_complete(b"HTTP/1.1 200 OK\r\n"));
    }

    #[tokio::test]
    async fn http_connect_success_and_refusals_over_duplex_pipe() {
        let proxy = SshProxy::parse("http://127.0.0.1:7890").unwrap();
        // Success.
        let (mut client, mut server) = tokio::io::duplex(1024);
        let task = tokio::spawn({
            let proxy = proxy.clone();
            async move { http_connect(&mut client, &proxy, "example.com", 22).await }
        });
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            server.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        let text = String::from_utf8(request).unwrap();
        assert!(text.starts_with("CONNECT example.com:22 HTTP/1.1\r\n"));
        assert!(text.contains("Host: example.com:22\r\n"));
        server
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        task.await.unwrap().unwrap();
        // 407 names the v1 limitation.
        let (mut client, mut server) = tokio::io::duplex(1024);
        let task = tokio::spawn({
            let proxy = proxy.clone();
            async move { http_connect(&mut client, &proxy, "example.com", 22).await }
        });
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            server.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        server
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            .await
            .unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("407") && error.to_string().contains("not supported"),
            "{error}"
        );
        // Garbage status line.
        let (mut client, mut server) = tokio::io::duplex(1024);
        let task = tokio::spawn({
            let proxy = proxy.clone();
            async move { http_connect(&mut client, &proxy, "example.com", 22).await }
        });
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            server.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        server.write_all(b"garbage\r\n\r\n").await.unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("status line"), "{error}");
    }

    /// Minimal SOCKS5 no-auth mock: records the requested target and replies
    /// success. Verifies `open_proxy_stream` end to end over real TCP.
    #[tokio::test]
    async fn open_proxy_stream_succeeds_through_mock_socks5_proxy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (requested_tx, requested_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            stream.write_all(&[0x05, 0x00]).await.unwrap();
            let mut head = [0_u8; 5];
            stream.read_exact(&mut head).await.unwrap();
            let len = usize::from(head[4]);
            let mut body = vec![0_u8; len + 2];
            stream.read_exact(&mut body).await.unwrap();
            let port = u16::from_be_bytes([body[len], body[len + 1]]);
            let _ = requested_tx.send(format!("{}:{port}", String::from_utf8_lossy(&body[..len])));
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        });
        let proxy = SshProxy {
            scheme: SshProxyScheme::Socks5,
            host: "127.0.0.1".to_string(),
            port: addr.port(),
        };
        let stream = open_proxy_stream(&proxy, "47.87.131.29", 22).await.unwrap();
        assert!(stream.local_addr().is_ok());
        assert_eq!(requested_rx.await.unwrap(), "47.87.131.29:22");
    }

    /// Minimal HTTP CONNECT mock: replies 200 to any CONNECT.
    #[tokio::test]
    async fn open_proxy_stream_succeeds_through_mock_http_proxy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut byte).await.is_err() {
                    break;
                }
                request.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&request).to_string();
            let _ = seen_tx.send(text.lines().next().unwrap_or_default().to_string());
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
        });
        let proxy = SshProxy {
            scheme: SshProxyScheme::Http,
            host: "127.0.0.1".to_string(),
            port: addr.port(),
        };
        let stream = open_proxy_stream(&proxy, "example.com", 22).await.unwrap();
        assert!(stream.local_addr().is_ok());
        assert_eq!(seen_rx.await.unwrap(), "CONNECT example.com:22 HTTP/1.1");
    }
}
