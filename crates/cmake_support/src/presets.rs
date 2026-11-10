use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use fs::Fs;
use gpui::SharedString;
use serde_json::{Map, Value};
use util::command::new_command;

pub const PRESETS_FILE_NAMES: &[&str] = &["CMakePresets.json", "CMakeUserPresets.json"];

const MAX_RECURSION_DEPTH: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigurePreset {
    pub name: SharedString,
    pub display_name: Option<SharedString>,
}

impl ConfigurePreset {
    pub fn label(&self) -> SharedString {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.name.clone())
    }
}

/// CMake resolves preset inheritance, includes, conditions, and hidden presets
/// itself, so we ask it instead of parsing the presets files.
pub async fn list_configure_presets(
    cmake_program: &str,
    source_dir: &Path,
) -> Result<Vec<ConfigurePreset>> {
    let output = new_command(cmake_program)
        .arg("--list-presets=configure")
        .current_dir(source_dir)
        .output()
        .await
        .with_context(|| format!("failed to run {cmake_program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(parse_configure_presets(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Parses the output of `cmake --list-presets`, which looks like:
///
/// ```text
/// Available configure presets:
///
///   "debug"   - Debug
///   "release"
/// ```
fn parse_configure_presets(output: &str) -> Vec<ConfigurePreset> {
    output
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().strip_prefix('"')?.split_once('"')?;
            let display_name = rest
                .trim()
                .strip_prefix('-')
                .map(str::trim)
                .filter(|display_name| !display_name.is_empty());
            Some(ConfigurePreset {
                name: name.to_string().into(),
                display_name: display_name.map(|display_name| display_name.to_string().into()),
            })
        })
        .collect()
}

/// Resolves the build directory of a configure preset.
///
/// CMake has no command that prints this, but it's needed up front to place a
/// file API query in the build directory before configuring, so this mirrors
/// CMake's preset resolution for the `binaryDir` field. Returns `None` when no
/// preset in the inheritance chain sets `binaryDir`.
pub async fn resolve_binary_dir(
    fs: &dyn Fs,
    source_dir: &Path,
    preset_name: &str,
) -> Result<Option<PathBuf>> {
    let presets = load_configure_presets(fs, source_dir).await?;
    let Some((binary_dir, file_dir)) =
        find_in_inheritance_chain(&presets, preset_name, 0, &|preset| {
            preset.fields.get("binaryDir")
        })?
    else {
        return Ok(None);
    };
    let binary_dir = binary_dir.as_str().context("binaryDir must be a string")?;
    let context = MacroContext {
        presets: &presets,
        source_dir,
        preset_name: Some(preset_name),
        file_dir,
    };
    let binary_dir = PathBuf::from(expand_macros(binary_dir, &context, 0)?);
    Ok(Some(if binary_dir.is_relative() {
        source_dir.join(binary_dir)
    } else {
        binary_dir
    }))
}

struct PresetDefinition {
    fields: Map<String, Value>,
    file_dir: PathBuf,
}

async fn load_configure_presets(
    fs: &dyn Fs,
    source_dir: &Path,
) -> Result<HashMap<String, PresetDefinition>> {
    let mut presets = HashMap::default();
    let mut loaded_files = HashSet::default();
    // The root presets files are optional, but included files must exist.
    let mut pending_files: Vec<(PathBuf, bool)> = PRESETS_FILE_NAMES
        .iter()
        .map(|file_name| (source_dir.join(file_name), false))
        .collect();

    while let Some((path, required)) = pending_files.pop() {
        if !loaded_files.insert(path.clone()) {
            continue;
        }
        let content = match fs.load(&path).await {
            Ok(content) => content,
            Err(_) if !required => continue,
            Err(error) => {
                return Err(error.context(format!("failed to read {}", path.display())));
            }
        };
        let document: Value = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let file_dir = path.parent().unwrap_or(source_dir).to_path_buf();

        if let Some(includes) = document.get("include").and_then(Value::as_array) {
            let context = MacroContext {
                presets: &presets,
                source_dir,
                preset_name: None,
                file_dir: &file_dir,
            };
            for include in includes.iter().filter_map(Value::as_str) {
                let include = PathBuf::from(expand_macros(include, &context, 0)?);
                pending_files.push((file_dir.join(include), true));
            }
        }

        let configure_presets = document
            .get("configurePresets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for preset in configure_presets {
            if let Some(fields) = preset.as_object()
                && let Some(name) = fields.get("name").and_then(Value::as_str)
            {
                presets
                    .entry(name.to_string())
                    .or_insert_with(|| PresetDefinition {
                        fields: fields.clone(),
                        file_dir: file_dir.clone(),
                    });
            }
        }
    }

    Ok(presets)
}

/// Looks a value up on a preset and then on its parents, depth first in
/// `inherits` order, which is how CMake gives earlier parents precedence.
fn find_in_inheritance_chain<'a>(
    presets: &'a HashMap<String, PresetDefinition>,
    preset_name: &str,
    depth: usize,
    lookup: &dyn Fn(&'a PresetDefinition) -> Option<&'a Value>,
) -> Result<Option<(&'a Value, &'a Path)>> {
    if depth > MAX_RECURSION_DEPTH {
        bail!("preset inheritance of {preset_name:?} is too deep or cyclic");
    }
    let preset = presets
        .get(preset_name)
        .with_context(|| format!("unknown configure preset {preset_name:?}"))?;
    if let Some(value) = lookup(preset) {
        return Ok(Some((value, &preset.file_dir)));
    }
    let parents: Vec<&str> = match preset.fields.get("inherits") {
        Some(Value::String(parent)) => vec![parent.as_str()],
        Some(Value::Array(parents)) => parents.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for parent in parents {
        if let Some(found) = find_in_inheritance_chain(presets, parent, depth + 1, lookup)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

struct MacroContext<'a> {
    presets: &'a HashMap<String, PresetDefinition>,
    source_dir: &'a Path,
    /// `None` when expanding `include` paths, where preset-specific macros
    /// are not available.
    preset_name: Option<&'a str>,
    /// Directory of the presets file that contains the value being expanded.
    file_dir: &'a Path,
}

fn expand_macros(input: &str, context: &MacroContext, depth: usize) -> Result<String> {
    if depth > MAX_RECURSION_DEPTH {
        bail!("macro expansion of {input:?} is too deep or cyclic");
    }
    let mut output = String::new();
    let mut rest = input;
    while let Some(dollar_index) = rest.find('$') {
        output.push_str(&rest[..dollar_index]);
        let after_dollar = &rest[dollar_index + 1..];
        let namespace_end = after_dollar
            .find(|character: char| !character.is_ascii_alphabetic())
            .unwrap_or(after_dollar.len());
        let namespace = &after_dollar[..namespace_end];
        let is_macro = after_dollar[namespace_end..].starts_with('{')
            && matches!(namespace, "" | "env" | "penv" | "vendor");
        if !is_macro {
            // CMake treats a `$` that doesn't start a macro as a literal.
            output.push('$');
            rest = after_dollar;
            continue;
        }
        let body = &after_dollar[namespace_end + 1..];
        let closing_brace = body
            .find('}')
            .with_context(|| format!("unterminated macro in {input:?}"))?;
        let macro_name = &body[..closing_brace];
        output.push_str(&expand_macro(namespace, macro_name, context, depth)?);
        rest = &body[closing_brace + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn expand_macro(
    namespace: &str,
    macro_name: &str,
    context: &MacroContext,
    depth: usize,
) -> Result<String> {
    match namespace {
        "" => expand_builtin_macro(macro_name, context),
        "env" => {
            let Some(preset_name) = context.preset_name else {
                bail!("$env{{{macro_name}}} is not supported here");
            };
            let variable = find_in_inheritance_chain(context.presets, preset_name, 0, &|preset| {
                preset
                    .fields
                    .get("environment")
                    .and_then(|environment| environment.get(macro_name))
            })?;
            match variable {
                Some((Value::String(value), file_dir)) => {
                    let context = MacroContext {
                        file_dir,
                        ..*context
                    };
                    expand_macros(value, &context, depth + 1)
                }
                // A `null` value unsets the variable for this preset.
                Some(_) => Ok(String::new()),
                None => Ok(std::env::var(macro_name).unwrap_or_default()),
            }
        }
        "penv" => Ok(std::env::var(macro_name).unwrap_or_default()),
        _ => bail!("${namespace}{{{macro_name}}} macros are not supported"),
    }
}

fn expand_builtin_macro(macro_name: &str, context: &MacroContext) -> Result<String> {
    let path_to_string = |path: &Path| path.to_string_lossy().into_owned();
    Ok(match macro_name {
        "sourceDir" => path_to_string(context.source_dir),
        "sourceParentDir" => context
            .source_dir
            .parent()
            .map(path_to_string)
            .unwrap_or_default(),
        "sourceDirName" => context
            .source_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "fileDir" => path_to_string(context.file_dir),
        "hostSystemName" => host_system_name().to_string(),
        "dollar" => "$".to_string(),
        "pathListSep" => if cfg!(windows) { ";" } else { ":" }.to_string(),
        "presetName" => context
            .preset_name
            .context("${presetName} is not supported here")?
            .to_string(),
        "generator" => {
            let preset_name = context
                .preset_name
                .context("${generator} is not supported here")?;
            find_in_inheritance_chain(context.presets, preset_name, 0, &|preset| {
                preset.fields.get("generator")
            })?
            .and_then(|(generator, _)| generator.as_str())
            .unwrap_or_default()
            .to_string()
        }
        _ => bail!("unknown macro ${{{macro_name}}}"),
    })
}

/// The value of `CMAKE_HOST_SYSTEM_NAME` for the current host.
fn host_system_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    #[test]
    fn test_parse_configure_presets() {
        let output = "Available configure presets:\n\
                      \n  \"my-local\" - My local debug\
                      \n  \"debug\"    - Debug (Ninja)\
                      \n  \"release\"\n";
        assert_eq!(
            parse_configure_presets(output),
            vec![
                ConfigurePreset {
                    name: "my-local".into(),
                    display_name: Some("My local debug".into()),
                },
                ConfigurePreset {
                    name: "debug".into(),
                    display_name: Some("Debug (Ninja)".into()),
                },
                ConfigurePreset {
                    name: "release".into(),
                    display_name: None,
                },
            ]
        );
    }

    #[gpui::test]
    async fn test_resolve_binary_dir(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "CMakePresets.json": json!({
                    "version": 6,
                    "include": ["cmake/base.json"],
                    "configurePresets": [
                        {
                            "name": "ninja",
                            "inherits": "base",
                            "generator": "Ninja",
                            "environment": { "BUILD_ROOT": "${sourceDir}/out" }
                        },
                        {
                            "name": "debug",
                            "inherits": ["ninja", "other"]
                        },
                        {
                            "name": "from-env",
                            "inherits": "ninja",
                            "binaryDir": "$env{BUILD_ROOT}/${presetName}"
                        },
                        { "name": "relative", "binaryDir": "build/${dollar}x" },
                        { "name": "no-binary-dir" }
                    ]
                })
                .to_string(),
                "CMakeUserPresets.json": json!({
                    "version": 6,
                    "configurePresets": [{ "name": "mine", "inherits": "debug" }]
                })
                .to_string(),
                "cmake": {
                    "base.json": json!({
                        "version": 6,
                        "configurePresets": [
                            {
                                "name": "base",
                                "hidden": true,
                                "binaryDir": "${fileDir}/../build/${presetName}-${generator}"
                            },
                            { "name": "other", "binaryDir": "/elsewhere" }
                        ]
                    })
                    .to_string(),
                },
            }),
        )
        .await;

        let resolve = async |preset_name: &str| {
            resolve_binary_dir(fs.as_ref(), Path::new("/project"), preset_name)
                .await
                .unwrap()
        };

        assert_eq!(
            resolve("debug").await,
            Some(PathBuf::from("/project/cmake/../build/debug-Ninja"))
        );
        assert_eq!(
            resolve("mine").await,
            Some(PathBuf::from("/project/cmake/../build/mine-Ninja"))
        );
        assert_eq!(
            resolve("from-env").await,
            Some(PathBuf::from("/project/out/from-env"))
        );
        assert_eq!(
            resolve("relative").await,
            Some(PathBuf::from("/project/build/$x"))
        );
        assert_eq!(resolve("no-binary-dir").await, None);
        assert!(
            resolve_binary_dir(fs.as_ref(), Path::new("/project"), "missing")
                .await
                .is_err()
        );
    }
}
