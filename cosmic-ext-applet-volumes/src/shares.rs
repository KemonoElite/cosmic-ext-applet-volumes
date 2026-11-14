// SPDX-License-Identifier: GPL-3.0-only

//! Network shares (Samba/SMB and NFS): addresses, the saved-shares file, and telling network
//! mounts apart from local ones. Pure logic, no GIO, so it can be tested on its own.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// URI schemes GVfs serves for the protocols this applet supports.
const SCHEMES: &[&str] = &["smb", "nfs"];

/// Filesystem types of kernel mounts that are network shares.
const NETWORK_FS: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "smbfs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressError {
    /// Not `smb://host/...` or `nfs://host/...`.
    Invalid,
    /// `smb://user:secret@host/...`: it would be written to disk in plain text.
    PasswordInAddress,
}

#[must_use]
pub fn is_share_scheme(scheme: &str) -> bool {
    SCHEMES.iter().any(|s| s.eq_ignore_ascii_case(scheme))
}

/// True for anything that looks like `smb://...` or `nfs://...`.
#[must_use]
pub fn is_share_uri(text: &str) -> bool {
    text.split_once("://")
        .is_some_and(|(scheme, _)| is_share_scheme(scheme))
}

/// The parts of an address: scheme (lower case), user info, host (lower case) with port, path
/// (no trailing slash).
fn split(address: &str) -> Result<(String, Option<&str>, String, &str), AddressError> {
    let (scheme, rest) = address.split_once("://").ok_or(AddressError::Invalid)?;
    if !is_share_scheme(scheme) {
        return Err(AddressError::Invalid);
    }
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let (userinfo, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    if host.is_empty() || host.starts_with(':') {
        return Err(AddressError::Invalid);
    }
    Ok((
        scheme.to_ascii_lowercase(),
        userinfo,
        host.to_ascii_lowercase(),
        path.trim_end_matches('/'),
    ))
}

/// Clean up what the user typed into "Connect to server".
///
/// # Errors
/// [`AddressError::Invalid`] when it is not an SMB or NFS address, and
/// [`AddressError::PasswordInAddress`] when it carries a password.
pub fn normalize(input: &str) -> Result<String, AddressError> {
    let input = input.trim().replace(' ', "%20");
    let (scheme, userinfo, host, path) = split(&input)?;
    if userinfo.is_some_and(|u| u.contains(':')) {
        return Err(AddressError::PasswordInAddress);
    }
    let user = userinfo.map(|u| format!("{u}@")).unwrap_or_default();
    Ok(format!("{scheme}://{user}{host}{path}"))
}

/// What identifies a share regardless of how it was spelled: GVfs may add or drop the user name
/// and a trailing slash in the URI it reports for a mount.
#[must_use]
pub fn key(uri: &str) -> String {
    split(uri).map_or_else(
        |_| uri.trim_end_matches('/').to_string(),
        |(scheme, _, host, path)| format!("{scheme}://{host}{path}"),
    )
}

/// `smb://nas/Family Photos` becomes "Family Photos on nas".
#[must_use]
pub fn display_name(uri: &str) -> String {
    let Ok((_, _, host, path)) = split(uri) else {
        return uri.to_string();
    };
    match path.rsplit_once('/') {
        Some((_, last)) if !last.is_empty() => format!("{} on {host}", last.replace("%20", " ")),
        _ => host,
    }
}

/// Is this the device column of an `/etc/fstab` entry for a network filesystem?
/// (`//server/share` for CIFS, `server:/export` for NFS.)
#[must_use]
pub fn is_network_device(spec: &str) -> bool {
    spec.starts_with("//") || (!spec.starts_with('/') && spec.contains(":/"))
}

#[must_use]
pub fn is_network_fs(fs_type: &str) -> bool {
    NETWORK_FS.contains(&fs_type)
}

/// `\040`-style escapes used by `/proc/self/mountinfo`.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|_| bytes[i] == b'\\')
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u8::from_str_radix(d, 8).ok());
        if let Some(byte) = octal {
            out.push(byte);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(mount point, filesystem type)` for every line of a mountinfo file.
#[must_use]
pub fn parse_mountinfo(text: &str) -> Vec<(PathBuf, String)> {
    text.lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mount_point = before.split_whitespace().nth(4)?;
            let fs_type = after.split_whitespace().next()?;
            Some((PathBuf::from(unescape(mount_point)), fs_type.to_string()))
        })
        .collect()
}

/// Where the kernel currently has network filesystems mounted.
#[must_use]
pub fn network_mount_points() -> Vec<PathBuf> {
    fs::read_to_string("/proc/self/mountinfo")
        .map(|text| network_mount_points_in(&text))
        .unwrap_or_default()
}

fn network_mount_points_in(mountinfo: &str) -> Vec<PathBuf> {
    parse_mountinfo(mountinfo)
        .into_iter()
        .filter(|(_, fs_type)| is_network_fs(fs_type))
        .map(|(mount_point, _)| mount_point)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The saved shares file: one address per line, `#` for comments.
// ---------------------------------------------------------------------------------------------

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("cosmic-ext-applet-volumes").join("shares"))
}

/// The shares the user added, or none when there is no file yet.
#[must_use]
pub fn load() -> Vec<String> {
    config_path().map(|p| load_from(&p)).unwrap_or_default()
}

/// # Errors
/// Any I/O error while writing the file.
pub fn save(shares: &[String]) -> io::Result<()> {
    let path = config_path().ok_or_else(|| io::Error::other("no config directory"))?;
    save_to(&path, shares)
}

fn load_from(path: &Path) -> Vec<String> {
    let mut shares: Vec<String> = Vec::new();
    for line in fs::read_to_string(path).unwrap_or_default().lines() {
        let line = line.trim();
        // Anything that no longer parses (or sneaked a password in) is dropped, not trusted.
        if let Ok(share) = normalize(line)
            && !line.starts_with('#')
            && !shares.contains(&share)
        {
            shares.push(share);
        }
    }
    shares
}

fn save_to(path: &Path, shares: &[String]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut text = String::from("# Network shares shown by the Volumes applet, one per line.\n");
    for share in shares {
        text.push_str(share);
        text.push('\n');
    }
    // Write next to the target and rename, so a crash never leaves a half-written list.
    let temp = path.with_extension("tmp");
    fs::write(&temp, text)?;
    fs::rename(temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_what_people_type() {
        for (input, expected) in [
            ("smb://nas/media", "smb://nas/media"),
            ("  SMB://NAS.local/Media/  ", "smb://nas.local/Media"),
            ("nfs://10.0.0.5/export/data", "nfs://10.0.0.5/export/data"),
            ("smb://nas", "smb://nas"),
            ("smb://nas/", "smb://nas"),
            ("smb://alice@nas/media", "smb://alice@nas/media"),
            ("smb://nas:4455/media", "smb://nas:4455/media"),
            ("smb://nas/Family Photos", "smb://nas/Family%20Photos"),
        ] {
            assert_eq!(normalize(input).as_deref(), Ok(expected), "{input:?}");
        }
    }

    #[test]
    fn rejects_other_protocols_and_garbage() {
        for input in [
            "",
            "nas",
            "//nas/share",
            "ftp://nas/x",
            "http://x",
            "smb://",
            "smb:///x",
            "smb://:445/x",
            "file:///tmp",
        ] {
            assert_eq!(normalize(input), Err(AddressError::Invalid), "{input:?}");
        }
    }

    #[test]
    fn refuses_passwords_in_addresses() {
        assert_eq!(
            normalize("smb://alice:hunter2@nas/media"),
            Err(AddressError::PasswordInAddress)
        );
        assert_eq!(
            normalize("nfs://a:b@h/x"),
            Err(AddressError::PasswordInAddress)
        );
    }

    #[test]
    fn key_ignores_what_gvfs_adds_or_drops() {
        let saved = key("smb://nas/media");
        assert_eq!(key("smb://alice@nas/media/"), saved);
        assert_eq!(key("SMB://NAS/media"), saved);
        assert_ne!(key("smb://nas/other"), saved);
        assert_ne!(key("nfs://nas/media"), saved);
    }

    #[test]
    fn names_shares_for_people() {
        assert_eq!(display_name("smb://nas/media"), "media on nas");
        assert_eq!(
            display_name("smb://nas/Family%20Photos"),
            "Family Photos on nas"
        );
        assert_eq!(
            display_name("nfs://10.0.0.5/export/data"),
            "data on 10.0.0.5"
        );
        assert_eq!(display_name("smb://nas"), "nas");
    }

    #[test]
    fn recognises_network_entries_in_fstab() {
        for spec in [
            "//nas/media",
            "nas:/export",
            "10.0.0.5:/export/data",
            "[fe80::1]:/x",
        ] {
            assert!(is_network_device(spec), "{spec}");
        }
        for spec in [
            "/dev/sda1",
            "UUID=1234",
            "tmpfs",
            "/dev/disk/by-id/x:/y",
            "LABEL=data",
        ] {
            assert!(!is_network_device(spec), "{spec}");
        }
    }

    const MOUNTINFO: &str = "\
22 28 0:21 / /sys rw,nosuid shared:7 - sysfs sysfs rw
36 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
90 36 0:50 / /mnt/nas rw,relatime shared:60 - nfs4 nas:/export rw,vers=4.2
91 36 0:51 / /mnt/My\\040Music rw,relatime shared:61 - cifs //nas/music rw,vers=3.1.1
92 36 0:52 / /mnt/backup rw,relatime - fuse.sshfs me@host:/ rw
garbage line without a separator
";

    #[test]
    fn finds_network_mounts_in_mountinfo() {
        assert_eq!(
            network_mount_points_in(MOUNTINFO),
            [PathBuf::from("/mnt/nas"), PathBuf::from("/mnt/My Music")]
        );
        assert_eq!(parse_mountinfo(MOUNTINFO).len(), 5);
    }

    #[test]
    fn saved_shares_round_trip_and_survive_junk() {
        let dir = std::env::temp_dir().join(format!("volumes-shares-test-{}", std::process::id()));
        let path = dir.join("sub").join("shares");
        let _ = fs::remove_dir_all(&dir);

        assert!(load_from(&path).is_empty(), "no file yet");

        let shares = vec![
            "smb://nas/media".to_string(),
            "nfs://nas/export".to_string(),
        ];
        save_to(&path, &shares).unwrap();
        assert_eq!(load_from(&path), shares);
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file renamed away"
        );

        fs::write(
            &path,
            "# comment\nsmb://nas/media\n\nnot an address\nsmb://u:pw@nas/secret\nsmb://nas/media\nftp://x/y\n",
        )
        .unwrap();
        assert_eq!(load_from(&path), ["smb://nas/media"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
