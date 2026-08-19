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
