// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Lexical path normalization (root `TOOLCHAIN_POSIX_LAYER_DESIGN.md` §4).
//!
//! A path is resolved against the working directory, then reduced to WRNS's
//! canonical absolute form: `/`, or `/` and components joined by single
//! slashes. Empty components and `.` vanish; `..` removes the previous
//! component and never climbs above `/`, so the namespace root stays a
//! boundary. Symbolic links resolve on the server, never here.

use crate::errno::{ENAMETOOLONG, ENOENT, Result};
use wyrmroot_fs_proto::{MAX_NAME_BYTES, MAX_PATH_BYTES};

/// Room for one canonical absolute path.
pub type PathBuf = [u8; MAX_PATH_BYTES];

/// A normalized path in a caller's buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Normalized {
    /// Bytes of the canonical path at the start of the buffer.
    pub length: usize,
    /// The input ended in `/` (or `/.`, `/..`), so it must name a
    /// directory.
    pub directory: bool,
}

/// Resolves `input` against `cwd` (itself canonical and absolute) into
/// `output`. An empty input is `ENOENT`; a component or result too long for
/// WRNS is `ENAMETOOLONG`.
pub fn normalize(cwd: &[u8], input: &[u8], output: &mut PathBuf) -> Result<Normalized> {
    if input.is_empty() {
        return Err(ENOENT);
    }
    let mut length = 0usize;
    output[0] = b'/';
    length += 1;
    if input[0] != b'/' {
        // The working directory is already canonical: copy it as the base.
        if cwd.len() > 1 {
            output[..cwd.len()].copy_from_slice(cwd);
            length = cwd.len();
        }
    }
    let mut directory = false;
    for component in input.split(|&byte| byte == b'/') {
        directory = false;
        match component {
            b"" => directory = true,
            b"." => directory = true,
            b".." => {
                directory = true;
                // Drop the last component; at the root, stay there.
                if length > 1 {
                    let slash = output[..length]
                        .iter()
                        .rposition(|&byte| byte == b'/')
                        .unwrap_or(0);
                    length = slash.max(1);
                }
            }
            name => {
                if name.len() > MAX_NAME_BYTES {
                    return Err(ENAMETOOLONG);
                }
                let separator = usize::from(length > 1);
                let end = length + separator + name.len();
                if end > MAX_PATH_BYTES {
                    return Err(ENAMETOOLONG);
                }
                if separator == 1 {
                    output[length] = b'/';
                }
                output[length + separator..end].copy_from_slice(name);
                length = end;
            }
        }
    }
    // A bare `/` names a directory too, but says nothing more than itself.
    Ok(Normalized { length, directory })
}

/// Splits a canonical absolute path into its parent and last name. The root
/// has no name.
#[must_use]
pub fn split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() <= 1 {
        return None;
    }
    let slash = path.iter().rposition(|&byte| byte == b'/')?;
    let parent = if slash == 0 {
        &path[..1]
    } else {
        &path[..slash]
    };
    Some((parent, &path[slash + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &str, input: &str) -> Result<(String, bool)> {
        let mut buffer = [0u8; MAX_PATH_BYTES];
        let normalized = normalize(cwd.as_bytes(), input.as_bytes(), &mut buffer)?;
        Ok((
            String::from_utf8(buffer[..normalized.length].to_vec()).unwrap(),
            normalized.directory,
        ))
    }

    #[test]
    fn absolute_paths_collapse_slashes_and_dots() {
        assert_eq!(run("/", "/a//b/./c").unwrap(), ("/a/b/c".into(), false));
        assert_eq!(run("/x", "/").unwrap(), ("/".into(), true));
        assert_eq!(run("/x", "///").unwrap(), ("/".into(), true));
        assert_eq!(run("/", "/a/b/").unwrap(), ("/a/b".into(), true));
        assert_eq!(run("/", "/a/.").unwrap(), ("/a".into(), true));
    }

    #[test]
    fn relative_paths_start_at_the_working_directory() {
        assert_eq!(
            run("/home/u", "f.txt").unwrap(),
            ("/home/u/f.txt".into(), false)
        );
        assert_eq!(run("/", "f.txt").unwrap(), ("/f.txt".into(), false));
        assert_eq!(run("/home/u", ".").unwrap(), ("/home/u".into(), true));
        assert_eq!(run("/home/u", "../v").unwrap(), ("/home/v".into(), false));
    }

    #[test]
    fn dot_dot_never_climbs_above_the_root() {
        assert_eq!(run("/", "..").unwrap(), ("/".into(), true));
        assert_eq!(run("/a", "../../..").unwrap(), ("/".into(), true));
        assert_eq!(run("/", "/../etc").unwrap(), ("/etc".into(), false));
        assert_eq!(
            run("/a/b", "/a/b/../../../c").unwrap(),
            ("/c".into(), false)
        );
    }

    #[test]
    fn empty_and_oversized_paths_are_refused() {
        assert_eq!(run("/", ""), Err(ENOENT));
        let long_name = "n".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(run("/", &long_name), Err(ENAMETOOLONG));
        let name = "n".repeat(MAX_NAME_BYTES);
        let deep = format!("/{name}/{name}/{name}/{name}/{name}");
        assert_eq!(run("/", &deep), Err(ENAMETOOLONG));
        // Exactly the limit fits.
        let fits = format!("/{}", "m".repeat(MAX_PATH_BYTES - 1 - 3 * 256));
        let fits = format!("{fits}/{name}/{name}/{name}");
        assert_eq!(fits.len(), MAX_PATH_BYTES);
        assert_eq!(run("/", &fits).unwrap().0.len(), MAX_PATH_BYTES);
    }

    #[test]
    fn every_result_is_canonical_for_wrns() {
        for (cwd, input) in [
            ("/", "a/../b//c/."),
            ("/q/r", "../../../s"),
            ("/", "/"),
            ("/w", "./x/./y/.."),
        ] {
            let (path, _) = run(cwd, input).unwrap();
            assert!(
                wyrmroot_fs_proto::check_absolute(path.as_bytes()).is_ok(),
                "{path}"
            );
        }
    }

    #[test]
    fn split_gives_parent_and_name() {
        assert_eq!(split(b"/a/b"), Some((&b"/a"[..], &b"b"[..])));
        assert_eq!(split(b"/a"), Some((&b"/"[..], &b"a"[..])));
        assert_eq!(split(b"/"), None);
    }
}
