use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::Path;

use fl_core::VendorVersionAdapter;

#[derive(Debug)]
pub(crate) struct MuseVersionAdapter;

impl VendorVersionAdapter for MuseVersionAdapter {
    fn vendor_id(&self) -> &'static str {
        "muse"
    }

    fn version_from_executable(&self, executable: &Path) -> Option<String> {
        if !executable.is_absolute() {
            return None;
        }
        let name = executable.file_name()?.to_str()?;
        if let Some(version) = name.strip_prefix("muse-bin-") {
            return runtime_version(version).map(str::to_owned);
        }
        if name != "muse" {
            return None;
        }
        // The launcher reads this exact sibling file and executes the named sibling binary.
        // Other installed binaries cannot establish which one the launcher selects.
        let directory = executable.parent()?;
        let selection = read_selection(&directory.join(".muse-version"))?;
        let version = runtime_version(selection.trim_end_matches('\n'))?;
        let runtime = fs::symlink_metadata(directory.join(format!("muse-bin-{version}"))).ok()?;
        if !runtime.is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if runtime.permissions().mode() & 0o111 == 0 {
                return None;
            }
        }
        Some(version.to_owned())
    }
}

fn runtime_version(version: &str) -> Option<&str> {
    crate::valid_release_version(version)?;
    let (_, release) = version.split_once("-R")?;
    let (number, revision) = release.split_once('.').unwrap_or((release, "0"));
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    (digits(number) && digits(revision)).then_some(version)
}

fn read_selection(path: &Path) -> Option<String> {
    const LIMIT: u64 = 160;
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Keep a replacement symlink or FIFO from redirecting or blocking the read.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut contents = String::new();
    file.take(LIMIT + 1).read_to_string(&mut contents).ok()?;
    (contents.len() as u64 <= LIMIT).then_some(contents)
}
