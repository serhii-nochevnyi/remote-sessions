use crate::model::{ProductIssue, ProviderProfile};
use nix::libc;
use std::{
    ffi::{CString, OsStr},
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};

const MAX_LOCATOR_BYTES: usize = 4096;
const POLICY_VERSION: u32 = 1;
const MAX_INTERPRETER_DEPTH: u8 = 4;

#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub canonical: String,
    pub device: u64,
    pub inode: u64,
}

pub fn workspace_locator(path: &Path) -> Result<&str, ProductIssue> {
    let locator = path.to_str().ok_or(ProductIssue::UnsupportedPath)?;
    if !path.is_absolute() {
        return Err(ProductIssue::InvalidFolder);
    }
    if locator.len() > MAX_LOCATOR_BYTES || locator.chars().any(char::is_control) {
        return Err(ProductIssue::UnsupportedPath);
    }
    Ok(locator)
}

fn folder_error(error: io::Error) -> ProductIssue {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ProductIssue::FolderAccessDenied,
        io::ErrorKind::NotADirectory => ProductIssue::InvalidFolder,
        _ => ProductIssue::FolderUnavailable,
    }
}

fn service_access(path: &Path, mode: libc::c_int) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // @security: Check access with the daemon's effective identity and filesystem policy.
    let result = unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_EACCESS) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn resolve_workspace(locator: &Path) -> Result<WorkspaceIdentity, ProductIssue> {
    workspace_locator(locator)?;
    let canonical = fs::canonicalize(locator).map_err(folder_error)?;
    let path = workspace_locator(&canonical)?;
    let directory = fs::metadata(&canonical).map_err(folder_error)?;
    if !directory.is_dir() {
        return Err(ProductIssue::InvalidFolder);
    }
    service_access(locator, libc::R_OK | libc::X_OK).map_err(folder_error)?;
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(locator)
        .map_err(folder_error)?
        .metadata()
        .map_err(folder_error)?;
    if (opened.dev(), opened.ino()) != (directory.dev(), directory.ino()) {
        return Err(ProductIssue::WorkspaceChanged);
    }

    Ok(WorkspaceIdentity {
        canonical: path.to_owned(),
        device: opened.dev(),
        inode: opened.ino(),
    })
}

pub struct PreparedLaunch {
    pub policy_version: u32,
    pub workspace: WorkspaceIdentity,
    pub executable: PathBuf,
    pub path: std::ffi::OsString,
}

pub struct ServiceProfiles {
    search: Vec<PathBuf>,
}

impl ServiceProfiles {
    pub fn new(path: Option<&OsStr>) -> Self {
        let search = path
            .into_iter()
            .flat_map(std::env::split_paths)
            .filter(|entry| {
                entry.is_absolute() && !entry.components().any(|part| part == Component::ParentDir)
            })
            .collect();
        Self { search }
    }

    pub fn preflight(
        &self,
        locator: &Path,
        saved: &WorkspaceIdentity,
        profile: ProviderProfile,
    ) -> Result<PreparedLaunch, ProductIssue> {
        let current = resolve_workspace(locator).map_err(|issue| match issue {
            ProductIssue::InvalidFolder | ProductIssue::FolderUnavailable => {
                ProductIssue::WorkspaceChanged
            }
            issue => issue,
        })?;
        if &current != saved {
            return Err(ProductIssue::WorkspaceChanged);
        }

        let name = match profile {
            ProviderProfile::ClaudeCode => "claude",
            ProviderProfile::Codex => "codex",
        };
        let executable = self.lookup(name)?;
        self.verify_entrypoint(&executable, MAX_INTERPRETER_DEPTH)
            .map_err(|issue| match issue {
                EntryPointIssue::Invalid => ProductIssue::ProviderNotExecutable,
                EntryPointIssue::Interpreter => ProductIssue::InterpreterUnavailable,
            })?;
        Ok(PreparedLaunch {
            policy_version: POLICY_VERSION,
            workspace: current,
            executable,
            path: std::env::join_paths(&self.search)
                .map_err(|_| ProductIssue::ProviderNotExecutable)?,
        })
    }

    fn lookup(&self, name: &str) -> Result<PathBuf, ProductIssue> {
        let mut unusable = false;
        for directory in &self.search {
            let candidate = directory.join(name);
            match executable_path(&candidate) {
                Ok(path) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => unusable = true,
            }
        }
        Err(if unusable {
            ProductIssue::ProviderNotExecutable
        } else {
            ProductIssue::ProviderMissing
        })
    }

    fn verify_entrypoint(&self, path: &Path, remaining: u8) -> Result<(), EntryPointIssue> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| EntryPointIssue::Invalid)?;
        if !file
            .metadata()
            .map_err(|_| EntryPointIssue::Invalid)?
            .is_file()
        {
            return Err(EntryPointIssue::Invalid);
        }
        let mut header = Vec::new();
        (&mut file)
            .take(512)
            .read_to_end(&mut header)
            .map_err(|_| EntryPointIssue::Invalid)?;
        let Some(script) = header.strip_prefix(b"#!") else {
            return native_executable(&header)
                .then_some(())
                .ok_or(EntryPointIssue::Invalid);
        };
        if remaining == 0 {
            return Err(EntryPointIssue::Interpreter);
        }
        let (line, _) = script.split_at(
            script
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(script.len()),
        );
        if line.len() == 510 || line.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
            return Err(EntryPointIssue::Interpreter);
        }
        let shebang = std::str::from_utf8(line).map_err(|_| EntryPointIssue::Interpreter)?;
        let mut words = shebang.split_ascii_whitespace();
        let interpreter = words
            .next()
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .ok_or(EntryPointIssue::Interpreter)?;
        let mut resolved =
            executable_path(interpreter).map_err(|_| EntryPointIssue::Interpreter)?;
        if resolved == Path::new("/usr/bin/env") {
            let first = words.next().ok_or(EntryPointIssue::Interpreter)?;
            let name = if first == "-S" {
                words.next().ok_or(EntryPointIssue::Interpreter)?
            } else {
                if words.next().is_some() {
                    return Err(EntryPointIssue::Interpreter);
                }
                first
            };
            if !name.starts_with(|c: char| c.is_ascii_alphanumeric())
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            {
                return Err(EntryPointIssue::Interpreter);
            }
            resolved = self
                .lookup(name)
                .map_err(|_| EntryPointIssue::Interpreter)?;
        }
        self.verify_entrypoint(&resolved, remaining - 1)
            .map_err(|_| EntryPointIssue::Interpreter)
    }
}

#[derive(Debug)]
enum EntryPointIssue {
    Invalid,
    Interpreter,
}

fn executable_path(path: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    if !canonical.metadata()?.is_file() || workspace_locator(&canonical).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid executable path",
        ));
    }
    service_access(path, libc::X_OK)?;
    Ok(canonical)
}

fn native_executable(header: &[u8]) -> bool {
    #[cfg(target_os = "macos")]
    {
        matches!(
            header.get(..4),
            Some(
                [0xfe, 0xed, 0xfa, 0xce | 0xcf]
                    | [0xce | 0xcf, 0xfa, 0xed, 0xfe]
                    | [0xca, 0xfe, 0xba, 0xbe | 0xbf]
                    | [0xbe | 0xbf, 0xba, 0xfe, 0xca]
            )
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        header.starts_with(b"\x7fELF")
    }
}
