//! Newline-delimited messages between the daemon and one session supervisor.
//!
//! The daemon writes `spawn` and then `go`. The supervisor writes identity before
//! execution is released, then exec, leader, descendant, and cleanup. Control EOF is
//! not a message and is not proof the subtree has exited.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonMsg {
    Spawn {
        argv: Vec<String>,
        cwd: Vec<u8>,
        env: Vec<(String, String)>,
        session: String,
        epoch: u64,
    },
    Go,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SupervisorMsg {
    Fail {
        error: String,
    },
    Identity {
        vendor_pid: u32,
        vendor_start: u64,
        pgid: u32,
        supervisor_pid: u32,
        supervisor_start: u64,
    },
    Exec {
        ok: bool,
        error: String,
    },
    Leader {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Descendant {
        pid: u32,
        code: Option<i32>,
        signal: Option<i32>,
        adopted: bool,
    },
    Cleanup {
        complete: bool,
    },
}

pub fn to_line(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err("supervisor message is too large".into());
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn from_line<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Result<T, String> {
    serde_json::from_slice(line).map_err(|err| err.to_string())
}
