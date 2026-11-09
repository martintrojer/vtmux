//! The published crates' manifests agree with `scripts/publish.sh` and with
//! each other (docs/RELEASING.md): what `cargo publish` would only find out
//! halfway through a release.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace root")
        .to_path_buf()
}

fn read(path: &Path) -> Table {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Where `publish.sh`'s `manifest` finds a crate's manifest.
fn manifest(name: &str) -> PathBuf {
    let root = root();
    match name {
        "vtmux-ghostty-vt-sys" => {
            root.join("third_party/libghostty-rs/libghostty-vt-sys/Cargo.toml")
        }
        "vtmux-ghostty-vt" => root.join("third_party/libghostty-rs/libghostty-vt/Cargo.toml"),
        _ => root.join("crates").join(name).join("Cargo.toml"),
    }
}

/// `publish.sh`'s `crates=(...)`: the publish order.
fn publish_order() -> Vec<String> {
    let path = root().join("scripts/publish.sh");
    let text = std::fs::read_to_string(&path).expect("scripts/publish.sh");
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("crates=("))
        .and_then(|l| l.strip_suffix(')'))
        .expect("publish.sh has a crates=(...) line");
    line.split_whitespace().map(str::to_owned).collect()
}

fn get<'a>(t: &'a Table, path: &[&str]) -> Option<&'a Value> {
    let (last, init) = path.split_last()?;
    let mut t = t;
    for k in init {
        t = t.get(*k)?.as_table()?;
    }
    t.get(*last)
}

/// A `[package]` key, set in the crate or inherited (`key.workspace = true`)
/// from `[workspace.package]`.
fn package_key<'a>(ws: &'a Table, pkg: &'a Table, key: &str) -> Option<&'a Value> {
    match pkg.get(key)? {
        Value::Table(t) if t.get("workspace").and_then(Value::as_bool) == Some(true) => {
            get(ws, &["workspace", "package", key])
        }
        v => Some(v),
    }
}

/// A dependency as published: its crate name and its version requirement,
/// with `dep.workspace = true` taken from `[workspace.dependencies]`.
fn dependency(ws: &Table, key: &str, spec: &Value) -> (String, Option<String>) {
    let table = spec.as_table();
    let inherited = table
        .and_then(|t| t.get("workspace"))
        .and_then(Value::as_bool)
        == Some(true);
    let spec = if inherited {
        get(ws, &["workspace", "dependencies", key]).expect("a workspace dependency")
    } else {
        spec
    };
    match spec {
        Value::String(v) => (key.to_owned(), Some(v.clone())),
        Value::Table(t) => (
            t.get("package")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_owned(),
            t.get("version").and_then(Value::as_str).map(str::to_owned),
        ),
        other => panic!("dependency {key}: {other:?}"),
    }
}

#[test]
fn publish_manifests() {
    let ws = read(&root().join("Cargo.toml"));
    let version = get(&ws, &["workspace", "package", "version"])
        .and_then(Value::as_str)
        .expect("[workspace.package] version");
    let order = publish_order();

    // Every workspace member but xtask is published, and the two forks.
    let mut want: BTreeSet<String> = ["vtmux-ghostty-vt-sys", "vtmux-ghostty-vt"]
        .map(str::to_owned)
        .into();
    for member in get(&ws, &["workspace", "members"])
        .and_then(Value::as_array)
        .expect("workspace members")
    {
        let m = read(&root().join(member.as_str().unwrap()).join("Cargo.toml"));
        let name = get(&m, &["package", "name"])
            .and_then(Value::as_str)
            .unwrap();
        if name == "xtask" {
            assert_eq!(
                get(&m, &["package", "publish"]),
                Some(&Value::Boolean(false))
            );
        } else {
            want.insert(name.to_owned());
        }
    }
    assert_eq!(
        order.iter().cloned().collect::<BTreeSet<_>>(),
        want,
        "publish.sh's crates"
    );
    assert_eq!(order.len(), want.len(), "publish.sh names a crate twice");

    for (i, name) in order.iter().enumerate() {
        check_crate(&ws, version, &order[..i], name);
    }
}

/// One published crate's manifest: its metadata, files and dependencies on
/// ours (`before`: the crates `publish.sh` publishes ahead of it).
fn check_crate(ws: &Table, version: &str, before: &[String], name: &str) {
    let m = read(&manifest(name));
    let pkg = m["package"].as_table().expect("[package]");
    assert_eq!(pkg["name"].as_str(), Some(name));
    assert_eq!(
        package_key(ws, pkg, "version").and_then(Value::as_str),
        Some(version),
        "{name}: version"
    );
    for key in [
        "description",
        "license",
        "repository",
        "include",
        "keywords",
        "categories",
    ] {
        assert!(package_key(ws, pkg, key).is_some(), "{name}: no {key}");
    }
    // Cargo skips an `include` entry that names nothing, so a missing
    // licence file ships as no licence file.
    let dir = manifest(name).parent().unwrap().to_path_buf();
    for entry in pkg["include"].as_array().expect("include is a list") {
        let entry = entry.as_str().unwrap().trim_start_matches('/');
        if !entry.contains('*') {
            assert!(
                dir.join(entry).is_file(),
                "{name}: include {entry} is no file"
            );
        }
    }
    assert_ne!(
        package_key(ws, pkg, "publish"),
        Some(&Value::Boolean(false)),
        "{name}: publish = false"
    );
    // Our crates are one program: each depends on the exact version of
    // the others, published before it.
    for kind in ["dependencies", "build-dependencies"] {
        for (key, spec) in m.get(kind).and_then(Value::as_table).into_iter().flatten() {
            let (dep, req) = dependency(ws, key, spec);
            if dep.starts_with("vtmux") {
                assert_eq!(req, Some(format!("={version}")), "{name}: {kind} {dep}");
                assert!(
                    before.contains(&dep),
                    "{name} publishes before its dependency {dep}"
                );
            }
        }
    }
    // A dev-dependency of ours with no version is dropped by `cargo
    // publish`; with one, it must already be on crates.io.
    for (key, spec) in m
        .get("dev-dependencies")
        .and_then(Value::as_table)
        .into_iter()
        .flatten()
    {
        let (dep, req) = dependency(ws, key, spec);
        if dep.starts_with("vtmux") {
            assert_eq!(req, None, "{name}: dev-dependency {dep} has a version");
        }
    }
}

/// A profile override for a package not in the graph (one renamed) is only
/// a cargo warning, and the build it was for silently changes.
#[test]
fn profile_overrides_name_packages() {
    let ws = read(&root().join("Cargo.toml"));
    let lock = read(&root().join("Cargo.lock"));
    let packages: BTreeSet<&str> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.get("name").and_then(Value::as_str))
        .collect();
    for (profile, p) in ws["profile"].as_table().unwrap() {
        for name in p
            .get("package")
            .and_then(Value::as_table)
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
        {
            assert!(
                packages.contains(name.as_str()),
                "[profile.{profile}.package.{name}]: no such package"
            );
        }
    }
}
