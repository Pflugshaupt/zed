//! Turns compiler output from a build into diagnostics for Zed's Problems panel.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use collections::{HashMap, HashSet};
use gpui::{App, Entity};
use language::DiagnosticSourceKind;
use lsp::{DiagnosticSeverity, LanguageServerId};
use project::{LspStore, lsp_store::DocumentDiagnosticsUpdate};
use util::{ResultExt as _, paths::normalize_path};

/// Diagnostics are stored per language server, so build issues are published
/// under an id of their own. It's far above the ids Zed hands out to real
/// servers, so restarting a server never clears or replaces build issues.
const BUILD_DIAGNOSTICS_SERVER_ID: LanguageServerId = LanguageServerId(usize::MAX / 2);

/// Shown as the source of each issue. Zed treats diagnostics from this source
/// as describing the files on disk rather than unsaved edits.
const DIAGNOSTIC_SOURCE: &str = "build";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IssueSeverity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BuildIssue {
    pub path: PathBuf,
    /// 1-based, as printed by the compiler.
    pub line: u32,
    /// 1-based, as printed by the compiler.
    pub column: Option<u32>,
    pub severity: IssueSeverity,
    pub message: String,
}

const SEVERITY_MARKERS: &[(&str, IssueSeverity)] = &[
    (": fatal error: ", IssueSeverity::Error),
    (": error: ", IssueSeverity::Error),
    (": warning: ", IssueSeverity::Warning),
];

/// Extracts GCC and Clang style issues (`path:line[:column]: error: message`)
/// from build output. Relative paths are resolved against the build
/// directory, which is where the compiler runs. Issues are deduplicated, since
/// a problem in a header is reported once for every file that includes it.
pub fn parse_build_output(output: &str, build_dir: &Path) -> Vec<BuildIssue> {
    let mut seen = HashSet::default();
    output
        .lines()
        .filter_map(|line| parse_issue(line.trim(), build_dir))
        .filter(|issue| seen.insert(issue.clone()))
        .collect()
}

fn parse_issue(line: &str, build_dir: &Path) -> Option<BuildIssue> {
    let (marker_index, marker, severity) = SEVERITY_MARKERS
        .iter()
        .filter_map(|(marker, severity)| Some((line.find(marker)?, *marker, *severity)))
        .min_by_key(|(marker_index, _, _)| *marker_index)?;
    let location = &line[..marker_index];
    let message = line[marker_index + marker.len()..].trim();

    let (before_last, last) = location.rsplit_once(':')?;
    let last = last.parse::<u32>().ok()?;
    let (path, line_number, column) = match before_last.rsplit_once(':') {
        Some((path, line_number)) if line_number.parse::<u32>().is_ok() => {
            (path, line_number.parse::<u32>().ok()?, Some(last))
        }
        _ => (before_last, last, None),
    };
    if path.is_empty() || message.is_empty() {
        return None;
    }

    Some(BuildIssue {
        path: normalize_path(&build_dir.join(path)),
        line: line_number,
        column,
        severity,
        message: message.to_string(),
    })
}

/// Replaces the build issues shown in the Problems panel and in editors, and
/// returns the files that now have issues. `previous_paths` are the files
/// returned by the last call: each update only replaces the diagnostics of
/// one file, so files whose issues went away are cleared with an empty update.
/// Files outside the project are skipped by Zed.
pub fn publish(
    lsp_store: &Entity<LspStore>,
    issues: &[BuildIssue],
    previous_paths: HashSet<PathBuf>,
    cx: &mut App,
) -> HashSet<PathBuf> {
    let mut diagnostics_by_path = diagnostics_by_path(issues);
    let new_paths: HashSet<PathBuf> = diagnostics_by_path.keys().cloned().collect();
    for path in previous_paths {
        diagnostics_by_path.entry(path).or_default();
    }

    let disk_based_sources = [DIAGNOSTIC_SOURCE.to_string()];
    let updates = diagnostics_by_path
        .into_iter()
        .filter_map(|(path, diagnostics)| {
            let Ok(uri) = lsp::Uri::from_file_path(&path) else {
                log::warn!("can't report build issues for {}", path.display());
                return None;
            };
            Some(DocumentDiagnosticsUpdate {
                diagnostics: lsp::PublishDiagnosticsParams {
                    uri,
                    diagnostics,
                    version: None,
                },
                result_id: None,
                registration_id: None,
                server_id: BUILD_DIAGNOSTICS_SERVER_ID,
                disk_based_sources: Cow::Borrowed(&disk_based_sources),
            })
        })
        .collect();

    lsp_store.update(cx, |lsp_store, cx| {
        lsp_store
            .merge_lsp_diagnostics(DiagnosticSourceKind::Pushed, updates, |_, _, _| false, cx)
            .log_err();
    });
    new_paths
}

/// Groups issues by file as LSP diagnostics, which is the form in which Zed
/// accepts diagnostics from outside its own language servers.
fn diagnostics_by_path(issues: &[BuildIssue]) -> HashMap<PathBuf, Vec<lsp::Diagnostic>> {
    let mut diagnostics_by_path: HashMap<PathBuf, Vec<lsp::Diagnostic>> = HashMap::default();
    for issue in issues {
        let line = issue.line.saturating_sub(1);
        let character = issue.column.unwrap_or(1).saturating_sub(1);
        let diagnostic = lsp::Diagnostic {
            range: lsp::Range::new(
                lsp::Position::new(line, character),
                lsp::Position::new(line, character + 1),
            ),
            severity: Some(match issue.severity {
                IssueSeverity::Error => DiagnosticSeverity::ERROR,
                IssueSeverity::Warning => DiagnosticSeverity::WARNING,
            }),
            source: Some(DIAGNOSTIC_SOURCE.to_string()),
            message: lsp::DiagnosticMessage::String(issue.message.clone()),
            ..lsp::Diagnostic::default()
        };
        diagnostics_by_path
            .entry(issue.path.clone())
            .or_default()
            .push(diagnostic);
    }
    diagnostics_by_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;

    #[gpui::test]
    async fn test_publish(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "src": {
                    "main.cpp": "int main() {\n  foo();\n}\n",
                    "util.h": "int x;\n",
                },
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
        let error_and_warning_counts = |cx: &mut TestAppContext| {
            project.read_with(cx, |project, cx| {
                let summary = project.diagnostic_summary(false, cx);
                (summary.error_count, summary.warning_count)
            })
        };

        let issues = parse_build_output(
            "/project/src/main.cpp:2:3: error: undeclared identifier 'foo'\n\
             /project/src/util.h:1:5: warning: unused variable 'x'\n\
             /elsewhere/system.h:1:1: error: outside of the project\n",
            Path::new("/project/build"),
        );
        let paths = cx.update(|cx| publish(&lsp_store, &issues, HashSet::default(), cx));
        cx.run_until_parked();
        assert_eq!(error_and_warning_counts(cx), (1, 1));

        // A later build that fixed main.cpp clears that file's issue.
        let issues = parse_build_output(
            "/project/src/util.h:1:5: warning: unused variable 'x'\n",
            Path::new("/project/build"),
        );
        let paths = cx.update(|cx| publish(&lsp_store, &issues, paths, cx));
        cx.run_until_parked();
        assert_eq!(error_and_warning_counts(cx), (0, 1));
        assert_eq!(
            paths,
            HashSet::from_iter([PathBuf::from("/project/src/util.h")])
        );

        cx.update(|cx| publish(&lsp_store, &[], paths, cx));
        cx.run_until_parked();
        assert_eq!(error_and_warning_counts(cx), (0, 0));
    }

    #[test]
    fn test_parse_build_output() {
        let output = "\
[1/3] Building CXX object CMakeFiles/app.dir/src/main.cpp.o
FAILED: CMakeFiles/app.dir/src/main.cpp.o
/project/src/main.cpp:12:5: error: use of undeclared identifier 'foo'
   12 |     foo();
      |     ^
/project/src/main.cpp:3:10: warning: unused variable 'x' [-Wunused-variable]
../src/util.h:7: warning: no column here
/project/src/main.cpp:3:10: warning: unused variable 'x' [-Wunused-variable]
/project/src/other.cpp:1:1: fatal error: 'missing.h' file not found
/project/src/main.cpp:20:1: note: candidate function not viable
ld: warning: ignoring duplicate libraries
ninja: build stopped: subcommand failed.
";
        assert_eq!(
            parse_build_output(output, Path::new("/project/build")),
            vec![
                BuildIssue {
                    path: PathBuf::from("/project/src/main.cpp"),
                    line: 12,
                    column: Some(5),
                    severity: IssueSeverity::Error,
                    message: "use of undeclared identifier 'foo'".to_string(),
                },
                BuildIssue {
                    path: PathBuf::from("/project/src/main.cpp"),
                    line: 3,
                    column: Some(10),
                    severity: IssueSeverity::Warning,
                    message: "unused variable 'x' [-Wunused-variable]".to_string(),
                },
                BuildIssue {
                    path: PathBuf::from("/project/src/util.h"),
                    line: 7,
                    column: None,
                    severity: IssueSeverity::Warning,
                    message: "no column here".to_string(),
                },
                BuildIssue {
                    path: PathBuf::from("/project/src/other.cpp"),
                    line: 1,
                    column: Some(1),
                    severity: IssueSeverity::Error,
                    message: "'missing.h' file not found".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_diagnostics_by_path() {
        let issues = parse_build_output(
            "/a.cpp:2:3: error: first\n/a.cpp:4:1: warning: second\n/b.cpp:1:1: error: third\n",
            Path::new("/build"),
        );
        let diagnostics = diagnostics_by_path(&issues);
        let a_diagnostics = &diagnostics[Path::new("/a.cpp")];
        assert_eq!(a_diagnostics.len(), 2);
        assert_eq!(a_diagnostics[0].range.start, lsp::Position::new(1, 2));
        assert_eq!(a_diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(a_diagnostics[1].severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(a_diagnostics[1].source.as_deref(), Some(DIAGNOSTIC_SOURCE));
        assert_eq!(diagnostics[Path::new("/b.cpp")].len(), 1);
    }
}
