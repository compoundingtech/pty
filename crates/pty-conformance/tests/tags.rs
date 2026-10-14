//! Port of tests/tags.test.ts: tags persisted in metadata, `list --json` /
//! `--filter-tag` / hashtags / hidden bookkeeping keys / `--tags`, tags across
//! exit, restart, and `run -a`. Node starts most daemons from
//! `PTY_SERVER_CONFIG`; here every session is created with `pty run -d --tag`,
//! which accepts the same keys (including the reserved ones).

use pty_conformance::*;
use serde_json::json;

fn tagged(tags: &[(&str, &str)]) -> DaemonOpts {
    let mut o = DaemonOpts::no_display_name();
    for (k, v) in tags {
        o = o.tag(k, v);
    }
    o
}

/// node: tests/tags.test.ts:106
#[test]
fn tags_are_persisted_in_metadata() {
    let rig = Rig::new();
    let name = unique_id("tag");
    let d = rig.daemon(&name, &["cat"], tagged(&[("owner", "forge"), ("env", "dev")]));
    assert_eq!(d.meta()["tags"], json!({"owner": "forge", "env": "dev"}));
}

/// node: tests/tags.test.ts:116
#[test]
fn tags_appear_in_list_json() {
    let rig = Rig::new();
    let name = unique_id("tag");
    rig.daemon(&name, &["cat"], tagged(&[("owner", "myapp")]));
    let s = rig.list_entry(&name).expect("listed");
    assert_eq!(s["tags"], json!({"owner": "myapp"}));
}

/// node: tests/tags.test.ts:130
#[test]
fn filter_tag_filters_json_output() {
    let rig = Rig::new();
    let m = unique_id("tagm");
    let o = unique_id("tago");
    rig.daemon(&m, &["cat"], tagged(&[("layout", "work"), ("role", "srv")]));
    rig.daemon(&o, &["cat"], tagged(&[("layout", "play")]));
    let out = expect_json(&rig.pty(&["list", "--json", "--filter-tag", "layout=work"]));
    let names: Vec<&str> = out.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec![m.as_str()]);
}

/// node: tests/tags.test.ts:142
#[test]
fn filter_tag_requires_all_tags_to_match() {
    let rig = Rig::new();
    let both = unique_id("tagb");
    let one = unique_id("tago");
    rig.daemon(&both, &["cat"], tagged(&[("layout", "work"), ("role", "srv")]));
    rig.daemon(&one, &["cat"], tagged(&[("layout", "work")]));
    let out = expect_json(&rig.pty(&[
        "list", "--json", "--filter-tag", "layout=work", "--filter-tag", "role=srv",
    ]));
    let names: Vec<&str> = out.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec![both.as_str()]);
}

/// node: tests/tags.test.ts:154
#[test]
fn list_shows_tags_as_hashtags_by_default() {
    let rig = Rig::new();
    let name = unique_id("tag");
    rig.daemon(&name, &["cat"], tagged(&[("role", "web"), ("env", "dev")]));
    let out = rig.pty(&["list"]);
    let s = out.stdout();
    expect_contains(&s, &name);
    expect_contains(&s, "#role=web");
    expect_contains(&s, "#env=dev");
}

/// node: tests/tags.test.ts:165
#[test]
fn list_hides_bookkeeping_tags_by_default() {
    let rig = Rig::new();
    let name = unique_id("tag");
    rig.daemon(
        &name,
        &["cat"],
        tagged(&[
            ("role", "web"),
            ("ptyfile", "/some/path/pty.toml"),
            ("ptyfile.session", "s"),
            ("ptyfile.tags", "role"),
            ("strategy", "permanent"),
        ]),
    );
    let s = rig.pty(&["list"]).stdout();
    expect_contains(&s, "#role=web");
    expect_not_contains(&s, "#ptyfile=");
    expect_not_contains(&s, "#ptyfile.session=");
    expect_not_contains(&s, "#ptyfile.tags=");
    expect_not_contains(&s, "#strategy=");
    expect_contains(&s, "[permanent]");
}

/// node: tests/tags.test.ts:185
#[test]
fn list_tags_includes_bookkeeping_tags() {
    let rig = Rig::new();
    let name = unique_id("tag");
    rig.daemon(
        &name,
        &["cat"],
        tagged(&[("role", "web"), ("ptyfile", "/some/path/pty.toml"), ("strategy", "permanent")]),
    );
    let s = rig.pty(&["list", "--tags"]).stdout();
    expect_contains(&s, "#role=web");
    expect_contains(&s, "#ptyfile=");
    expect_contains(&s, "#strategy=permanent");
}

/// node: tests/tags.test.ts:200
#[test]
fn list_hides_colon_tags_by_default_and_shows_them_with_tags() {
    let rig = Rig::new();
    let name = unique_id("tag");
    rig.daemon(
        &name,
        &["cat"],
        tagged(&[("role", "web"), (":l1234-abc", "1"), (":layout", "grid")]),
    );
    let s = rig.pty(&["list"]).stdout();
    expect_contains(&s, "#role=web");
    expect_not_contains(&s, ":l1234-abc");
    expect_not_contains(&s, ":layout");
    let t = rig.pty(&["list", "--tags"]).stdout();
    expect_contains(&t, "#:l1234-abc=1");
    expect_contains(&t, "#:layout=grid");
}

/// node: tests/tags.test.ts:219
#[test]
fn filter_tag_filters_text_output() {
    let rig = Rig::new();
    let m = unique_id("tagm");
    let o = unique_id("tago");
    rig.daemon(&m, &["cat"], tagged(&[("layout", "work")]));
    rig.daemon(&o, &["cat"], tagged(&[("layout", "play")]));
    let s = rig.pty(&["list", "--filter-tag", "layout=work"]).stdout();
    expect_contains(&s, &m);
    expect_not_contains(&s, &o);
}

/// node: tests/tags.test.ts:231
#[test]
fn sessions_without_tags_have_no_tags_field() {
    let rig = Rig::new();
    let name = unique_id("tag");
    let d = rig.daemon(&name, &["cat"], DaemonOpts::no_display_name());
    assert!(d.meta().get("tags").is_none(), "{}", d.meta());
}

/// node: tests/tags.test.ts:241
#[test]
fn tags_survive_process_exit() {
    let rig = Rig::new();
    let name = unique_id("tag");
    let d = rig.daemon(&name, &["true"], tagged(&[("owner", "ci"), ("keep", "true")]));
    rig.wait_for_exit(&name);
    let meta = d.meta();
    assert_eq!(meta["tags"], json!({"owner": "ci", "keep": "true"}));
    assert_eq!(meta["exitCode"], 0);
}

/// node: tests/tags.test.ts:256
#[test]
fn run_tag_flag_sets_tags() {
    let rig = Rig::new();
    let name = unique_id("tag");
    let out = rig.pty(&[
        "run", "-d", "--id", &name, "--tag", "owner=forge", "--tag", "env=staging", "--", "cat",
    ]);
    expect_status(&out, 0);
    let meta = rig.meta(&name).expect("metadata");
    assert_eq!(meta["tags"], json!({"owner": "forge", "env": "staging"}));
}

/// node: tests/tags.test.ts:282
#[test]
fn tags_survive_restart() {
    let rig = Rig::new();
    let name = unique_id("tag");
    let d = rig.daemon(
        &name,
        &["true"],
        tagged(&[("owner", "forge"), ("env", "prod"), ("keep", "true")]),
    );
    rig.wait_for_exit(&name);
    let before = d.meta();
    assert_eq!(before["tags"], json!({"owner": "forge", "env": "prod", "keep": "true"}));
    assert_eq!(before["exitCode"], 0);

    // Inside a session, restart reports and returns instead of attaching.
    let out = rig.pty_env(&[("PTY_SESSION", "outer")], &["restart", "-y", &name]);
    expect_status(&out, 0);
    wait_until("restarted session metadata", || {
        rig.meta(&name).map(|m| m["createdAt"] != before["createdAt"]).unwrap_or(false)
    });
    let after = rig.meta(&name).unwrap();
    assert_eq!(after["tags"], json!({"owner": "forge", "env": "prod", "keep": "true"}));
    assert_ne!(after["createdAt"], before["createdAt"]);
}

/// node: tests/tags.test.ts:314
#[test]
fn run_a_preserves_tags_of_an_exited_session() {
    let rig = Rig::new();
    let name = unique_id("tag");
    rig.daemon(&name, &["true"], tagged(&[("owner", "ci"), ("keep", "true")]));
    rig.wait_for_exit(&name);
    let out = rig.pty(&["run", "-a", "-d", "--id", &name, "--", "cat"]);
    expect_status(&out, 0);
    let meta = rig.meta(&name).unwrap();
    assert_eq!(meta["tags"], json!({"owner": "ci", "keep": "true"}));
}

/// node: tests/tags.test.ts:343
#[test]
fn run_a_with_new_tags_replaces_the_old_ones() {
    let rig = Rig::new();
    let name = unique_id("tag");
    rig.daemon(&name, &["true"], tagged(&[("owner", "old"), ("keep", "true")]));
    rig.wait_for_exit(&name);
    let out = rig.pty(&["run", "-a", "-d", "--id", &name, "--tag", "owner=new", "--", "cat"]);
    expect_status(&out, 0);
    let meta = rig.meta(&name).unwrap();
    assert_eq!(meta["tags"], json!({"owner": "new"}));
}

/// node: tests/tags.test.ts:375
#[test]
fn run_tag_with_invalid_format_is_rejected() {
    let rig = Rig::new();
    let out = rig.pty(&["run", "-d", "--id", "bad-tag", "--tag", "no-equals-sign", "--", "cat"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "key=value");
}
