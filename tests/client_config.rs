use std::path::{Path, PathBuf};

use axle::client::{
    AxlDir, ClientConfig, ClientError, load_config, read_token, remove_token, resolve_server_url,
    resolve_server_url_with_warning, save_config, write_token,
};

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("axl-client-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn server_url_env_beats_config_beats_default() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    assert_eq!(resolve_server_url(None, &dir), "http://127.0.0.1:7420");
    save_config(
        &dir,
        &ClientConfig {
            server_url: Some("https://axle.example.com/".into()),
            port: None,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    assert_eq!(resolve_server_url(None, &dir), "https://axle.example.com");
    assert_eq!(
        resolve_server_url(Some("http://10.0.0.5:7420"), &dir),
        "http://10.0.0.5:7420"
    );
    assert_eq!(
        resolve_server_url(Some("  "), &dir),
        "https://axle.example.com"
    );
}

#[test]
fn token_env_beats_file() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    write_token(&dir, "axl_file").unwrap();
    assert_eq!(read_token(Some("axl_env"), &dir).unwrap(), "axl_env");
    assert_eq!(read_token(Some(" "), &dir).unwrap(), "axl_file");
    assert_eq!(read_token(None, &dir).unwrap(), "axl_file");
    assert!(remove_token(&dir).unwrap());
    assert!(!remove_token(&dir).unwrap());
    assert_eq!(
        read_token(None, &dir).unwrap_err(),
        "no token (set AXL_TOKEN or write ~/.axl/token)"
    );
}

#[cfg(unix)]
#[test]
fn files_and_dir_have_private_modes() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    write_token(&dir, "axl_x").unwrap();
    save_config(&dir, &ClientConfig::default()).unwrap();
    assert_eq!(mode(&dir.root), 0o700);
    assert_eq!(mode(&dir.token_path()), 0o600);
    assert_eq!(mode(&dir.config_path()), 0o600);
    // Overwriting keeps the mode and leaves no temp file behind.
    write_token(&dir, "axl_y").unwrap();
    assert_eq!(mode(&dir.token_path()), 0o600);
    assert!(!dir.root.join("token.tmp").exists());
}

#[cfg(unix)]
#[test]
fn ensure_tightens_a_loose_dir() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    std::fs::create_dir(&dir.root).unwrap();
    std::fs::set_permissions(&dir.root, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir.ensure().unwrap();
    assert_eq!(mode(&dir.root), 0o700);
}

#[test]
fn missing_config_is_default_and_bad_toml_is_an_error() {
    let home = TempHome::new();
    let dir = AxlDir::new(home.path());
    assert_eq!(load_config(&dir).unwrap(), ClientConfig::default());
    dir.ensure().unwrap();
    std::fs::write(dir.config_path(), "server_url = [").unwrap();
    assert!(matches!(load_config(&dir), Err(ClientError::Toml(_))));
    // A broken file never blocks resolution.
    assert_eq!(resolve_server_url(None, &dir), "http://127.0.0.1:7420");
    let (url, warning) = resolve_server_url_with_warning(None, &dir);
    assert_eq!(url, "http://127.0.0.1:7420");
    let warning = warning.expect("a malformed config warns");
    assert!(
        warning.starts_with("axl: ignoring ~/.axl/config.toml: "),
        "{warning}"
    );
    // The env decides the URL, so the file is not consulted and nothing is warned about.
    let (url, warning) = resolve_server_url_with_warning(Some("http://h:1"), &dir);
    assert_eq!((url.as_str(), warning), ("http://h:1", None));
}
