//! Mount-aware path overlap checks for the Linux offline process profile.
//!
//! Canonical path prefixes do not detect bind mounts. `/proc/self/mountinfo`
//! records the mounted filesystem root and mountpoint, which lets policy compare
//! the backing paths for roots and for mounts nested beneath either root.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
struct MountEntry {
    device: (u32, u32),
    root: PathBuf,
    mountpoint: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BackingPath {
    device: (u32, u32),
    path: PathBuf,
}

pub(super) fn paths_overlap_with_mounts(left: &Path, right: &Path) -> Result<bool, &'static str> {
    if !left.is_absolute() || !right.is_absolute() {
        return Err("offline process paths must be absolute");
    }

    let contents = std::fs::read("/proc/self/mountinfo")
        .map_err(|_| "Linux mount information is unavailable")?;
    paths_overlap_in_mountinfo(&contents, left, right)
}

fn paths_overlap_in_mountinfo(
    contents: &[u8],
    left: &Path,
    right: &Path,
) -> Result<bool, &'static str> {
    if left == right || left.starts_with(right) || right.starts_with(left) {
        return Ok(true);
    }
    let mounts = parse_mountinfo(contents)?;
    let left_candidates = root_and_nested_mountpoints(left, &mounts);
    let right_candidates = root_and_nested_mountpoints(right, &mounts);

    for left_candidate in &left_candidates {
        let left_backing = backing_path(left_candidate, &mounts)?;
        for right_candidate in &right_candidates {
            let right_backing = backing_path(right_candidate, &mounts)?;
            if backing_paths_overlap(&left_backing, &right_backing) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn root_and_nested_mountpoints(root: &Path, mounts: &[MountEntry]) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    paths.extend(
        mounts
            .iter()
            .filter(|mount| mount.mountpoint.starts_with(root))
            .map(|mount| mount.mountpoint.clone()),
    );
    paths.sort();
    paths.dedup();
    paths
}

fn backing_path(path: &Path, mounts: &[MountEntry]) -> Result<BackingPath, &'static str> {
    let mut most_specific = None::<(&MountEntry, usize)>;
    for mount in mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mountpoint))
    {
        let depth = mount.mountpoint.components().count();
        match most_specific {
            None => most_specific = Some((mount, depth)),
            Some((_current, current_depth)) if depth > current_depth => {
                most_specific = Some((mount, depth));
            }
            Some((current, current_depth)) if depth == current_depth => {
                if current.device != mount.device || current.root != mount.root {
                    return Err("Linux mount information has ambiguous mountpoints");
                }
            }
            Some(_) => {}
        }
    }
    let (mount, _) =
        most_specific.ok_or("Linux mount information does not cover an offline path")?;
    let suffix = path
        .strip_prefix(&mount.mountpoint)
        .map_err(|_| "Linux mount path could not be resolved")?;
    let mut backing = mount.root.clone();
    backing.push(suffix);
    if !backing.is_absolute()
        || backing
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("Linux mount information contains an invalid backing path");
    }
    Ok(BackingPath {
        device: mount.device,
        path: backing,
    })
}

fn backing_paths_overlap(left: &BackingPath, right: &BackingPath) -> bool {
    left.device == right.device
        && (left.path == right.path
            || left.path.starts_with(&right.path)
            || right.path.starts_with(&left.path))
}

fn parse_mountinfo(contents: &[u8]) -> Result<Vec<MountEntry>, &'static str> {
    let mut mounts = Vec::new();
    for line in contents
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields = line
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or("Linux mount information is malformed")?;
        if separator < 6 || fields.len() <= separator + 2 {
            return Err("Linux mount information is malformed");
        }
        let (major, minor) = parse_device(fields[2])?;
        let root = decode_mount_path(fields[3])?;
        let mountpoint = decode_mount_path(fields[4])?;
        mounts.push(MountEntry {
            device: (major, minor),
            root,
            mountpoint,
        });
    }
    if mounts.is_empty() {
        return Err("Linux mount information is empty");
    }
    Ok(mounts)
}

fn parse_device(value: &[u8]) -> Result<(u32, u32), &'static str> {
    let colon = value
        .iter()
        .position(|byte| *byte == b':')
        .ok_or("Linux mount device is malformed")?;
    let major = std::str::from_utf8(&value[..colon])
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or("Linux mount device is malformed")?;
    let minor = std::str::from_utf8(&value[colon + 1..])
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or("Linux mount device is malformed")?;
    Ok((major, minor))
}

fn decode_mount_path(value: &[u8]) -> Result<PathBuf, &'static str> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'\\' {
            decoded.push(value[index]);
            index += 1;
            continue;
        }
        let octal = value
            .get(index + 1..index + 4)
            .ok_or("Linux mount path escape is malformed")?;
        if !octal.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
            return Err("Linux mount path escape is malformed");
        }
        let value = u16::from(octal[0] - b'0') * 64
            + u16::from(octal[1] - b'0') * 8
            + u16::from(octal[2] - b'0');
        if value == 0 {
            return Err("Linux mount path contains a NUL byte");
        }
        let byte = u8::try_from(value).map_err(|_| "Linux mount path escape is out of range")?;
        decoded.push(byte);
        index += 4;
    }
    let path = PathBuf::from(OsString::from_vec(decoded));
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("Linux mount information contains an invalid path");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{parse_mountinfo, paths_overlap_with_mounts};
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const ROOT_MOUNT: &str = "1 0 8:1 / / rw - ext4 /dev/root rw\n";

    #[test]
    fn mountinfo_parser_decodes_escaped_mount_paths() {
        let input = b"1 0 8:1 /folder\\040with\\040spaces /mnt\\040with\\040spaces rw - ext4 /dev/root rw\n";
        let mounts = parse_mountinfo(input).unwrap();
        assert_eq!(mounts[0].root, Path::new("/folder with spaces"));
        assert_eq!(mounts[0].mountpoint, Path::new("/mnt with spaces"));
    }

    #[test]
    fn nested_bind_mount_alias_to_private_settings_overlaps_workspace() {
        let input = format!(
            "{ROOT_MOUNT}2 1 8:1 /home/alice/.config/krypper /work/private rw - ext4 /dev/root rw\n"
        );
        assert!(paths_overlap_with_mounts_from(
            &input,
            "/work",
            "/home/alice/.config/krypper/offline-process.json"
        )
        .unwrap());
    }

    #[test]
    fn bind_mounted_workspace_root_aliases_its_source_tree() {
        let input = format!(
            "{ROOT_MOUNT}2 1 8:1 /home/alice/.config/krypper /work/project rw - ext4 /dev/root rw\n"
        );
        assert!(paths_overlap_with_mounts_from(
            &input,
            "/work/project",
            "/home/alice/.config/krypper/offline-process.json"
        )
        .unwrap());
    }

    #[test]
    fn disjoint_sibling_roots_do_not_overlap() {
        assert!(
            !paths_overlap_with_mounts_from(ROOT_MOUNT, "/opt/toolchain", "/opt/registry").unwrap()
        );
    }

    #[test]
    fn nested_bind_mount_alias_between_roots_is_rejected() {
        let input = format!(
            "{ROOT_MOUNT}2 1 8:1 /opt/toolchain /opt/registry/toolchain-view rw - ext4 /dev/root rw\n"
        );
        assert!(paths_overlap_with_mounts_from(&input, "/opt/toolchain", "/opt/registry").unwrap());
    }

    #[test]
    fn malformed_mountinfo_fails_closed() {
        assert!(parse_mountinfo(b"not mount info\n").is_err());
        assert!(parse_mountinfo(b"1 0 8:1 /bad\\777 /mnt rw - ext4 /dev/root rw\n").is_err());
        assert!(paths_overlap_with_mounts_from("", "/work", "/private").is_err());
    }

    fn paths_overlap_with_mounts_from(
        contents: &str,
        left: &str,
        right: &str,
    ) -> Result<bool, &'static str> {
        super::paths_overlap_in_mountinfo(contents.as_bytes(), Path::new(left), Path::new(right))
    }

    #[test]
    fn real_bind_mount_alias_is_rejected_when_mount_namespace_is_available() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let settings = temporary.path().join("settings");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&settings).unwrap();
        let alias = workspace.join("settings-alias");

        // SAFETY: unshare takes only a supported flag bitmask and no pointers.
        if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
            eprintln!("skipping live bind-mount check: mount namespace is unavailable");
            return;
        }
        let slash = CString::new("/").unwrap();
        // SAFETY: the mount target is a valid NUL-terminated root path; null
        // source, filesystem type, and data are valid for a propagation change.
        if unsafe {
            libc::mount(
                std::ptr::null(),
                slash.as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } != 0
        {
            eprintln!("skipping live bind-mount check: private propagation is unavailable");
            return;
        }
        let source = CString::new(settings.as_os_str().as_bytes()).unwrap();
        fs::create_dir_all(&alias).unwrap();
        let target = CString::new(alias.as_os_str().as_bytes()).unwrap();
        // SAFETY: source and target are live NUL-terminated paths, and null
        // filesystem type and data are valid for an MS_BIND mount.
        if unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } != 0
        {
            eprintln!("skipping live bind-mount check: bind mounts are unavailable");
            return;
        }
        let _mount_cleanup = MountCleanup(alias);
        assert!(paths_overlap_with_mounts(&workspace, &settings).unwrap());
    }

    struct MountCleanup(std::path::PathBuf);

    impl Drop for MountCleanup {
        fn drop(&mut self) {
            if let Ok(path) = CString::new(self.0.as_os_str().as_bytes()) {
                // SAFETY: path is a live NUL-terminated mountpoint path.
                let _ = unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
            }
        }
    }
}
