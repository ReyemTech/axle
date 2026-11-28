//! Cursor files, billing detection and the `[usage]` config table. Temp homes only; the
//! environment comes from a map, never the process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axle::client::{AxlDir, ClientConfig, UsageConfig, load_config, save_config, usage_off};
use axle::domain::Billing;
use axle::hook::Harness;
use axle::usage::billing::{Source, detect, resolve};
use axle::usage::cursor::{self, Cursor, Status};
use axle::usage::parsers::ParserState;

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("axle-usage-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn write(&self, rel: &str, text: &str) {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

#[test]
fn claude_billing_order() {
    let h = TempHome::new();
    let none = env(&[]);
    assert_eq!(
        detect(Harness::ClaudeCode, h.path(), &none),
        Billing::Unknown
    );

    h.write(".claude.json", r#"{"oauthAccount": {"x": "y"}}"#);
    assert_eq!(
        detect(Harness::ClaudeCode, h.path(), &none),
        Billing::Subscription
    );
    for name in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        let e = env(&[(name, "k")]);
        assert_eq!(
            detect(Harness::ClaudeCode, h.path(), &e),
            Billing::Api,
            "{name}"
        );
    }
    // An empty value does not count.
    let empty = env(&[("ANTHROPIC_API_KEY", "")]);
    assert_eq!(
        detect(Harness::ClaudeCode, h.path(), &empty),
        Billing::Subscription
    );

    h.write(".claude/settings.json", r#"{"apiKeyHelper": "echo k"}"#);
    assert_eq!(detect(Harness::ClaudeCode, h.path(), &none), Billing::Api);

    let h2 = TempHome::new();
    h2.write(".claude.json", r#"{"oauthAccount": null}"#);
    assert_eq!(
        detect(Harness::ClaudeCode, h2.path(), &none),
        Billing::Unknown
    );

    let mut cfg = UsageConfig::default();
    cfg.billing
        .insert("claude-code".into(), "subscription".into());
    let e = env(&[("ANTHROPIC_API_KEY", "k")]);
    assert_eq!(
        resolve(Harness::ClaudeCode, h.path(), &e, &cfg),
        (Billing::Subscription, Source::Config)
    );
    assert_eq!(
        resolve(Harness::ClaudeCode, h.path(), &e, &UsageConfig::default()),
        (Billing::Api, Source::Detected)
    );
}

#[test]
fn codex_billing() {
    let h = TempHome::new();
    let e = env(&[("OPENAI_API_KEY", "k")]);
    assert_eq!(detect(Harness::Codex, h.path(), &e), Billing::Unknown);
    h.write(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#);
    assert_eq!(detect(Harness::Codex, h.path(), &e), Billing::Subscription);
    h.write(".codex/auth.json", r#"{"auth_mode":"apikey"}"#);
    assert_eq!(detect(Harness::Codex, h.path(), &e), Billing::Api);
    h.write(".codex/auth.json", r#"{"auth_mode":"other"}"#);
    assert_eq!(detect(Harness::Codex, h.path(), &e), Billing::Unknown);
    h.write(".codex/auth.json", "not json");
    assert_eq!(detect(Harness::Codex, h.path(), &e), Billing::Unknown);
}

#[test]
fn usage_off_switches() {
    let d = ClientConfig::default();
    assert_eq!(usage_off(None, &d), None);
    assert_eq!(usage_off(Some("on"), &d), None);
    for v in ["off", "OFF", "0", "false", "No"] {
        assert_eq!(usage_off(Some(v), &d), Some("AXL_USAGE=off"), "{v}");
    }
    let cfg = ClientConfig {
        usage: Some(UsageConfig {
            enabled: Some(false),
            ..Default::default()
        }),
        ..ClientConfig::default()
    };
    assert_eq!(usage_off(None, &cfg), Some("[usage] enabled = false"));
    let on = ClientConfig {
        usage: Some(UsageConfig {
            enabled: Some(true),
            ..Default::default()
        }),
        ..ClientConfig::default()
    };
    assert_eq!(usage_off(None, &on), None);
}

#[test]
fn a_bad_billing_value_does_not_break_the_config() {
    let h = TempHome::new();
    let dir = AxlDir::new(h.path());
    dir.ensure().unwrap();
    std::fs::write(
        dir.config_path(),
        "server_url = \"http://example.test\"\n[usage]\nenabled = false\n\
         [usage.billing]\ncodex = \"apii\"\nclaude-code = \"subscription\"\n",
    )
    .unwrap();
    let cfg = load_config(&dir).unwrap();
    assert_eq!(cfg.server_url.as_deref(), Some("http://example.test"));
    assert_eq!(usage_off(None, &cfg), Some("[usage] enabled = false"));
    let u = cfg.usage.unwrap();
    assert_eq!(u.billing_for("codex"), None);
    assert_eq!(u.billing_for("claude-code"), Some(Billing::Subscription));
    let e = env(&[]);
    assert_eq!(
        resolve(Harness::Codex, h.path(), &e, &u).1,
        Source::Detected,
        "a value that is not a billing kind falls back to detection"
    );
}

#[test]
fn config_round_trip() {
    let h = TempHome::new();
    let dir = AxlDir::new(h.path());
    dir.ensure().unwrap();
    std::fs::write(
        dir.config_path(),
        "[usage]\nenabled = false\n[usage.billing]\ncodex = \"api\"\n",
    )
    .unwrap();
    let cfg = load_config(&dir).unwrap();
    let u = cfg.usage.clone().unwrap();
    assert_eq!(u.enabled, Some(false));
    assert_eq!(u.billing_for("codex"), Some(Billing::Api));
    save_config(&dir, &cfg).unwrap();
    assert_eq!(load_config(&dir).unwrap(), cfg);
    let text = std::fs::read_to_string(dir.config_path()).unwrap();
    assert!(text.contains("codex = \"api\""), "{text}");
}

fn sample(path: &Path) -> Cursor {
    Cursor {
        path: path.to_path_buf(),
        dev: 7,
        ino: 9,
        offset: 123,
        parser: "claude-code/1".into(),
        state: ParserState::default(),
        billing: Some(Billing::Api),
        session_id: None,
        server_url: Some("http://127.0.0.1:7420".into()),
        floor: Some("2026-11-28T00:00:00Z".into()),
        skip_partial: true,
        warned: false,
        updated_at: "2026-11-28T01:00:00Z".into(),
    }
}

#[test]
fn cursor_round_trip_private_and_atomic() {
    let h = TempHome::new();
    let t1 = Path::new("/work/a.jsonl");
    let t2 = Path::new("/work/b.jsonl");
    let c = sample(t1);
    cursor::save(h.path(), &c).unwrap();
    assert_eq!(cursor::load(h.path(), t1), Some(c));
    assert_eq!(cursor::load(h.path(), t2), None);
    assert_ne!(
        cursor::path_for(h.path(), t1),
        cursor::path_for(h.path(), t2)
    );

    let file = cursor::path_for(h.path(), t1);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.to_lowercase().contains("token"), "{text}");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&cursor::dir(h.path())), 0o700);
    }
    // No leftover temp file.
    let names: Vec<_> = std::fs::read_dir(cursor::dir(h.path()))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");

    std::fs::write(&file, "{ corrupt").unwrap();
    assert_eq!(cursor::load(h.path(), t1), None);
}

#[test]
fn prune_old_cursors_keeps_status() {
    let h = TempHome::new();
    let (old, fresh) = (Path::new("/work/old.jsonl"), Path::new("/work/new.jsonl"));
    cursor::save(h.path(), &sample(old)).unwrap();
    cursor::save(h.path(), &sample(fresh)).unwrap();
    let s = Status {
        last_success_at: Some("2026-01-01T00:00:00Z".into()),
        ..Default::default()
    };
    cursor::save_status(h.path(), &s).unwrap();
    assert_eq!(cursor::load_status(h.path()), s);

    let past = SystemTime::now() - Duration::from_secs(31 * 24 * 3600);
    for p in [
        cursor::path_for(h.path(), old),
        cursor::status_path(h.path()),
    ] {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(past)
            .unwrap();
    }
    let days30 = Duration::from_secs(30 * 24 * 3600);
    assert_eq!(cursor::prune(h.path(), SystemTime::now(), days30), 1);
    assert!(cursor::load(h.path(), old).is_none());
    assert!(cursor::load(h.path(), fresh).is_some());
    assert_eq!(cursor::load_status(h.path()), s);
}
