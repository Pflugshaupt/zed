//! Reads build targets through the CMake file API:
//! https://cmake.org/cmake/help/latest/manual/cmake-file-api.7.html

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use collections::HashSet;
use fs::Fs;
use futures::StreamExt as _;
use gpui::SharedString;
use serde_json::Value;

const CODEMODEL_QUERY_PATH: &str = ".cmake/api/v1/query/client-zed/codemodel-v2";
const REPLY_DIR: &str = ".cmake/api/v1/reply";

/// CMake only answers queries that exist when it configures, so this has to
/// happen before running configure.
pub async fn write_codemodel_query(fs: &dyn Fs, build_dir: &Path) -> Result<()> {
    let query_path = build_dir.join(CODEMODEL_QUERY_PATH);
    if let Some(query_dir) = query_path.parent() {
        fs.create_dir(query_dir).await?;
    }
    fs.write(&query_path, b"")
        .await
        .with_context(|| format!("failed to write {}", query_path.display()))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub name: SharedString,
    /// The program this target builds, if it's an executable.
    pub executable: Option<PathBuf>,
    /// The target's `FOLDER` property, with `/` separating nested folders.
    pub folder: Option<SharedString>,
}

/// Returns the buildable targets from the latest reply, sorted by name, or an
/// empty list if the build directory hasn't been configured with a query yet.
pub async fn read_targets(fs: &dyn Fs, build_dir: &Path) -> Result<Vec<Target>> {
    let reply_dir = build_dir.join(REPLY_DIR);
    if !fs.is_dir(&reply_dir).await {
        return Ok(Vec::new());
    }

    // The index file with the largest name is the current one.
    let mut latest_index_name: Option<String> = None;
    let mut entries = fs.read_dir(&reply_dir).await?;
    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let Some(file_name) = entry.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name.starts_with("index-")
            && file_name.ends_with(".json")
            && latest_index_name
                .as_deref()
                .is_none_or(|latest| file_name > latest)
        {
            latest_index_name = Some(file_name.to_string());
        }
    }
    let Some(index_name) = latest_index_name else {
        return Ok(Vec::new());
    };

    let index = load_json(fs, &reply_dir.join(index_name)).await?;
    let codemodel_file = index
        .get("objects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|object| {
            object.get("kind").and_then(Value::as_str) == Some("codemodel")
                && object
                    .get("version")
                    .and_then(|version| version.get("major"))
                    .and_then(Value::as_u64)
                    == Some(2)
        })
        .and_then(|object| object.get("jsonFile"))
        .and_then(Value::as_str);
    let Some(codemodel_file) = codemodel_file else {
        return Ok(Vec::new());
    };

    let codemodel = load_json(fs, &reply_dir.join(codemodel_file)).await?;

    // Multi-config generators report the same targets once per build type.
    // The first configuration is usually the one `cmake --build` builds by
    // default, so its entries win.
    let mut seen_names = HashSet::default();
    let mut targets = Vec::new();
    for (name, target_file) in codemodel_target_files(&codemodel) {
        if !seen_names.insert(name.clone()) {
            continue;
        }
        let target = load_json(fs, &reply_dir.join(target_file)).await?;
        targets.push(Target {
            name,
            executable: executable_path(&target, build_dir),
            folder: folder_name(&target),
        });
    }
    targets.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(targets)
}

async fn load_json(fs: &dyn Fs, path: &Path) -> Result<Value> {
    let content = fs.load(path).await?;
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn codemodel_target_files(codemodel: &Value) -> Vec<(SharedString, &str)> {
    codemodel
        .get("configurations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|configuration| configuration.get("targets").and_then(Value::as_array))
        .flatten()
        .filter_map(|target| {
            let name = target.get("name")?.as_str()?;
            let target_file = target.get("jsonFile")?.as_str()?;
            Some((SharedString::from(name.to_string()), target_file))
        })
        .collect()
}

/// CMake reports the folder even when the `USE_FOLDERS` global property is
/// off, and the file API has no way to tell whether it is.
fn folder_name(target: &Value) -> Option<SharedString> {
    let name = target.get("folder")?.get("name")?.as_str()?;
    Some(SharedString::from(name.to_string()))
}

/// Artifact paths are relative to the build directory unless they're outside
/// of it.
fn executable_path(target: &Value, build_dir: &Path) -> Option<PathBuf> {
    if target.get("type")?.as_str()? != "EXECUTABLE" {
        return None;
    }
    let artifact = target
        .get("artifacts")?
        .as_array()?
        .first()?
        .get("path")?
        .as_str()?;
    Some(build_dir.join(artifact))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    #[gpui::test]
    async fn test_read_targets(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let build_dir = Path::new("/build");

        assert!(
            read_targets(fs.as_ref(), build_dir)
                .await
                .unwrap()
                .is_empty()
        );

        write_codemodel_query(fs.as_ref(), build_dir).await.unwrap();
        assert!(fs.is_file(&build_dir.join(CODEMODEL_QUERY_PATH)).await);

        fs.insert_tree(
            "/build/.cmake/api/v1/reply",
            json!({
                "index-2026-01-01T00-00-00-0000.json": json!({
                    "objects": [{
                        "kind": "codemodel",
                        "version": { "major": 2, "minor": 0 },
                        "jsonFile": "codemodel-old.json"
                    }]
                })
                .to_string(),
                "index-2026-02-01T00-00-00-0000.json": json!({
                    "objects": [{
                        "kind": "codemodel",
                        "version": { "major": 2, "minor": 10 },
                        "jsonFile": "codemodel-new.json"
                    }]
                })
                .to_string(),
                "codemodel-old.json": json!({
                    "configurations": [{
                        "targets": [{ "name": "stale", "jsonFile": "target-stale.json" }]
                    }]
                })
                .to_string(),
                "codemodel-new.json": json!({
                    "configurations": [
                        {
                            "name": "Debug",
                            "targets": [
                                { "name": "util", "jsonFile": "target-util-debug.json" },
                                { "name": "app", "jsonFile": "target-app-debug.json" }
                            ]
                        },
                        {
                            "name": "Release",
                            "targets": [
                                { "name": "app", "jsonFile": "target-app-release.json" },
                                { "name": "util", "jsonFile": "target-util-release.json" }
                            ]
                        }
                    ]
                })
                .to_string(),
                "target-app-debug.json": json!({
                    "name": "app",
                    "type": "EXECUTABLE",
                    "folder": { "name": "Apps/Tools" },
                    "artifacts": [{ "path": "Debug/app" }]
                })
                .to_string(),
                "target-util-debug.json": json!({
                    "name": "util",
                    "type": "STATIC_LIBRARY",
                    "artifacts": [{ "path": "Debug/libutil.a" }]
                })
                .to_string(),
            }),
        )
        .await;

        assert_eq!(
            read_targets(fs.as_ref(), build_dir).await.unwrap(),
            vec![
                Target {
                    name: "app".into(),
                    executable: Some(PathBuf::from("/build/Debug/app")),
                    folder: Some("Apps/Tools".into()),
                },
                Target {
                    name: "util".into(),
                    executable: None,
                    folder: None,
                },
            ]
        );
    }
}
