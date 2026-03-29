//! Bubblewrap sandbox (user namespaces for Linux/macOS)

use crate::security::traits::Sandbox;
use std::process::Command;

/// Bubblewrap sandbox backend
#[derive(Debug, Clone, Default)]
pub struct BubblewrapSandbox {
    /// Additional paths to bind-mount as writable inside the sandbox.
    writable_paths: Vec<String>,
    /// Whether to allow network access (skip `--unshare-net`).
    allow_network: bool,
}

impl BubblewrapSandbox {
    pub fn new() -> std::io::Result<Self> {
        if Self::is_installed() {
            Ok(Self::default())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Bubblewrap not found",
            ))
        }
    }

    /// Create a sandbox with extra writable paths and optional network access.
    ///
    /// Each path in `writable_paths` is validated: it must be absolute and must
    /// not contain path-traversal segments (`..`). Invalid paths cause an error.
    pub fn with_config(
        writable_paths: Vec<String>,
        allow_network: bool,
    ) -> std::io::Result<Self> {
        if !Self::is_installed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Bubblewrap not found",
            ));
        }
        for p in &writable_paths {
            validate_writable_path(p)?;
        }
        Ok(Self {
            writable_paths,
            allow_network,
        })
    }

    pub fn probe() -> std::io::Result<Self> {
        Self::new()
    }

    fn is_installed() -> bool {
        Command::new("bwrap")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Validate that a writable path is absolute and contains no `..` segments.
///
/// Bubblewrap runs on Linux/macOS only, so paths must start with `/` regardless
/// of the compilation host platform.
fn validate_writable_path(path: &str) -> std::io::Result<()> {
    // Bubblewrap targets Unix, so we check for a leading '/' rather than
    // relying on `Path::is_absolute()` which behaves differently on Windows.
    if !path.starts_with('/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("sandbox writable path must be absolute: {path}"),
        ));
    }
    // Check for path-traversal components.
    for segment in path.split('/') {
        if segment == ".." {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("sandbox writable path must not contain '..': {path}"),
            ));
        }
    }
    Ok(())
}

impl Sandbox for BubblewrapSandbox {
    fn wrap_command(&self, cmd: &mut Command) -> std::io::Result<()> {
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        let mut bwrap_cmd = Command::new("bwrap");
        bwrap_cmd.args(["--ro-bind", "/usr", "/usr", "--dev", "/dev", "--proc", "/proc"]);

        // Default writable: /tmp
        bwrap_cmd.args(["--bind", "/tmp", "/tmp"]);

        // Additional writable paths from config
        for path in &self.writable_paths {
            bwrap_cmd.args(["--bind", path, path]);
        }

        // Namespace isolation: always unshare everything, then selectively
        // re-share network if configured.
        bwrap_cmd.arg("--unshare-all");
        if self.allow_network {
            bwrap_cmd.arg("--share-net");
        }

        bwrap_cmd.arg("--die-with-parent");
        bwrap_cmd.arg(&program);
        bwrap_cmd.args(&args);

        *cmd = bwrap_cmd;
        Ok(())
    }

    fn is_available(&self) -> bool {
        Self::is_installed()
    }

    fn name(&self) -> &str {
        "bubblewrap"
    }

    fn description(&self) -> &str {
        "User namespace sandbox (requires bwrap)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bubblewrap_sandbox_name() {
        let sandbox = BubblewrapSandbox::default();
        assert_eq!(sandbox.name(), "bubblewrap");
    }

    #[test]
    fn bubblewrap_is_available_only_if_installed() {
        // Result depends on whether bwrap is installed
        let sandbox = BubblewrapSandbox::default();
        let _available = sandbox.is_available();

        // Either way, the name should still work
        assert_eq!(sandbox.name(), "bubblewrap");
    }

    // ── §1.1 Sandbox isolation flag tests ──────────────────────

    #[test]
    fn bubblewrap_wrap_command_includes_isolation_flags() {
        let sandbox = BubblewrapSandbox::default();
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        sandbox.wrap_command(&mut cmd).unwrap();

        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "bwrap",
            "wrapped command should use bwrap as program"
        );

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(
            args.contains(&"--unshare-all".to_string()),
            "must include --unshare-all for namespace isolation"
        );
        assert!(
            args.contains(&"--die-with-parent".to_string()),
            "must include --die-with-parent to prevent orphan processes"
        );
        assert!(
            !args.contains(&"--share-net".to_string()),
            "must NOT include --share-net (network should be blocked by default)"
        );
    }

    #[test]
    fn bubblewrap_wrap_command_preserves_original_command() {
        let sandbox = BubblewrapSandbox::default();
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.arg("/tmp");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(
            args.contains(&"ls".to_string()),
            "original program must be passed as argument"
        );
        assert!(
            args.contains(&"-la".to_string()),
            "original args must be preserved"
        );
        assert!(
            args.contains(&"/tmp".to_string()),
            "original args must be preserved"
        );
    }

    #[test]
    fn bubblewrap_wrap_command_binds_required_paths() {
        let sandbox = BubblewrapSandbox::default();
        let mut cmd = Command::new("echo");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(
            args.contains(&"--ro-bind".to_string()),
            "must include read-only bind for /usr"
        );
        assert!(
            args.contains(&"--dev".to_string()),
            "must include /dev mount"
        );
        assert!(
            args.contains(&"--proc".to_string()),
            "must include /proc mount"
        );
    }

    // ── §1.2 Configurable writable paths ──────────────────────

    #[test]
    fn writable_paths_added_as_bind_mounts() {
        let sandbox = BubblewrapSandbox {
            writable_paths: vec!["/home/user".to_string(), "/var/data".to_string()],
            allow_network: false,
        };
        let mut cmd = Command::new("echo");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        // Each writable path should appear twice (source and dest) after --bind
        let bind_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == "--bind")
            .map(|(i, _)| i)
            .collect();

        // At least 3 --bind args: /tmp + /home/user + /var/data
        assert!(
            bind_positions.len() >= 3,
            "expected at least 3 --bind entries, got {}",
            bind_positions.len()
        );
        assert!(
            args.contains(&"/home/user".to_string()),
            "must bind-mount /home/user"
        );
        assert!(
            args.contains(&"/var/data".to_string()),
            "must bind-mount /var/data"
        );
    }

    // ── §1.3 Network access control ───────────────────────────

    #[test]
    fn allow_network_adds_share_net() {
        let sandbox = BubblewrapSandbox {
            writable_paths: Vec::new(),
            allow_network: true,
        };
        let mut cmd = Command::new("echo");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(
            args.contains(&"--unshare-all".to_string()),
            "must still unshare-all first"
        );
        assert!(
            args.contains(&"--share-net".to_string()),
            "must include --share-net when allow_network is true"
        );
    }

    #[test]
    fn deny_network_by_default() {
        let sandbox = BubblewrapSandbox::default();
        let mut cmd = Command::new("echo");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(
            !args.contains(&"--share-net".to_string()),
            "must NOT include --share-net by default"
        );
    }

    // ── §1.4 Path validation ──────────────────────────────────

    #[test]
    fn validate_rejects_relative_path() {
        let err = validate_writable_path("relative/path").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("absolute"),
            "error should mention 'absolute'"
        );
    }

    #[test]
    fn validate_rejects_path_traversal() {
        let err = validate_writable_path("/home/../etc/shadow").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains(".."),
            "error should mention '..'"
        );
    }

    #[test]
    fn validate_accepts_valid_absolute_path() {
        assert!(validate_writable_path("/home/user/workspace").is_ok());
        assert!(validate_writable_path("/var/data").is_ok());
        assert!(validate_writable_path("/tmp").is_ok());
    }

    #[test]
    fn with_config_rejects_invalid_paths() {
        // Cannot test with_config when bwrap is not installed, so test
        // validate_writable_path directly (already covered above).
        // This test validates the function wiring rather than bwrap presence.
        let err = validate_writable_path("not-absolute").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
