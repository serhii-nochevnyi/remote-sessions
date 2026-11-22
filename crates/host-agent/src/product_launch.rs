use crate::model::{ProductIssue, ProviderProfile};
use nix::libc;
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CString, OsStr},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

const MAX_LOCATOR_BYTES: usize = 4096;
const POLICY_VERSION: u32 = 1;
const MAX_INTERPRETER_DEPTH: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedLaunch {
    pub policy_version: u32,
    pub workspace: WorkspaceIdentity,
    pub executable: PathBuf,
    pub path: String,
    pub executable_identity: (u64, u64),
}

pub(crate) const NONCE_ENV: &str = "RS_CREATE_NONCE";
pub(crate) const LAUNCH_COMMAND: &str = "__managed-launch";

#[derive(Serialize, Deserialize)]
pub(crate) struct LaunchGuard {
    pub locator: String,
    pub profile: ProviderProfile,
    pub policy: PreparedLaunch,
    pub service_state: PathBuf,
    pub nonce: Uuid,
}

impl LaunchGuard {
    pub(crate) fn argument(&self) -> anyhow::Result<String> {
        let bytes = serde_json::to_vec(self)?;
        anyhow::ensure!(
            bytes.len() <= 16_384,
            "launch policy exceeds argument limit"
        );
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn decode(argument: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            argument.len() <= 32_768
                && argument.len().is_multiple_of(2)
                && argument.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid launch policy argument"
        );
        let bytes: Result<Vec<_>, _> = (0..argument.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&argument[index..index + 2], 16))
            .collect();
        Ok(serde_json::from_slice(&bytes?)?)
    }

    fn exec(&self) -> ProductIssue {
        if self.policy.policy_version != POLICY_VERSION
            || std::env::var(NONCE_ENV).ok().as_deref() != Some(&self.nonce.to_string())
        {
            return ProductIssue::LaunchSetupFailed;
        }
        let Ok(cwd) = fs::metadata(".") else {
            return ProductIssue::WorkspaceChanged;
        };
        if (cwd.dev(), cwd.ino()) != (self.policy.workspace.device, self.policy.workspace.inode) {
            return ProductIssue::WorkspaceChanged;
        }
        let profiles = ServiceProfiles::new(Some(OsStr::new(&self.policy.path)));
        match profiles.preflight(
            Path::new(&self.locator),
            &self.policy.workspace,
            self.profile,
        ) {
            Ok(policy) if policy == self.policy => {}
            Ok(_) => return ProductIssue::ProviderNotExecutable,
            Err(issue) => return issue,
        }
        let _error = std::process::Command::new(&self.policy.executable)
            .env("PATH", &self.policy.path)
            .env_remove(NONCE_ENV)
            .exec();
        ProductIssue::LaunchSetupFailed
    }
}

fn setup_fault_path(directory: &Path, nonce: Uuid) -> PathBuf {
    directory.join(format!("create-setup-{}", nonce.simple()))
}

pub(crate) fn read_setup_fault(directory: &Path, nonce: Uuid) -> Option<ProductIssue> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(setup_fault_path(directory, nonce))
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file()
        || meta.uid() != nix::unistd::geteuid().as_raw()
        || meta.mode() & 0o7777 != 0o600
        || meta.nlink() != 1
        || meta.len() > 128
    {
        return None;
    }
    let mut issue = String::new();
    file.take(129).read_to_string(&mut issue).ok()?;
    ProductIssue::decode(&issue)
}

/// Internal tmux child entry point. Only a fixed provider can be executed.
#[doc(hidden)]
pub fn managed_launch_argument(args: &[std::ffi::OsString]) -> Option<anyhow::Result<()>> {
    if args.first()? != LAUNCH_COMMAND {
        return None;
    }
    Some((|| {
        anyhow::ensure!(args.len() == 2, "invalid managed launch arguments");
        let guard = LaunchGuard::decode(args[1].to_str().unwrap_or_default())?;
        let issue = guard.exec();
        if crate::validate_directory(&guard.service_state).is_ok()
            && let Ok(mut file) = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(setup_fault_path(&guard.service_state, guard.nonce))
        {
            let _ = file.write_all(issue.encode().as_bytes());
        }
        anyhow::bail!("managed provider setup failed ({issue:?})")
    })())
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

        let executable = self.lookup(profile.executable_name())?;
        self.verify_entrypoint(&executable, MAX_INTERPRETER_DEPTH)
            .map_err(|issue| match issue {
                EntryPointIssue::Invalid => ProductIssue::ProviderNotExecutable,
                EntryPointIssue::Interpreter => ProductIssue::InterpreterUnavailable,
            })?;
        let metadata = executable
            .metadata()
            .map_err(|_| ProductIssue::ProviderNotExecutable)?;
        let path = std::env::join_paths(&self.search)
            .map_err(|_| ProductIssue::ProviderNotExecutable)?
            .into_string()
            .map_err(|_| ProductIssue::ProviderNotExecutable)?;
        Ok(PreparedLaunch {
            policy_version: POLICY_VERSION,
            workspace: current,
            executable,
            path,
            executable_identity: (metadata.dev(), metadata.ino()),
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
        let env = Path::new("/usr/bin/env");
        if interpreter == env || resolved == env {
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
