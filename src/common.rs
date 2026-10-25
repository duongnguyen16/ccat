//! Provider-agnostic helpers: the error type, logging, ids and the shared HTTP agent.

use std::{io::Write, time::Duration};

/// Errors reach the GUI as text and the proxy as an HTTP status for Claude Code.
#[derive(Debug)]
pub struct Failure {
    pub status: u16,
    pub message: String,
    pub retry_after: Option<u64>,
}

pub(crate) fn fail(status: u16, message: impl Into<String>) -> Failure {
    Failure { status, message: message.into(), retry_after: None }
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("OS randomness");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn uuid() -> String {
    let h = random_hex(16);
    format!("{}-{}-4{}-a{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

pub fn agent(global: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .timeout_global(global)
        .build()
        .into()
}

/// Appends to `antigravity.log` next to the config: claude owns the terminal while the proxy runs.
pub fn log(line: &str) {
    if cfg!(test) {
        return;
    }
    let path = log_path();
    let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 2_000_000);
    let file = std::fs::OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(&path);
    if let Ok(mut f) = file {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        // Several ccat sessions can share this log; the pid tells their lines apart.
        let _ = writeln!(f, "{secs} [{}] {line}", std::process::id());
    }
}

pub fn log_path() -> std::path::PathBuf {
    crate::config_path().with_file_name("antigravity.log")
}
