#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_history_store::RemoteHostAuthProfile;
use crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretValue;
use hmac::{Hmac, KeyInit, Mac};
use russh::client;
use russh::keys::ssh_key::{
    self,
    known_hosts::{HostPatterns, KnownHosts, Marker},
};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};
use sha1::Sha1;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SSH_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Clone)]
pub struct RemoteSshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: RemoteSshAuth,
}

impl RemoteSshTarget {
    pub fn from_profile(
        host: impl Into<String>,
        user: impl Into<String>,
        auth: &RemoteHostAuthProfile,
        ssh_password: Option<RemoteHostSecretValue>,
    ) -> Result<Self, RemoteSshError> {
        let auth = match auth {
            RemoteHostAuthProfile::Password { .. } => {
                let password = ssh_password.ok_or_else(|| {
                    RemoteSshError::new("password auth requires a loaded SSH password")
                })?;
                RemoteSshAuth::Password {
                    password: password.expose_secret().to_string(),
                }
            }
            RemoteHostAuthProfile::Key { key_path } => RemoteSshAuth::Key {
                key_path: key_path.clone(),
            },
        };
        Ok(Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth,
        })
    }

    fn socket_addrs(&self) -> Result<Vec<SocketAddr>, RemoteSshError> {
        (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|error| RemoteSshError::new(format!("failed to resolve SSH host: {error}")))
            .map(|addrs| addrs.collect())
    }
}

#[derive(Debug, Clone)]
pub enum RemoteSshAuth {
    Password { password: String },
    Key { key_path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSshOutput {
    pub status: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSshError {
    message: String,
}

impl RemoteSshError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteSshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteSshError {}

pub trait RemoteSshExecutor {
    type Error;

    fn exec(
        &self,
        target: &RemoteSshTarget,
        command: &str,
        stdin: Option<&str>,
    ) -> Result<RemoteSshOutput, Self::Error>;
}

#[derive(Debug, Clone, Default)]
pub struct RusshRemoteSshExecutor;

impl RemoteSshExecutor for RusshRemoteSshExecutor {
    type Error = RemoteSshError;

    fn exec(
        &self,
        target: &RemoteSshTarget,
        command: &str,
        stdin: Option<&str>,
    ) -> Result<RemoteSshOutput, Self::Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|error| {
                RemoteSshError::new(format!("failed to start SSH runtime: {error}"))
            })?;
        runtime.block_on(exec_async(
            target.clone(),
            command.to_string(),
            stdin.map(str::to_string),
        ))
    }
}

#[derive(Debug, Clone)]
struct Client {
    host: String,
    port: u16,
    known_hosts_path: PathBuf,
}

impl Client {
    fn from_target(target: &RemoteSshTarget) -> Result<Self, RemoteSshError> {
        Ok(Self {
            host: target.host.clone(),
            port: target.port,
            known_hosts_path: default_known_hosts_path()?,
        })
    }
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        verify_or_accept_known_host(
            &self.known_hosts_path,
            &self.host,
            self.port,
            server_public_key,
        )?;
        Ok(true)
    }
}

async fn exec_async(
    target: RemoteSshTarget,
    command: String,
    stdin: Option<String>,
) -> Result<RemoteSshOutput, RemoteSshError> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(SSH_INACTIVITY_TIMEOUT),
        ..Default::default()
    });
    let addrs = target.socket_addrs()?;
    let client = Client::from_target(&target)?;
    let connect = client::connect(config, addrs.as_slice(), client);
    let mut session = tokio::time::timeout(SSH_CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| RemoteSshError::new("SSH connect timed out"))?
        .map_err(|error| RemoteSshError::new(format!("SSH connect failed: {error}")))?;
    authenticate(&mut session, &target)
        .await
        .map_err(|error| RemoteSshError::new(format!("SSH authentication failed: {error}")))?;

    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|error| RemoteSshError::new(format!("SSH open session failed: {error}")))?;
    channel
        .exec(true, command)
        .await
        .map_err(|error| RemoteSshError::new(format!("SSH exec failed: {error}")))?;
    if let Some(stdin) = stdin {
        channel
            .data_bytes(stdin.into_bytes())
            .await
            .map_err(|error| RemoteSshError::new(format!("SSH stdin write failed: {error}")))?;
        channel
            .eof()
            .await
            .map_err(|error| RemoteSshError::new(format!("SSH stdin close failed: {error}")))?;
    }

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    let _ = session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;
    Ok(RemoteSshOutput {
        status: status.unwrap_or(255),
        stdout,
        stderr,
    })
}

async fn authenticate(
    session: &mut client::Handle<Client>,
    target: &RemoteSshTarget,
) -> Result<(), russh::Error> {
    let auth = match &target.auth {
        RemoteSshAuth::Password { password } => {
            session
                .authenticate_password(target.user.clone(), password.clone())
                .await?
        }
        RemoteSshAuth::Key { key_path } => {
            let key_pair = load_secret_key(key_path, None)?;
            let rsa_hash = session.best_supported_rsa_hash().await?.flatten();
            session
                .authenticate_publickey(
                    target.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key_pair), rsa_hash),
                )
                .await?
        }
    };
    if auth.success() {
        Ok(())
    } else {
        Err(russh::Error::NotAuthenticated)
    }
}

fn default_known_hosts_path() -> Result<PathBuf, RemoteSshError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| RemoteSshError::new("HOME is not set; cannot locate OpenSSH known_hosts"))?;
    Ok(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

fn verify_or_accept_known_host(
    path: &Path,
    host: &str,
    port: u16,
    server_public_key: &ssh_key::PublicKey,
) -> Result<(), russh::Error> {
    let known_hosts = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    match known_host_status(&known_hosts, host, port, server_public_key)? {
        KnownHostStatus::Matched => Ok(()),
        KnownHostStatus::New => append_known_host(path, host, port, server_public_key),
        KnownHostStatus::Changed { line } => Err(russh::Error::KeyChanged { line }),
        KnownHostStatus::Revoked { line } => Err(russh::Error::KeyChanged { line }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnownHostStatus {
    Matched,
    New,
    Changed { line: usize },
    Revoked { line: usize },
}

fn known_host_status(
    input: &str,
    host: &str,
    port: u16,
    server_public_key: &ssh_key::PublicKey,
) -> Result<KnownHostStatus, russh::Error> {
    let host_patterns = known_host_candidates(host, port);
    for (index, entry) in KnownHosts::new(input).enumerate() {
        let line = index + 1;
        let entry = entry?;
        if !entry_matches_host(entry.host_patterns(), &host_patterns) {
            continue;
        }
        if entry.marker() == Some(&Marker::Revoked) {
            return Ok(KnownHostStatus::Revoked { line });
        }
        if entry.public_key().algorithm() != server_public_key.algorithm() {
            continue;
        }
        if entry.public_key() == server_public_key {
            return Ok(KnownHostStatus::Matched);
        }
        return Ok(KnownHostStatus::Changed { line });
    }
    Ok(KnownHostStatus::New)
}

fn append_known_host(
    path: &Path,
    host: &str,
    port: u16,
    server_public_key: &ssh_key::PublicKey,
) -> Result<(), russh::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let host_pattern = known_host_write_pattern(host, port);
    let public_key = public_key_without_comment(server_public_key)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{host_pattern} {public_key}")?;
    Ok(())
}

fn public_key_without_comment(public_key: &ssh_key::PublicKey) -> Result<String, ssh_key::Error> {
    let mut public_key = public_key.clone();
    public_key.set_comment("");
    public_key.to_openssh()
}

fn known_host_write_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

fn known_host_candidates(host: &str, port: u16) -> Vec<String> {
    if port == 22 {
        vec![host.to_string()]
    } else {
        vec![format!("[{host}]:{port}"), host.to_string()]
    }
}

fn entry_matches_host(patterns: &HostPatterns, candidates: &[String]) -> bool {
    match patterns {
        HostPatterns::Patterns(patterns) => candidates.iter().any(|candidate| {
            let mut matched = false;
            for pattern in patterns {
                if let Some(negated) = pattern.strip_prefix('!') {
                    if host_pattern_matches(negated, candidate) {
                        return false;
                    }
                } else if host_pattern_matches(pattern, candidate) {
                    matched = true;
                }
            }
            matched
        }),
        HostPatterns::HashedName { salt, hash } => candidates
            .iter()
            .any(|candidate| hashed_host_matches(salt, hash, candidate)),
    }
}

fn host_pattern_matches(pattern: &str, value: &str) -> bool {
    glob_matches(
        pattern.to_ascii_lowercase().as_bytes(),
        value.to_ascii_lowercase().as_bytes(),
    )
}

fn glob_matches(pattern: &[u8], value: &[u8]) -> bool {
    let (mut p, mut v) = (0, 0);
    let mut star = None;
    let mut star_value = 0;
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            star_value = v;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            star_value += 1;
            v = star_value;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn hashed_host_matches(salt: &[u8], expected_hash: &[u8; 20], candidate: &str) -> bool {
    let Ok(mut mac) = HmacSha1::new_from_slice(salt) else {
        return false;
    };
    mac.update(candidate.as_bytes());
    let digest = mac.finalize().into_bytes();
    digest.as_slice() == expected_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB9dG4kjRhQTtWTVzd2t27+t0DEHBPW7iOD23TUiYLio";
    const KEY_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAACAQC0WRHtxuxefSJhpIxGq4ibGFgwYnESPm8C3JFM88A1JJLoprenklrd7VJ+VH3Ov/bQwZwLyRU5dRmfR/SWTtIPWs7tToJVayKKDB+/qoXmM5ui/0CU2U4rCdQ6PdaCJdC7yFgpPL8WexjWN06+eSIKYz1AAXbx9rRv1iasslK/KUqtsqzVliagI6jl7FPO2GhRZMcso6LsZGgSxuYf/Lp0D/FcBU8GkeOo1Sx5xEt8H8bJcErtCe4Blb8JxcW6EXO3sReb4z+zcR07gumPgFITZ6hDA8sSNuvo/AlWg0IKTeZSwHHVknWdQqDJ0uczE837caBxyTZllDNIGkBjCIIOFzuTT76HfYc/7CTTGk07uaNkUFXKN79xDiFOX8JQ1ZZMZvGOTwWjuT9CqgdTvQRORbRWwOYv3MH8re9ykw3Ip6lrPifY7s6hOaAKry/nkGPMt40m1TdiW98MTIpooE7W+WXu96ax2l2OJvxX8QR7l+LFlKnkIEEJd/ItF1G22UmOjkVwNASTwza/hlY+8DoVvEmwum/nMgH2TwQT3bTQzF9s9DOJkH4d8p4Mw4gEDjNx0EgUFA91ysCAeUMQQyIvuR8HXXa+VcvhOOO5mmBcVhxJ3qUOJTyDBsT0932Zb4mNtkxdigoVxu+iiwk0vwtvKwGVDYdyMP5EAQeEIP1t0w==";

    fn key(value: &str) -> ssh_key::PublicKey {
        value.parse().unwrap()
    }

    #[test]
    fn known_hosts_accepts_first_key_as_new() {
        assert_eq!(
            known_host_status("", "example.com", 22, &key(KEY_A)).unwrap(),
            KnownHostStatus::New
        );
    }

    #[test]
    fn known_hosts_accepts_existing_same_key() {
        let input = format!("example.com {KEY_A}\n");
        assert_eq!(
            known_host_status(&input, "example.com", 22, &key(KEY_A)).unwrap(),
            KnownHostStatus::Matched
        );
    }

    #[test]
    fn known_hosts_rejects_changed_same_algorithm_key() {
        let input = format!("example.com {KEY_A}\n");
        assert_eq!(
            known_host_status(&input, "example.com", 22, &key(KEY_B)).unwrap(),
            KnownHostStatus::Changed { line: 1 }
        );
    }

    #[test]
    fn known_hosts_allows_learning_different_key_algorithm() {
        let input = format!("example.com {KEY_RSA}\n");
        assert_eq!(
            known_host_status(&input, "example.com", 22, &key(KEY_A)).unwrap(),
            KnownHostStatus::New
        );
    }

    #[test]
    fn known_hosts_matches_non_default_port_open_ssh_pattern() {
        let input = format!("[example.com]:2200 {KEY_A}\n");
        assert_eq!(
            known_host_status(&input, "example.com", 2200, &key(KEY_A)).unwrap(),
            KnownHostStatus::Matched
        );
    }

    #[test]
    fn known_hosts_matches_glob_and_negation_patterns() {
        let input = format!("*.example.com,!bad.example.com {KEY_A}\n");
        assert_eq!(
            known_host_status(&input, "good.example.com", 22, &key(KEY_A)).unwrap(),
            KnownHostStatus::Matched
        );
        assert_eq!(
            known_host_status(&input, "bad.example.com", 22, &key(KEY_A)).unwrap(),
            KnownHostStatus::New
        );
    }

    #[test]
    fn known_hosts_matches_hashed_host() {
        let salt = [1_u8; 20];
        let mut mac = HmacSha1::new_from_slice(&salt).unwrap();
        mac.update(b"example.com");
        let digest = mac.finalize().into_bytes();
        let salt = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, salt);
        let hash = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, digest);
        let input = format!("|1|{salt}|{hash} {KEY_A}\n");

        assert_eq!(
            known_host_status(&input, "example.com", 22, &key(KEY_A)).unwrap(),
            KnownHostStatus::Matched
        );
    }

    #[test]
    fn known_hosts_rejects_revoked_host() {
        let input = format!("@revoked example.com {KEY_A}\n");
        assert_eq!(
            known_host_status(&input, "example.com", 22, &key(KEY_A)).unwrap(),
            KnownHostStatus::Revoked { line: 1 }
        );
    }

    #[test]
    fn known_host_write_pattern_uses_open_ssh_port_format() {
        assert_eq!(known_host_write_pattern("example.com", 22), "example.com");
        assert_eq!(
            known_host_write_pattern("example.com", 2200),
            "[example.com]:2200"
        );
    }
}
