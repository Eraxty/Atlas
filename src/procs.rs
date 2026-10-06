//! Starting, stopping and checking the background indexer process.

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System, UpdateKind};

use crate::paths;
use crate::sab::{detach, open_log};
use crate::ui;

pub const BG_FLAG: &str = "--bg-indexer";

/// the indexer we spawned, kept soo it gets reaped instead of lingering as a zombie
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

fn reap() {
    if let Some(child) = CHILD.lock().unwrap().as_mut() {
        let _ = child.try_wait();
    }
}

fn with_process<T>(pid: u32, f: impl FnOnce(&sysinfo::Process) -> T) -> Option<T> {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).with_exe(UpdateKind::Always),
    );
    sys.process(pid).map(f)
}

/// Is `pid` alive and actually an atlas indexer (not some recycled pid)?
pub fn is_indexer_pid(pid: i64) -> bool {
    if pid <= 0 || pid > u32::MAX as i64 {
        return false;
    }

    reap();

    with_process(pid as u32, |p| {
        if matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead) {
            return false;
        }

        let cmd = p.cmd();
        if cmd.is_empty() {
            // cant see its args (permissions), trust the pid
            return true;
        }

        cmd.iter().any(|a| {
            let a = a.to_string_lossy();
            a.contains(BG_FLAG) || a.contains("bg_indexer.py")
        })
    })
    .unwrap_or(false)
}

fn read_pid() -> Option<i64> {
    let text = fs::read_to_string(paths::pid_file()).ok()?;
    match text.trim().parse() {
        Ok(pid) => Some(pid),
        Err(_) => {
            let _ = fs::remove_file(paths::pid_file());
            None
        }
    }
}

pub fn indexer_alive() -> bool {
    let Some(pid) = read_pid() else { return false };

    if is_indexer_pid(pid) {
        return true;
    }

    let _ = fs::remove_file(paths::pid_file());
    false
}

pub fn start_background_indexer() -> bool {
    if indexer_alive() {
        ui::warn("indexer already running");
        return false;
    }

    let log_path = paths::indexer_log();
    let log = match open_log(&log_path) {
        Ok(f) => f,
        Err(e) => {
            ui::error(&format!("couldnt start indexer: {e}"));
            return false;
        }
    };

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            ui::error(&format!("couldnt start indexer: {e}"));
            return false;
        }
    };

    let mut cmd = Command::new(exe);
    cmd.arg(BG_FLAG)
        .current_dir(paths::app_dir())
        .env("ATLAS_HOME", paths::app_dir())
        .stdin(Stdio::null())
        .stdout(log.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .stderr(Stdio::from(log));
    detach(&mut cmd);

    match cmd.spawn() {
        Ok(child) => *CHILD.lock().unwrap() = Some(child),
        Err(e) => {
            ui::error(&format!("couldnt start indexer: {e}"));
            return false;
        }
    }

    for _ in 0..50 {
        if indexer_alive() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }

    ui::error(&format!("indexer didnt come up, check {}", log_path.file_name().unwrap_or_default().to_string_lossy()));
    false
}

pub fn stop_background_indexer() -> bool {
    let Some(pid) = read_pid() else { return false };

    if !is_indexer_pid(pid) {
        let _ = fs::remove_file(paths::pid_file());
        return false;
    }

    let sent = with_process(pid as u32, |p| p.kill_with(Signal::Term).unwrap_or_else(|| p.kill())).unwrap_or(false);

    if !sent {
        let _ = fs::remove_file(paths::pid_file());
        return false;
    }

    // 5 sec to comply or die
    for _ in 0..50 {
        if !is_indexer_pid(pid) {
            let _ = fs::remove_file(paths::pid_file());
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }

    // didnt exit in time soo kill him
    with_process(pid as u32, |p| p.kill());
    reap();

    let _ = fs::remove_file(paths::pid_file());
    true
}

/// status.json written by the indexer, plus `stale` when its pid is gone.
pub fn get_status() -> Value {
    let status = fs::read_to_string(paths::status_file()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());

    let Some(Value::Object(mut status)) = status else {
        return serde_json::json!({"running": false, "group": ""});
    };

    let pid = status.get("pid").and_then(Value::as_i64).unwrap_or(0);
    status.insert("stale".into(), Value::Bool(!is_indexer_pid(pid)));

    Value::Object(status)
}
