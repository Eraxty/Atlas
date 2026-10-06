use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const ATTEMPTS: u32 = 5;

/// Rename `tmp` over `target`, retrying a few times because windows
/// refuses to replace a file another process has open.
pub fn atomic_replace(tmp: &Path, target: &Path) -> bool {
    for attempt in 0..ATTEMPTS {
        match fs::rename(tmp, target) {
            Ok(()) => return true,
            Err(_) if attempt + 1 < ATTEMPTS => {
                thread::sleep(Duration::from_millis(50 * u64::from(attempt + 1)));
            }
            Err(_) => return false,
        }
    }

    false
}

/// `foo.json` -> `foo.json.tmp`
pub fn tmp_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    target.with_file_name(name)
}

/// Write via a temp file soo readers never see half a file.
pub fn write_atomic(target: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let tmp = tmp_path(target);
    fs::write(&tmp, contents)?;

    if atomic_replace(&tmp, target) {
        Ok(())
    } else {
        let _ = fs::remove_file(&tmp);
        Err(io::Error::other(format!("could not write {}", target.display())))
    }
}
