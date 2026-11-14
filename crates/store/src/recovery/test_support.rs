//! Shared fixture-only setup. Never falls back from native disk proof.
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
pub fn fixture_root() -> tempfile::TempDir {
    let parent = std::env::var_os("FL_RECOVERY_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let root =
        tempfile::tempdir_in(parent).expect("native disk-backed FL_RECOVERY_TEST_DIR required");
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    #[cfg(target_os = "linux")]
    {
        let path = std::ffi::CString::new(root.path().as_os_str().as_encoded_bytes()).unwrap();
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        assert_eq!(unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) }, 0);
        assert!(
            matches!(
                unsafe { stat.assume_init() }.f_type as u64,
                0xef53 | 0x58465342
            ),
            "native ext4/XFS recovery fixture required; unsupported does not count as refusal proof"
        );
    }
    root
}
