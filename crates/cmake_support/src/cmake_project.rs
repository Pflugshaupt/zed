use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{AsyncWindowContext, Entity, Subscription, Task, WeakEntity};
use lsp::{LanguageServerName, LanguageServerSelector};
use project::{PathChange, Project};
use serde::{Deserialize, Serialize};
use serde_json::json;
use settings::SettingsStore;
use task::{DebugScenario, RevealStrategy, SharedTaskContext, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::terminal_panel::TerminalPanel;
use ui::prelude::*;
use util::{ResultExt as _, TryFutureExt as _, rel_path::RelPath, shell::ShellKind};
use workspace::Workspace;

use crate::{
    CMAKE_PANEL_KEY,
    build_issues::{self, BuildIssue},
    file_api::{self, Target},
    presets::{self, ConfigurePreset, PRESETS_FILE_NAMES},
};

pub(crate) const ALL_TARGETS_LABEL: &str = "All Targets";

/// The `"cmake"` key of the user's settings file. It's read straight from the
/// file so that it doesn't have to be registered in Zed's settings schema,
/// which lives outside this crate.
#[derive(Default, Deserialize)]
struct UserSettingsFile {
    cmake: Option<CMakeSettings>,
}

#[derive(Default, Deserialize)]
struct CMakeSettings {
    /// Path to the cmake executable. When unset, `cmake` is looked up on `PATH`.
    path: Option<String>,
    /// Whether the target menu groups targets into submenus by their `FOLDER`
    /// property. Defaults to true.
    use_folders: Option<bool>,
    /// Whether clangd restarts when a `.clangd` file in the project changes.
    /// Defaults to true.
    restart_clangd_on_config_change: Option<bool>,
}

async fn load_cmake_settings(fs: &dyn Fs) -> Result<CMakeSettings> {
    let settings_path = paths::settings_file();
    if !fs.is_file(settings_path).await {
        return Ok(CMakeSettings::default());
    }
    let content = fs.load(settings_path).await?;
    if content.trim().is_empty() {
        return Ok(CMakeSettings::default());
    }
    let settings: UserSettingsFile = settings::parse_json_with_comments(&content)
        .with_context(|| format!("failed to parse {}", settings_path.display()))?;
    Ok(settings.cmake.unwrap_or_default())
}

/// The key under which a workspace's CMake state is saved, like the built-in
/// panels use.
pub(crate) fn serialization_key(workspace: &Workspace) -> Option<String> {
    workspace
        .database_id()
        .map(|id| i64::from(id).to_string())
        .or(workspace.session_id())
        .map(|id| format!("{CMAKE_PANEL_KEY}-{id:?}"))
}

/// The selections that are restored when the workspace is reopened.
#[derive(Default, Serialize, Deserialize)]
struct SerializedCMakeProject {
    configure_preset: Option<String>,
    /// `None` when all targets are selected.
    target: Option<String>,
    /// The targets last selected in the other configure presets.
    #[serde(default)]
    preset_targets: HashMap<String, Option<String>>,
    action: Option<BuildAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TargetSelection {
    /// Runs `cmake --build` without `--target`, which builds the project's
    /// default target: `all`, or `ALL_BUILD` for Visual Studio and Xcode.
    All,
    Named(SharedString),
}

impl TargetSelection {
    fn from_serialized(target: Option<String>) -> Self {
        target.map_or(Self::All, |target| Self::Named(target.into()))
    }

    fn to_serialized(&self) -> Option<String> {
        match self {
            Self::All => None,
            Self::Named(target) => Some(target.to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BuildAction {
    Build,
    Run,
    Debug,
}

impl BuildAction {
    pub(crate) const ALL: [Self; 3] = [Self::Build, Self::Run, Self::Debug];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::Run => "Run",
            Self::Debug => "Debug",
        }
    }

    pub(crate) fn tooltip(self) -> &'static str {
        match self {
            Self::Build => "Build the selected target",
            Self::Run => "Build and run the selected target",
            Self::Debug => "Build and debug the selected target",
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::Build => IconName::ToolHammer,
            Self::Run => IconName::PlayFilled,
            Self::Debug => IconName::Debug,
        }
    }
}

/// A workspace's CMake state: its presets and targets, the selections, and
/// the running configure, build or program. The panel and the title bar both
/// show and change it.
pub(crate) struct CMakeProject {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    serialization_key: Option<String>,
    settings_loaded: bool,
    /// Whether the saved selections have been restored. Presets are listed
    /// only after that, so that the saved preset is the one picked.
    state_restored: bool,
    cmake_path: Option<String>,
    use_folders: bool,
    restart_clangd_on_config_change: bool,
    /// Whether the project has a presets file that the panel can use.
    has_presets_file: bool,
    configure_presets: Vec<ConfigurePreset>,
    selected_configure_preset: Option<SharedString>,
    configure_presets_error: Option<SharedString>,
    targets: Vec<Target>,
    selected_target: TargetSelection,
    /// The targets last selected in the configure presets other than the
    /// selected one, so that switching back to a preset restores its target.
    preset_targets: HashMap<SharedString, TargetSelection>,
    targets_error: Option<SharedString>,
    is_configuring: bool,
    action: BuildAction,
    running_action: Option<BuildAction>,
    action_error: Option<SharedString>,
    /// The terminal of the running build, which Stop kills.
    build_terminal: Option<WeakEntity<Terminal>>,
    /// Set when Stop kills a build, so that Run and Debug don't continue.
    build_stopped: bool,
    /// The terminal of the program started by Run while it's running, which
    /// Stop kills when no build is running.
    program_terminal: Option<WeakEntity<Terminal>>,
    /// Files that currently have build issues in the Problems panel.
    build_issue_paths: HashSet<PathBuf>,
    load_settings_task: Task<()>,
    restore_state_task: Task<()>,
    serialize_task: Task<Option<()>>,
    refresh_configure_presets_task: Task<()>,
    refresh_targets_task: Task<()>,
    configure_task: Task<()>,
    action_task: Task<()>,
    program_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl CMakeProject {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        serialization_key: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&project, |this, _project, event, cx| match event {
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                    this.refresh_configure_presets(cx);
                }
                project::Event::WorktreeUpdatedEntries(_, entries) => {
                    if entries.iter().any(|(path, _, _)| is_presets_file(path)) {
                        this.refresh_configure_presets(cx);
                    }
                    // `Loaded` entries come from the initial scan, not from a change.
                    if this.restart_clangd_on_config_change
                        && entries.iter().any(|(path, _, change)| {
                            *change != PathChange::Loaded && is_clangd_config_file(path)
                        })
                    {
                        this.restart_clangd(cx);
                    }
                }
                _ => {}
            }),
            // The user's settings file may have changed, and with it the cmake path.
            cx.observe_global::<SettingsStore>(|this, cx| this.load_settings(cx)),
        ];

        let fs = project.read(cx).fs().clone();
        let mut this = Self {
            workspace,
            project,
            fs,
            serialization_key: serialization_key.clone(),
            settings_loaded: false,
            state_restored: false,
            cmake_path: None,
            use_folders: true,
            restart_clangd_on_config_change: true,
            has_presets_file: false,
            configure_presets: Vec::new(),
            selected_configure_preset: None,
            configure_presets_error: None,
            targets: Vec::new(),
            selected_target: TargetSelection::All,
            preset_targets: HashMap::default(),
            targets_error: None,
            is_configuring: false,
            action: BuildAction::Build,
            running_action: None,
            action_error: None,
            build_terminal: None,
            build_stopped: false,
            program_terminal: None,
            build_issue_paths: HashSet::default(),
            load_settings_task: Task::ready(()),
            restore_state_task: Task::ready(()),
            serialize_task: Task::ready(None),
            refresh_configure_presets_task: Task::ready(()),
            refresh_targets_task: Task::ready(()),
            configure_task: Task::ready(()),
            action_task: Task::ready(()),
            program_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.load_settings(cx);
        this.restore_state(serialization_key, cx);
        this
    }

    pub(crate) fn has_presets_file(&self) -> bool {
        self.has_presets_file
    }

    pub(crate) fn configure_presets(&self) -> &[ConfigurePreset] {
        &self.configure_presets
    }

    pub(crate) fn selected_configure_preset(&self) -> Option<&SharedString> {
        self.selected_configure_preset.as_ref()
    }

    pub(crate) fn configure_presets_error(&self) -> Option<&SharedString> {
        self.configure_presets_error.as_ref()
    }

    pub(crate) fn targets(&self) -> &[Target] {
        &self.targets
    }

    pub(crate) fn target_selection(&self) -> &TargetSelection {
        &self.selected_target
    }

    pub(crate) fn targets_error(&self) -> Option<&SharedString> {
        self.targets_error.as_ref()
    }

    pub(crate) fn use_folders(&self) -> bool {
        self.use_folders
    }

    pub(crate) fn is_configuring(&self) -> bool {
        self.is_configuring
    }

    pub(crate) fn action(&self) -> BuildAction {
        self.action
    }

    pub(crate) fn running_action(&self) -> Option<BuildAction> {
        self.running_action
    }

    pub(crate) fn action_error(&self) -> Option<&SharedString> {
        self.action_error.as_ref()
    }

    fn restore_state(&mut self, serialization_key: Option<String>, cx: &mut Context<Self>) {
        let Some(serialization_key) = serialization_key else {
            self.apply_restored_state(SerializedCMakeProject::default(), cx);
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.restore_state_task = cx.spawn(async move |this, cx| {
            let state = cx
                .background_spawn(async move { kvp.read_kvp(&serialization_key) })
                .await
                .context("failed to load the CMake panel state")
                .log_err()
                .flatten()
                .and_then(|state| serde_json::from_str::<SerializedCMakeProject>(&state).log_err())
                .unwrap_or_default();
            this.update(cx, |this, cx| this.apply_restored_state(state, cx))
                .ok();
        });
    }

    fn apply_restored_state(&mut self, state: SerializedCMakeProject, cx: &mut Context<Self>) {
        self.selected_configure_preset = state.configure_preset.map(SharedString::from);
        self.selected_target = TargetSelection::from_serialized(state.target);
        self.preset_targets = state
            .preset_targets
            .into_iter()
            .map(|(preset, target)| (preset.into(), TargetSelection::from_serialized(target)))
            .collect();
        self.action = state.action.unwrap_or(BuildAction::Build);
        self.state_restored = true;
        self.refresh_configure_presets(cx);
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self.serialization_key.clone() else {
            return;
        };
        let state = SerializedCMakeProject {
            configure_preset: self
                .selected_configure_preset
                .as_ref()
                .map(|preset| preset.to_string()),
            target: self.selected_target.to_serialized(),
            preset_targets: self
                .preset_targets
                .iter()
                .map(|(preset, target)| (preset.to_string(), target.to_serialized()))
                .collect(),
            action: Some(self.action),
        };
        let kvp = KeyValueStore::global(cx);
        self.serialize_task = cx.background_spawn(
            async move {
                kvp.write_kvp(serialization_key, serde_json::to_string(&state)?)
                    .await
            }
            .log_err(),
        );
    }

    fn load_settings(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        self.load_settings_task = cx.spawn(async move |this, cx| {
            let settings = load_cmake_settings(fs.as_ref()).await;
            this.update(cx, |this, cx| {
                let cmake_path = match settings {
                    Ok(settings) => {
                        this.use_folders = settings.use_folders.unwrap_or(true);
                        this.restart_clangd_on_config_change =
                            settings.restart_clangd_on_config_change.unwrap_or(true);
                        cx.notify();
                        settings.path
                    }
                    // Zed reports invalid settings files itself; keep the last good values.
                    Err(error) => {
                        log::error!("failed to load cmake settings: {error:#}");
                        this.cmake_path.clone()
                    }
                };
                if !this.settings_loaded || cmake_path != this.cmake_path {
                    this.settings_loaded = true;
                    this.cmake_path = cmake_path;
                    this.refresh_configure_presets(cx);
                }
            })
            .log_err();
        });
    }

    fn cmake_program(&self) -> String {
        self.cmake_path
            .clone()
            .unwrap_or_else(|| "cmake".to_string())
    }

    /// The first visible worktree whose root contains a presets file.
    fn presets_source_dir(&self, cx: &App) -> Option<PathBuf> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .find_map(|worktree| {
                let worktree = worktree.read(cx);
                let root_dir = worktree.root_dir()?;
                worktree
                    .child_entries(RelPath::empty())
                    .any(|entry| is_presets_file(&entry.path))
                    .then(|| root_dir.to_path_buf())
            })
    }

    fn refresh_configure_presets(&mut self, cx: &mut Context<Self>) {
        // Presets are listed once settings are loaded, so that a configured
        // cmake path is used from the start.
        if !self.settings_loaded || !self.state_restored {
            return;
        }

        // Running the local cmake against a remote project's paths would be wrong.
        if !self.project.read(cx).is_local() {
            self.has_presets_file = false;
            self.refresh_configure_presets_task = Task::ready(());
            self.set_configure_presets(
                Err(anyhow!(
                    "CMake presets are only supported for local projects"
                )),
                cx,
            );
            return;
        }

        let Some(source_dir) = self.presets_source_dir(cx) else {
            self.has_presets_file = false;
            self.refresh_configure_presets_task = Task::ready(());
            self.set_configure_presets(Ok(Vec::new()), cx);
            return;
        };
        self.has_presets_file = true;

        let cmake_program = self.cmake_program();
        self.refresh_configure_presets_task = cx.spawn(async move |this, cx| {
            let presets = cx
                .background_spawn(async move {
                    presets::list_configure_presets(&cmake_program, &source_dir).await
                })
                .await;
            this.update(cx, |this, cx| this.set_configure_presets(presets, cx))
                .ok();
        });
    }

    fn set_configure_presets(
        &mut self,
        presets: Result<Vec<ConfigurePreset>>,
        cx: &mut Context<Self>,
    ) {
        match presets {
            Ok(presets) => {
                self.configure_presets = presets;
                self.configure_presets_error = None;
                let selection_is_valid =
                    self.selected_configure_preset
                        .as_ref()
                        .is_some_and(|selected| {
                            self.configure_presets
                                .iter()
                                .any(|preset| &preset.name == selected)
                        });
                if !selection_is_valid {
                    self.selected_configure_preset = self
                        .configure_presets
                        .first()
                        .map(|preset| preset.name.clone());
                }
            }
            // The selection is kept so that it survives the presets file being
            // temporarily invalid while the user edits it.
            Err(error) => {
                self.configure_presets = Vec::new();
                self.configure_presets_error = Some(format!("{error:#}").into());
            }
        }
        self.refresh_targets(cx);
        cx.notify();
    }

    pub(crate) fn select_configure_preset(
        &mut self,
        preset: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_configure_preset.as_ref() != Some(&preset) {
            // A preset that hasn't had a target selected yet keeps the current
            // one, which is usually what's wanted when switching between build
            // types.
            let target = self
                .preset_targets
                .remove(&preset)
                .unwrap_or_else(|| self.selected_target.clone());
            let previous_target = std::mem::replace(&mut self.selected_target, target);
            if let Some(previous_preset) = self.selected_configure_preset.take() {
                self.preset_targets.insert(previous_preset, previous_target);
            }
            self.action_error = None;
        }
        self.selected_configure_preset = Some(preset);
        self.serialize(cx);
        cx.notify();
        self.configure(window, cx);
    }

    /// Runs `cmake --preset` for the selected preset in a terminal and reloads
    /// the targets once it's done.
    fn configure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(source_dir), Some(preset)) = (
            self.presets_source_dir(cx),
            self.selected_configure_preset.clone(),
        ) else {
            return;
        };
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        let spawn_in_terminal = match configure_task(
            Shell::System.shell_kind(cfg!(windows)),
            &self.cmake_program(),
            &preset,
            source_dir.clone(),
        ) {
            Ok(spawn_in_terminal) => spawn_in_terminal,
            Err(error) => {
                self.configure_presets_error = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };

        self.is_configuring = true;
        cx.notify();
        self.configure_task = cx.spawn_in(window, async move |this, cx| {
            let query_result = cx
                .background_spawn(async move {
                    if let Some(build_dir) =
                        presets::resolve_binary_dir(fs.as_ref(), &source_dir, &preset).await?
                    {
                        file_api::write_codemodel_query(fs.as_ref(), &build_dir).await?;
                    }
                    anyhow::Ok(())
                })
                .await;
            // Without the query only the target list is affected, and refreshing
            // the targets below reports why, so configure regardless.
            query_result.log_err();

            let terminal_task = workspace.update_in(cx, |workspace, window, cx| {
                workspace.spawn_in_terminal(spawn_in_terminal, window, cx)
            });
            match terminal_task {
                Ok(terminal_task) => {
                    if let Some(Err(error)) = terminal_task.await {
                        log::error!("cmake configure failed to run: {error:#}");
                    }
                }
                Err(error) => log::error!("failed to spawn cmake configure: {error:#}"),
            }

            this.update(cx, |this, cx| {
                this.is_configuring = false;
                this.refresh_targets(cx);
            })
            .ok();
        });
    }

    /// clangd re-reads `.clangd` when it reparses a file, but open files and
    /// the background index keep their old compilation database, which
    /// matters when `.clangd` points at another build directory.
    fn restart_clangd(&self, cx: &mut Context<Self>) {
        let clangd = LanguageServerName::new_static("clangd");
        let project = self.project.read(cx);
        let lsp_store = project.lsp_store();
        let clangd_is_running = lsp_store
            .read(cx)
            .language_server_statuses()
            .any(|(_, status)| status.name == clangd);
        if !clangd_is_running {
            return;
        }
        let buffers = project.buffer_store().read(cx).buffers().collect();
        lsp_store.update(cx, |lsp_store, cx| {
            lsp_store.restart_language_servers_for_buffers(
                buffers,
                HashSet::from_iter([LanguageServerSelector::Name(clangd)]),
                cx,
            )
        });
    }

    fn refresh_targets(&mut self, cx: &mut Context<Self>) {
        let (Some(source_dir), Some(preset)) = (
            self.presets_source_dir(cx),
            self.selected_configure_preset.clone(),
        ) else {
            self.refresh_targets_task = Task::ready(());
            self.set_targets(Ok(Vec::new()), cx);
            return;
        };

        let fs = self.fs.clone();
        self.refresh_targets_task = cx.spawn(async move |this, cx| {
            let targets = cx
                .background_spawn(async move {
                    let build_dir =
                        presets::resolve_binary_dir(fs.as_ref(), &source_dir, &preset)
                            .await?
                            .with_context(|| {
                                format!("Preset {preset:?} doesn't set binaryDir, so its targets can't be found")
                            })?;
                    file_api::read_targets(fs.as_ref(), &build_dir).await
                })
                .await;
            this.update(cx, |this, cx| this.set_targets(targets, cx))
                .ok();
        });
    }

    fn set_targets(&mut self, targets: Result<Vec<Target>>, cx: &mut Context<Self>) {
        match targets {
            Ok(targets) => {
                self.targets = targets;
                self.targets_error = None;
            }
            Err(error) => {
                self.targets = Vec::new();
                self.targets_error = Some(format!("{error:#}").into());
            }
        }
        // Like CMake Tools, a selected target that no longer exists falls back
        // to all targets. It's kept while there are no targets, so that a
        // restored selection survives until the build directory is configured.
        if !self.targets.is_empty() && self.selected_target().is_none() {
            self.selected_target = TargetSelection::All;
        }
        cx.notify();
    }

    pub(crate) fn select_target(&mut self, target: TargetSelection, cx: &mut Context<Self>) {
        self.selected_target = target;
        self.action_error = None;
        self.serialize(cx);
        cx.notify();
    }

    /// The selected target, unless all targets are selected.
    fn selected_target(&self) -> Option<&Target> {
        let TargetSelection::Named(selected) = &self.selected_target else {
            return None;
        };
        self.targets.iter().find(|target| &target.name == selected)
    }

    pub(crate) fn set_action(&mut self, action: BuildAction, cx: &mut Context<Self>) {
        self.action = action;
        self.action_error = None;
        self.serialize(cx);
        cx.notify();
    }

    pub(crate) fn can_run_action(&self) -> bool {
        if self.running_action.is_some() || self.is_configuring {
            return false;
        }
        match &self.selected_target {
            TargetSelection::All => {
                self.action == BuildAction::Build && !self.targets.is_empty()
                    || self
                        .targets
                        .iter()
                        .any(|target| target.executable.is_some())
            }
            TargetSelection::Named(_) => self.selected_target().is_some_and(|target| {
                self.action == BuildAction::Build || target.executable.is_some()
            }),
        }
    }

    /// Whether Run or Debug can't tell which program to start, because all
    /// targets are selected and there's more than one executable. Like CMake
    /// Tools, the user is then asked to pick a target.
    pub(crate) fn needs_target_choice(&self) -> bool {
        self.action != BuildAction::Build && self.program().is_none()
    }

    /// The program that Run and Debug start: the selected target, or the only
    /// executable target when all targets are selected.
    fn program(&self) -> Option<(SharedString, PathBuf)> {
        let target = match &self.selected_target {
            TargetSelection::Named(_) => self.selected_target()?,
            TargetSelection::All => {
                let mut executables = self
                    .targets
                    .iter()
                    .filter(|target| target.executable.is_some());
                let only_executable = executables.next()?;
                executables.next().is_none().then_some(only_executable)?
            }
        };
        Some((target.name.clone(), target.executable.clone()?))
    }

    /// Builds the selected target in a terminal, publishes the compiler's
    /// issues to the Problems panel, and then runs or debugs the target if
    /// that's the selected action and the build succeeded.
    pub(crate) fn run_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_run_action() {
            return;
        }
        let (Some(source_dir), Some(preset)) = (
            self.presets_source_dir(cx),
            self.selected_configure_preset.clone(),
        ) else {
            return;
        };
        let action = self.action;
        let program = match action {
            BuildAction::Build => None,
            BuildAction::Run | BuildAction::Debug => {
                let Some(program) = self.program() else {
                    return;
                };
                Some(program)
            }
        };
        let build_target = match &self.selected_target {
            TargetSelection::All => None,
            TargetSelection::Named(target) => Some(target.clone()),
        };
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        let cmake_program = self.cmake_program();
        let shell_kind = Shell::System.shell_kind(cfg!(windows));

        self.running_action = Some(action);
        self.action_error = None;
        self.build_stopped = false;
        self.publish_build_issues(Vec::new(), cx);
        cx.notify();

        self.action_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let build_dir = cx
                    .background_spawn({
                        let preset = preset.clone();
                        async move {
                            presets::resolve_binary_dir(fs.as_ref(), &source_dir, &preset).await
                        }
                    })
                    .await?
                    .with_context(|| format!("Preset {preset:?} doesn't set binaryDir"))?;

                let build_task = build_task(
                    shell_kind,
                    &cmake_program,
                    &build_dir,
                    build_target.as_deref(),
                )?;
                let terminal = spawn_terminal_task(&workspace, &build_task, cx).await?;
                this.update(cx, |this, cx| {
                    this.build_terminal = Some(terminal.clone());
                    cx.notify();
                })?;
                let exit_status = terminal
                    .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))?
                    .await;
                let output = terminal.read_with(cx, |terminal, _| terminal.get_content())?;
                let issues = cx
                    .background_spawn(async move {
                        build_issues::parse_build_output(&output, &build_dir)
                    })
                    .await;
                // Issues printed before a stop are still worth showing.
                let build_stopped = this.update(cx, |this, cx| {
                    this.publish_build_issues(issues, cx);
                    this.build_terminal = None;
                    std::mem::take(&mut this.build_stopped)
                })?;
                if build_stopped {
                    return anyhow::Ok(());
                }
                anyhow::ensure!(
                    exit_status.is_some_and(|status| status.success()),
                    "Build failed"
                );

                match (action, program) {
                    (BuildAction::Run, Some((_, executable))) => {
                        let run_task = run_task(shell_kind, &executable)?;
                        let terminal = spawn_terminal_task(&workspace, &run_task, cx).await?;
                        this.update(cx, |this, cx| this.watch_program(terminal, cx))?;
                    }
                    (BuildAction::Debug, Some((name, executable))) => {
                        let scenario = debug_scenario(&name, &executable);
                        workspace.update_in(cx, |workspace, window, cx| {
                            workspace.start_debug_session(
                                scenario,
                                SharedTaskContext::default(),
                                None,
                                None,
                                window,
                                cx,
                            );
                        })?;
                    }
                    (BuildAction::Build, _) | (BuildAction::Run | BuildAction::Debug, None) => {}
                }
                anyhow::Ok(())
            }
            .await;

            this.update(cx, |this, cx| {
                this.running_action = None;
                this.build_terminal = None;
                if let Err(error) = result {
                    this.action_error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Tracks a program started by Run so that Stop can kill it until it exits.
    fn watch_program(&mut self, terminal: WeakEntity<Terminal>, cx: &mut Context<Self>) {
        self.program_terminal = Some(terminal.clone());
        cx.notify();
        self.program_task = cx.spawn(async move |this, cx| {
            if let Ok(completion) =
                terminal.read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
            {
                completion.await;
            }
            this.update(cx, |this, cx| {
                this.program_terminal = None;
                cx.notify();
            })
            .ok();
        });
    }

    pub(crate) fn can_stop(&self) -> bool {
        if self.running_action.is_some() {
            self.build_terminal.is_some()
        } else {
            self.program_terminal.is_some()
        }
    }

    /// Kills the running build, or the program started by Run if no build is
    /// running.
    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        let terminal = if self.running_action.is_some() {
            self.build_stopped = true;
            self.build_terminal.clone()
        } else {
            self.program_terminal.clone()
        };
        if let Some(terminal) = terminal.and_then(|terminal| terminal.upgrade()) {
            terminal.update(cx, |terminal, _| terminal.kill_active_task());
        }
    }

    /// Replaces the build issues shown in the Problems panel and in editors.
    fn publish_build_issues(&mut self, issues: Vec<BuildIssue>, cx: &mut Context<Self>) {
        let lsp_store = self.project.read(cx).lsp_store();
        let previous_paths = std::mem::take(&mut self.build_issue_paths);
        self.build_issue_paths = build_issues::publish(&lsp_store, &issues, previous_paths, cx);
    }

    pub(crate) fn selected_configure_preset_label(&self) -> SharedString {
        self.selected_configure_preset
            .as_ref()
            .and_then(|selected| {
                self.configure_presets
                    .iter()
                    .find(|preset| &preset.name == selected)
            })
            .map(ConfigurePreset::label)
            .unwrap_or_else(|| SharedString::from("No configure presets"))
    }

    pub(crate) fn selected_target_label(&self) -> SharedString {
        if self.is_configuring {
            return SharedString::from("Configuring...");
        }
        match &self.selected_target {
            TargetSelection::Named(target) => target.clone(),
            TargetSelection::All if self.targets.is_empty() => SharedString::from("No targets"),
            TargetSelection::All => SharedString::from(ALL_TARGETS_LABEL),
        }
    }
}

fn is_presets_file(path: &RelPath) -> bool {
    PRESETS_FILE_NAMES.contains(&path.as_unix_str())
}

/// clangd reads a `.clangd` file from every parent directory of a source
/// file, so any of them in the project counts.
fn is_clangd_config_file(path: &RelPath) -> bool {
    path.file_name() == Some(".clangd")
}

/// The terminal pastes a task's command and arguments into a shell command
/// line verbatim, so every word must be quoted here. Preset names, for
/// example, come from the project's files and often contain spaces.
fn shell_command(shell_kind: ShellKind, words: &[&str]) -> Result<String> {
    Ok(words
        .iter()
        .map(|word| {
            shell_kind
                .try_quote(word)
                .map(|quoted| quoted.into_owned())
                .with_context(|| format!("can't quote {word:?} for the shell"))
        })
        .collect::<Result<Vec<_>>>()?
        .join(" "))
}

/// Spawns a task through the terminal panel directly, rather than through
/// `Workspace::spawn_in_terminal`, because only the panel hands back the
/// terminal, which is needed to read the output and to stop the task.
async fn spawn_terminal_task(
    workspace: &WeakEntity<Workspace>,
    task: &SpawnInTerminal,
    cx: &mut AsyncWindowContext,
) -> Result<WeakEntity<Terminal>> {
    // `spawn_task` reads the workspace, so it must not be called while the
    // workspace is being updated or GPUI panics.
    let terminal_panel = workspace
        .read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx))?
        .context("the terminal panel isn't available")?;
    terminal_panel
        .update_in(cx, |terminal_panel, window, cx| {
            terminal_panel.spawn_task(task, window, cx)
        })?
        .await
}

fn terminal_task(label: &str, command: String, cwd: PathBuf) -> SpawnInTerminal {
    // The terminal panel reuses a task's terminal by `full_label`, so a fixed
    // label per kind of task makes repeated runs share one terminal tab.
    SpawnInTerminal {
        id: TaskId(label.to_string()),
        full_label: label.to_string(),
        label: label.to_string(),
        command: Some(command.clone()),
        command_label: command,
        cwd: Some(cwd),
        reveal: RevealStrategy::NoFocus,
        show_summary: true,
        show_command: true,
        ..SpawnInTerminal::default()
    }
}

fn configure_task(
    shell_kind: ShellKind,
    cmake_program: &str,
    preset: &str,
    source_dir: PathBuf,
) -> Result<SpawnInTerminal> {
    let command = shell_command(shell_kind, &[cmake_program, "--preset", preset])?;
    Ok(terminal_task("CMake: Configure", command, source_dir))
}

/// Builds the default target when `target` is `None`.
fn build_task(
    shell_kind: ShellKind,
    cmake_program: &str,
    build_dir: &Path,
    target: Option<&str>,
) -> Result<SpawnInTerminal> {
    let build_dir_text = build_dir.to_string_lossy();
    let mut words = vec![cmake_program, "--build", &build_dir_text];
    if let Some(target) = target {
        words.extend(["--target", target]);
    }
    let command = shell_command(shell_kind, &words)?;
    Ok(terminal_task(
        "CMake: Build",
        command,
        build_dir.to_path_buf(),
    ))
}

fn run_task(shell_kind: ShellKind, executable: &Path) -> Result<SpawnInTerminal> {
    let command = shell_command(shell_kind, &[&executable.to_string_lossy()])?;
    let working_dir = executable.parent().unwrap_or(executable).to_path_buf();
    Ok(SpawnInTerminal {
        // Replaces a still running instance instead of waiting for it to exit.
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Always,
        ..terminal_task("CMake: Run", command, working_dir)
    })
}

fn debug_scenario(target: &str, executable: &Path) -> DebugScenario {
    let working_dir = executable.parent().unwrap_or(executable);
    DebugScenario {
        adapter: "CodeLLDB".into(),
        label: format!("CMake: Debug {target}").into(),
        build: None,
        config: json!({
            "request": "launch",
            "program": executable.to_string_lossy(),
            "cwd": working_dir.to_string_lossy(),
        }),
        tcp_connection: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_configure_task_quotes_command() {
        let task = configure_task(
            ShellKind::Posix,
            "/opt/my tools/cmake",
            "apQualizr2 Debug",
            PathBuf::from("/project"),
        )
        .unwrap();
        assert_eq!(
            task.command.as_deref(),
            Some("'/opt/my tools/cmake' --preset 'apQualizr2 Debug'")
        );
        assert!(task.args.is_empty());

        assert!(
            configure_task(
                ShellKind::Posix,
                "cmake",
                "nul\0byte",
                PathBuf::from("/project")
            )
            .is_err()
        );
    }
}
