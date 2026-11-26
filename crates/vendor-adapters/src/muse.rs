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
        if name != "muse" || !is_native_launcher(executable) {
            return None;
        }
        // The launcher reads this exact sibling file and executes the named sibling binary.
        // Other installed binaries cannot establish which one the launcher selects.
        let directory = executable.parent()?;
        let selection = read_bounded_regular_file(&directory.join(".muse-version"), 160)?;
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

fn is_native_launcher(path: &Path) -> bool {
    const HEADER: &str = r#"#!/usr/bin/env bash
set -euo pipefail

channel="${MUSE_CHANNEL:-muse-stable}"
channel_url="https://api.meta.ai/muse-code/channels/$channel"
"#;
    const INSTALL_DIR: &str = r#"resolve_install_dir() {
  parent_directory "$(resolve_self)"
}"#;
    const SELECTOR: &str = r#"active_binary() {
  local dir="$1"
  local version
  version=""
  [[ ! -r "$dir/.muse-version" ]] || version="$(<"$dir/.muse-version")"
  [[ "$version" =~ $version_pattern ]] || return 1
  printf '%s/muse-bin-%s\n' "$dir" "$version"
}"#;
    const ENTRYPOINT: &str = "  exec \"$binary\" \"$@\"\n}\n\nmain \"$@\"\n";

    // The vendor wrapper resolves its own directory, reads the marker there and
    // execs the selected sibling. A custom executable named muse may do anything.
    read_bounded_regular_file(path, 64 * 1024).is_some_and(|script| {
        script.starts_with(HEADER)
            && script.contains(INSTALL_DIR)
            && script.contains(SELECTOR)
            && script.contains(r#"  dir="$(resolve_install_dir)""#)
            && script.contains(r#"  binary="$(active_binary "$dir" 2>/dev/null || true)""#)
            && script.ends_with(ENTRYPOINT)
    })
}

fn read_bounded_regular_file(path: &Path, limit: u64) -> Option<String> {
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.is_file() || metadata.len() > limit {
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
    file.take(limit + 1).read_to_string(&mut contents).ok()?;
    (contents.len() as u64 <= limit).then_some(contents)
}
