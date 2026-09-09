use std::fs;
use std::path::PathBuf;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn cargo_and_plugin_versions_match() {
    let root = project_root();
    let cargo: toml::Value = fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let plugin: toml::Value = fs::read_to_string(root.join("herdr-plugin.toml"))
        .unwrap()
        .parse()
        .unwrap();

    assert_eq!(cargo["package"]["version"], plugin["version"]);
    assert_eq!(cargo["package"]["name"], plugin["name"]);
    assert_eq!(cargo["package"]["rust-version"].as_str(), Some("1.87"));
    assert_eq!(
        cargo["package"]["repository"].as_str(),
        Some("https://github.com/SpaceK33z/herdr-worktrees")
    );
    assert_eq!(plugin["id"].as_str(), Some("worktrees"));

    let version = plugin["version"].as_str().unwrap();
    let changelog = fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
    assert!(
        changelog.contains(&format!("## [{version}]")),
        "CHANGELOG.md must contain a section for {version}"
    );
}

#[test]
fn plugin_declares_release_metadata() {
    let root = project_root();
    let plugin: toml::Value = fs::read_to_string(root.join("herdr-plugin.toml"))
        .unwrap()
        .parse()
        .unwrap();

    for key in ["id", "name", "version", "min_herdr_version", "description"] {
        assert!(
            plugin.get(key).and_then(toml::Value::as_str).is_some(),
            "herdr-plugin.toml must define {key}"
        );
    }

    let platforms = plugin["platforms"].as_array().unwrap();
    assert!(platforms
        .iter()
        .any(|value| value.as_str() == Some("macos")));
    assert!(platforms
        .iter()
        .any(|value| value.as_str() == Some("linux")));
    assert!(root.join("README.md").is_file());
    assert!(root.join("LICENSE").is_file());
}

#[test]
fn release_script_checks_values_and_rejects_mismatches() {
    let root = project_root();
    let script = root.join("scripts/check-release-versions.sh");
    let version = env!("CARGO_PKG_VERSION");
    let check = |dir: &std::path::Path, tag: &str| {
        std::process::Command::new("sh")
            .arg(&script)
            .arg(tag)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(check(&root, &format!("v{version}")));
    assert!(!check(&root, "v999.0.0"));
    let fixture = std::env::temp_dir().join(format!("herdr-release-check-{}", std::process::id()));
    fs::create_dir_all(&fixture).unwrap();
    fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nversion = \"1.2.3\"\n",
    )
    .unwrap();
    fs::write(fixture.join("herdr-plugin.toml"), "version = \"1.2.3\"\n").unwrap();
    assert!(check(&fixture, "v1.2.3"));
    fs::write(fixture.join("herdr-plugin.toml"), "version = \"1.2.4\"\n").unwrap();
    assert!(!check(&fixture, "v1.2.3"));
    fs::write(fixture.join("herdr-plugin.toml"), "").unwrap();
    assert!(!check(&fixture, "v1.2.3"));
    fs::remove_dir_all(fixture).unwrap();
    let workflow = fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
    assert!(
        workflow
            .find("sh scripts/check-release-versions.sh")
            .unwrap()
            < workflow.find("googleapis/release-please-action").unwrap()
    );
}
