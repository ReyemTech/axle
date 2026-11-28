use axle::store::{Priced, Pricer, UsageCounts};
use axle::usage::prices::PriceTable;

fn c(input: i64, output: i64, cw5m: i64, cw1h: i64, cr: i64) -> UsageCounts {
    UsageCounts {
        input,
        output,
        cache_write_5m: cw5m,
        cache_write_1h: cw1h,
        cache_read: cr,
        reasoning: 0,
    }
}

fn model(id: &str, extra: &str) -> String {
    format!("[[model]]\nid = \"{id}\"\n{extra}\n")
}

#[test]
fn builtin_parses_and_prices_the_spec_example() {
    let t = PriceTable::builtin();
    assert_eq!(t.version(), "2026-11-28");
    let p = t
        .price("claude-opus-5-5", None, &c(12, 845, 0, 3120, 48211))
        .unwrap();
    // 12×4 + 845×20 + 3120×8 + 48211×0.2 = 51550.2
    assert_eq!(
        p,
        Priced {
            cost_micros: 51550,
            price_ref: "builtin:2026-11-28#claude-opus-5-5".into()
        }
    );
}

#[test]
fn fast_tier() {
    let t = PriceTable::builtin();
    let p = t
        .price("claude-opus-5-5", Some("fast"), &c(12, 845, 0, 3120, 48211))
        .unwrap();
    // 12×8 + 845×40 + 3120×8 + 48211×0.2 = 68498.2
    assert_eq!(p.cost_micros, 68498);
    // a speed the entry has no tier for is the base price
    let slow = t
        .price(
            "claude-opus-5-5",
            Some("standard"),
            &c(12, 845, 0, 3120, 48211),
        )
        .unwrap();
    assert_eq!(slow.cost_micros, 51550);
}

#[test]
fn reasoning_is_never_priced() {
    let t = PriceTable::builtin();
    let mut counts = c(0, 100, 0, 0, 0);
    counts.reasoning = 90;
    let p = t.price("claude-opus-5-5", None, &counts).unwrap();
    assert_eq!(p.cost_micros, 2000);
}

#[test]
fn glob_match_and_precedence() {
    let t = PriceTable::builtin();
    let counts = c(1000, 0, 0, 0, 0);
    for m in ["claude-opus-5-5-20261128", "anthropic/claude-opus-5-5"] {
        let p = t.price(m, None, &counts).unwrap();
        assert_eq!(p.price_ref, "builtin:2026-11-28#claude-opus-5-5", "{m}");
    }
    let text = format!(
        "version = \"o\"\n{}{}{}",
        model("x", "match = [\"claude-*\"]\ninput = 1\noutput = 1"),
        model("y", "match = [\"claude-opus-*\"]\ninput = 2\noutput = 2"),
        model("claude-opus-9", "input = 3\noutput = 3"),
    );
    let t = PriceTable::builtin()
        .with_override(&text, "o.toml")
        .unwrap();
    let r = |m: &str| t.price(m, None, &counts).unwrap().price_ref;
    assert_eq!(r("claude-opus-9-1"), "file:o#y"); // longer glob
    assert_eq!(r("claude-opus-9"), "file:o#claude-opus-9"); // exact beats any glob
    assert_eq!(r("claude-zzz"), "file:o#x");
}

#[test]
fn equal_glob_length_goes_to_the_first_entry() {
    let text = format!(
        "version = \"o\"\n{}{}",
        model("a", "match = [\"zz-*\"]\ninput = 1\noutput = 1"),
        model("b", "match = [\"zz-*\"]\ninput = 2\noutput = 2"),
    );
    let t = PriceTable::parse(&text, "o.toml").unwrap();
    let p = t.price("zz-1", None, &c(1, 0, 0, 0, 0)).unwrap();
    assert_eq!(p.price_ref, "file:o#a");
}

#[test]
fn defaults_and_1h_fallback() {
    let text = format!(
        "version = \"o\"\n{}",
        model("d", "input = 1\noutput = 2\ncache_write_5m = 3")
    );
    let t = PriceTable::parse(&text, "o.toml").unwrap();
    // 1 + 2 + 3 + 3 (1h falls back to 5m) + 1 (cache read falls back to input)
    assert_eq!(
        t.price("d", None, &c(1, 1, 1, 1, 1)).unwrap().cost_micros,
        10
    );
}

#[test]
fn long_context_above_compares_whole_prompt() {
    let text = format!(
        "version = \"o\"\n{}",
        model(
            "h",
            "input = 1\noutput = 0\nlong_context = { above = 100000, input = 2 }"
        )
    );
    let t = PriceTable::parse(&text, "o.toml").unwrap();
    assert_eq!(
        t.price("h", None, &c(50_000, 0, 0, 0, 60_000))
            .unwrap()
            .cost_micros,
        220_000
    );
    assert_eq!(
        t.price("h", None, &c(40_000, 0, 0, 0, 60_000))
            .unwrap()
            .cost_micros,
        100_000
    );
}

#[test]
fn fast_wins_over_long_context() {
    let text = format!(
        "version = \"o\"\n{}",
        model(
            "f",
            "input = 1\noutput = 0\nlong_context = { above = 10, input = 2 }\nfast = { input = 5 }"
        )
    );
    let t = PriceTable::parse(&text, "o.toml").unwrap();
    assert_eq!(
        t.price("f", Some("fast"), &c(100, 0, 0, 0, 0))
            .unwrap()
            .cost_micros,
        500
    );
    assert_eq!(
        t.price("f", None, &c(100, 0, 0, 0, 0)).unwrap().cost_micros,
        200
    );
}

#[test]
fn unknown_model_has_no_price() {
    assert!(
        PriceTable::builtin()
            .price("nope-1", None, &c(1, 1, 0, 0, 0))
            .is_none()
    );
}

#[test]
fn builtin_has_a_codex_model() {
    let t = PriceTable::builtin();
    let p = t
        .price("gpt-5.3-codex", None, &c(1_000_000, 0, 0, 0, 0))
        .unwrap();
    assert_eq!(p.cost_micros, 1_750_000);
}

#[test]
fn override_replaces_by_id_and_adds() {
    let base = PriceTable::builtin();
    let text = format!(
        "version = \"2026-12-01\"\n{}{}",
        model("claude-opus-5-5", "input = 1\noutput = 1"),
        model("new-1", "input = 1\noutput = 1"),
    );
    let t = base.clone().with_override(&text, "p.toml").unwrap();
    assert_eq!(t.len(), base.len() + 1);
    let p = t
        .price("claude-opus-5-5", None, &c(10, 10, 0, 0, 0))
        .unwrap();
    assert_eq!(p.price_ref, "file:2026-12-01#claude-opus-5-5");
    assert_eq!(p.cost_micros, 20);
    // replaced entirely: the built-in fast tier is gone
    assert_eq!(
        t.price("claude-opus-5-5", Some("fast"), &c(10, 10, 0, 0, 0))
            .unwrap()
            .cost_micros,
        20
    );
    let other = t
        .price("claude-sonnet-5-5", None, &c(1, 0, 0, 0, 0))
        .unwrap();
    assert!(other.price_ref.starts_with("builtin:"));
}

#[test]
fn bad_override_names_file_and_line() {
    let f = "/etc/axl/prices.toml";
    let e = PriceTable::builtin()
        .with_override(
            "version = \"v\"\n\n[[model]]\nid = \"a\"\ninput = \"x\"\noutput = 1\n",
            f,
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains(f) && e.contains("line 5"), "{e}");

    let dup = "version = \"v\"\n\n[[model]]\nid = \"a\"\ninput = 1\noutput = 1\n\n[[model]]\nid = \"a\"\ninput = 1\noutput = 1\n";
    let e = PriceTable::parse(dup, f).unwrap_err().to_string();
    assert!(e.contains(f) && e.contains("line 9"), "{e}");

    let typo = "version = \"v\"\n\n[[model]]\nid = \"a\"\ninptu = 1\noutput = 1\n";
    assert!(PriceTable::parse(typo, f).is_err());

    let neg = "version = \"v\"\n\n[[model]]\nid = \"a\"\ninput = -1\noutput = 1\n";
    let e = PriceTable::parse(neg, f).unwrap_err().to_string();
    assert!(e.contains("line 4"), "{e}");

    let glob = "version = \"v\"\n\n[[model]]\nid = \"a\"\nmatch = [\"\"]\ninput = 1\noutput = 1\n";
    let e = PriceTable::parse(glob, f).unwrap_err().to_string();
    assert!(e.contains("line 4"), "{e}");
}

#[test]
fn load_reads_the_override_file() {
    let dir = std::env::temp_dir().join(format!("axl-prices-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.toml");
    std::fs::write(
        &path,
        format!("version = \"z\"\n{}", model("zz", "input = 1\noutput = 1")),
    )
    .unwrap();
    let t = PriceTable::load(Some(&path)).unwrap();
    assert!(t.price("zz", None, &c(1, 0, 0, 0, 0)).is_some());
    assert_eq!(
        PriceTable::load(None).unwrap().len(),
        PriceTable::builtin().len()
    );
    let missing = PriceTable::load(Some(&dir.join("nope.toml")))
        .unwrap_err()
        .to_string();
    assert!(missing.contains("nope.toml"), "{missing}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn serve_refuses_a_bad_prices_file() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let dir = std::env::temp_dir().join(format!("axl-prices-serve-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.toml");
    std::fs::write(
        &bad,
        "version = \"v\"\n\n[[model]]\nid = \"a\"\ninput = \"x\"\noutput = 1\n",
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_axl"));
    cmd.arg("serve")
        .env_clear()
        .env("HOME", &dir)
        .env(
            "DATABASE_URL",
            format!("sqlite://{}", dir.join("a.db").display()),
        )
        .env("AXL_BIND", "127.0.0.1:0")
        .env("AXL_ADMIN_SECRET", "test-secret-0123456789")
        .env("AXL_PRICES_FILE", &bad)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(10) {
            child.kill().ok();
            panic!("serve did not exit on a bad prices file");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.success(), "{stderr}");
    assert!(
        stderr.contains("bad.toml") && stderr.contains("line 5"),
        "{stderr}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
