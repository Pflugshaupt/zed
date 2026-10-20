mod build_issues;
mod file_api;
mod presets;

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use build_issues::BuildIssue;
use collections::HashSet;
use file_api::Target;
use fs::Fs;
use gpui::{
    Action, Anchor, AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Render, Subscription, Task, WeakEntity, actions, point,
};
use picker::{Picker, PickerDelegate};
use presets::{ConfigurePreset, PRESETS_FILE_NAMES};
use project::Project;
use serde::Deserialize;
use serde_json::json;
use settings::SettingsStore;
use task::{DebugScenario, RevealStrategy, SharedTaskContext, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::terminal_panel::TerminalPanel;
use ui::{
    ButtonLike, ContextMenu, ElevationIndex, ListItem, ListItemSpacing, PopoverMenu,
    PopoverMenuHandle, SplitButton, Tab, TintColor, Tooltip, prelude::*,
};
use util::{ResultExt as _, rel_path::RelPath, shell::ShellKind};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    cmake_panel,
    [
        /// Toggles focus on the CMake panel.
        ToggleFocus,
    ]
);

const CMAKE_PANEL_KEY: &str = "CMakePanel";

/// Everything this crate needs is wired up from here so that integrating it
/// into Zed only requires calling this function once at startup.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<CMakePanel>(window, cx);
        });

        // Added synchronously while the workspace is being created, so the panel
        // already exists in its dock when serialized dock state is restored.
        let workspace_handle = cx.weak_entity();
        let project = workspace.project().clone();
        let panel = cx.new(|cx| CMakePanel::new(workspace_handle, project, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildAction {
    Build,
    Run,
    Debug,
}

impl BuildAction {
    const ALL: [Self; 3] = [Self::Build, Self::Run, Self::Debug];

    fn label(self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::Run => "Run",
            Self::Debug => "Debug",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Build => "Build the selected target",
            Self::Run => "Build and run the selected target",
            Self::Debug => "Build and debug the selected target",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Build => IconName::ToolHammer,
            Self::Run => IconName::PlayFilled,
            Self::Debug => IconName::Debug,
        }
    }
}

pub struct CMakePanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    position: DockPosition,
    settings_loaded: bool,
    cmake_path: Option<String>,
    configure_presets: Vec<ConfigurePreset>,
    selected_configure_preset: Option<SharedString>,
    configure_presets_error: Option<SharedString>,
    targets: Vec<Target>,
    selected_target: Option<SharedString>,
    targets_error: Option<SharedString>,
    is_configuring: bool,
    action: BuildAction,
    running_action: Option<BuildAction>,
    action_error: Option<SharedString>,
    /// The terminal of the running build, which the Stop button kills.
    build_terminal: Option<WeakEntity<Terminal>>,
    /// Set when Stop kills a build, so that Run and Debug don't continue.
    build_stopped: bool,
    /// The terminal of the program started by Run while it's running, which
    /// the Stop button kills when no build is running.
    program_terminal: Option<WeakEntity<Terminal>>,
    /// Files that currently have build issues in the Problems panel.
    build_issue_paths: HashSet<PathBuf>,
    configure_preset_menu_handle: PopoverMenuHandle<ItemSelector>,
    target_menu_handle: PopoverMenuHandle<ItemSelector>,
    action_menu_handle: PopoverMenuHandle<ContextMenu>,
    load_settings_task: Task<()>,
    refresh_configure_presets_task: Task<()>,
    refresh_targets_task: Task<()>,
    configure_task: Task<()>,
    action_task: Task<()>,
    program_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl CMakePanel {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&project, |this, _project, event, cx| match event {
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                    this.refresh_configure_presets(cx);
                }
                project::Event::WorktreeUpdatedEntries(_, entries)
                    if entries.iter().any(|(path, _, _)| is_presets_file(path)) =>
                {
                    this.refresh_configure_presets(cx);
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
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            settings_loaded: false,
            cmake_path: None,
            configure_presets: Vec::new(),
            selected_configure_preset: None,
            configure_presets_error: None,
            targets: Vec::new(),
            selected_target: None,
            targets_error: None,
            is_configuring: false,
            action: BuildAction::Build,
            running_action: None,
            action_error: None,
            build_terminal: None,
            build_stopped: false,
            program_terminal: None,
            build_issue_paths: HashSet::default(),
            configure_preset_menu_handle: PopoverMenuHandle::default(),
            target_menu_handle: PopoverMenuHandle::default(),
            action_menu_handle: PopoverMenuHandle::default(),
            load_settings_task: Task::ready(()),
            refresh_configure_presets_task: Task::ready(()),
            refresh_targets_task: Task::ready(()),
            configure_task: Task::ready(()),
            action_task: Task::ready(()),
            program_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.load_settings(cx);
        this
    }

    fn load_settings(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        self.load_settings_task = cx.spawn(async move |this, cx| {
            let settings = load_cmake_settings(fs.as_ref()).await;
            this.update(cx, |this, cx| {
                let cmake_path = match settings {
                    Ok(settings) => settings.path,
                    // Zed reports invalid settings files itself; keep the last good value.
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
        if !self.settings_loaded {
            return;
        }

        // Running the local cmake against a remote project's paths would be wrong.
        if !self.project.read(cx).is_local() {
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
            self.refresh_configure_presets_task = Task::ready(());
            self.set_configure_presets(Ok(Vec::new()), cx);
            return;
        };

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

    fn select_configure_preset(
        &mut self,
        preset: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_configure_preset = Some(preset);
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
        if self.selected_target().is_none() {
            self.selected_target = self.targets.first().map(|target| target.name.clone());
        }
        cx.notify();
    }

    fn select_target(&mut self, target: SharedString, cx: &mut Context<Self>) {
        self.selected_target = Some(target);
        self.action_error = None;
        cx.notify();
    }

    fn selected_target(&self) -> Option<&Target> {
        let selected = self.selected_target.as_ref()?;
        self.targets.iter().find(|target| &target.name == selected)
    }

    fn set_action(&mut self, action: BuildAction, cx: &mut Context<Self>) {
        self.action = action;
        self.action_error = None;
        cx.notify();
    }

    fn can_run_action(&self) -> bool {
        let Some(target) = self.selected_target() else {
            return false;
        };
        self.running_action.is_none()
            && !self.is_configuring
            && (self.action == BuildAction::Build || target.executable.is_some())
    }

    /// Builds the selected target in a terminal, publishes the compiler's
    /// issues to the Problems panel, and then runs or debugs the target if
    /// that's the selected action and the build succeeded.
    fn run_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_run_action() {
            return;
        }
        let (Some(source_dir), Some(preset), Some(target)) = (
            self.presets_source_dir(cx),
            self.selected_configure_preset.clone(),
            self.selected_target().cloned(),
        ) else {
            return;
        };
        let action = self.action;
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

                let build_task = build_task(shell_kind, &cmake_program, &build_dir, &target.name)?;
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

                let executable = target.executable.as_deref();
                match (action, executable) {
                    (BuildAction::Build, _) => {}
                    (BuildAction::Run, Some(executable)) => {
                        let run_task = run_task(shell_kind, executable)?;
                        let terminal = spawn_terminal_task(&workspace, &run_task, cx).await?;
                        this.update(cx, |this, cx| this.watch_program(terminal, cx))?;
                    }
                    (BuildAction::Debug, Some(executable)) => {
                        let scenario = debug_scenario(&target.name, executable);
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
                    (BuildAction::Run | BuildAction::Debug, None) => {
                        anyhow::bail!("{} isn't an executable target", target.name);
                    }
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

    fn can_stop(&self) -> bool {
        if self.running_action.is_some() {
            self.build_terminal.is_some()
        } else {
            self.program_terminal.is_some()
        }
    }

    /// Kills the running build, or the program started by Run if no build is
    /// running.
    fn stop(&mut self, cx: &mut Context<Self>) {
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

    fn selected_configure_preset_label(&self) -> SharedString {
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

    fn selected_target_label(&self) -> SharedString {
        if self.is_configuring {
            SharedString::from("Configuring...")
        } else {
            self.selected_target
                .clone()
                .unwrap_or_else(|| SharedString::from("No targets"))
        }
    }

    fn configure_preset_selector_options(&self, cx: &mut Context<Self>) -> ItemSelectorOptions {
        let panel = cx.entity().downgrade();
        ItemSelectorOptions {
            items: self
                .configure_presets
                .iter()
                .map(|preset| SelectorItem {
                    id: preset.name.clone(),
                    label: preset.label(),
                })
                .collect(),
            selected_id: self.selected_configure_preset.clone(),
            placeholder: "Search presets...",
            on_confirm: Rc::new(move |preset, window, cx| {
                let panel = panel.clone();
                // Deferred because configuring updates the workspace, which may
                // be mid-update while the popover handles the confirmation.
                window.defer(cx, move |window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            panel.select_configure_preset(preset, window, cx)
                        })
                        .ok();
                });
            }),
        }
    }

    fn target_selector_options(&self, cx: &mut Context<Self>) -> ItemSelectorOptions {
        let panel = cx.entity().downgrade();
        ItemSelectorOptions {
            items: self
                .targets
                .iter()
                .map(|target| SelectorItem {
                    id: target.name.clone(),
                    label: target.name.clone(),
                })
                .collect(),
            selected_id: self.selected_target.clone(),
            placeholder: "Search targets...",
            on_confirm: Rc::new(move |target, _window, cx| {
                panel
                    .update(cx, |panel, cx| panel.select_target(target, cx))
                    .ok();
            }),
        }
    }
}

fn is_presets_file(path: &RelPath) -> bool {
    PRESETS_FILE_NAMES.contains(&path.as_unix_str())
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

fn build_task(
    shell_kind: ShellKind,
    cmake_program: &str,
    build_dir: &Path,
    target: &str,
) -> Result<SpawnInTerminal> {
    let build_dir_text = build_dir.to_string_lossy();
    let command = shell_command(
        shell_kind,
        &[
            cmake_program,
            "--build",
            &build_dir_text,
            "--target",
            target,
        ],
    )?;
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

fn render_selector(
    id: &'static str,
    label: SharedString,
    tooltip: &'static str,
    disabled: bool,
    menu_handle: &PopoverMenuHandle<ItemSelector>,
    options: ItemSelectorOptions,
) -> impl IntoElement {
    let chevron_icon = if menu_handle.is_deployed() {
        IconName::ChevronUp
    } else {
        IconName::ChevronDown
    };

    PopoverMenu::new(SharedString::from(format!("{id}-popover")))
        .with_handle(menu_handle.clone())
        .menu(move |window, cx| {
            let options = options.clone();
            Some(cx.new(|cx| ItemSelector::new(options, window, cx)))
        })
        .trigger_with_tooltip(
            Button::new(id, label)
                .style(ButtonStyle::Outlined)
                .layer(ElevationIndex::ModalSurface)
                .size(ButtonSize::Default)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                .label_size(LabelSize::Default)
                .truncate(true)
                .end_icon(
                    Icon::new(chevron_icon)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .disabled(disabled),
            Tooltip::text(tooltip),
        )
        .anchor(Anchor::TopLeft)
        .offset(point(px(0.), px(2.)))
}

fn render_section(
    title: &'static str,
    selector: impl IntoElement,
    error: Option<SharedString>,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            Label::new(title)
                .size(LabelSize::Default)
                .color(Color::Muted),
        )
        .child(selector)
        .when_some(error, |this, error| {
            this.child(
                Label::new(error)
                    .size(LabelSize::Default)
                    .color(Color::Error),
            )
        })
}

impl Panel for CMakePanel {
    fn persistent_name() -> &'static str {
        "CMake Panel"
    }

    fn panel_key() -> &'static str {
        CMAKE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Triangle)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("CMake Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    // Must be unique per dock; kept far from the built-in panels' priorities
    // so that upstream additions are unlikely to collide with it.
    fn activation_priority(&self) -> u32 {
        100
    }
}

impl Focusable for CMakePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CMakePanel {}

impl Render for CMakePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let configure_preset_selector = render_selector(
            "cmake-configure-preset",
            self.selected_configure_preset_label(),
            "Select Configure Preset",
            self.configure_presets.is_empty(),
            &self.configure_preset_menu_handle,
            self.configure_preset_selector_options(cx),
        );
        let target_selector = render_selector(
            "cmake-target",
            self.selected_target_label(),
            "Select Target",
            self.is_configuring || self.targets.is_empty(),
            &self.target_menu_handle,
            self.target_selector_options(cx),
        );

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                h_flex()
                    .flex_none()
                    .h(Tab::container_height(cx))
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new("CMake").size(LabelSize::Default)),
            )
            .child(
                v_flex()
                    .p_2()
                    .gap_3()
                    .child(render_section(
                        "Configure Preset",
                        configure_preset_selector,
                        self.configure_presets_error.clone(),
                    ))
                    .child(render_section(
                        "Target",
                        target_selector,
                        self.targets_error.clone(),
                    ))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(self.render_action_button(cx))
                                    .child(self.render_stop_button(cx)),
                            )
                            .when_some(self.action_error.clone(), |this, error| {
                                this.child(
                                    Label::new(error)
                                        .size(LabelSize::Default)
                                        .color(Color::Error),
                                )
                            }),
                    ),
            )
    }
}

impl CMakePanel {
    /// A split button like the git panel's: the left part runs the current
    /// action, and the chevron opens a menu that switches between actions.
    fn render_action_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let action = self.action;
        let label = if self.running_action.is_some() {
            "Building..."
        } else {
            action.label()
        };
        let chevron_icon = if self.action_menu_handle.is_deployed() {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };
        let panel = cx.entity().downgrade();

        let primary_button = ButtonLike::new_rounded_left("cmake-action")
            .layer(ElevationIndex::ModalSurface)
            .size(ButtonSize::Default)
            .child(
                h_flex()
                    .gap_1()
                    .px_1()
                    .child(
                        Icon::new(action.icon())
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(label).size(LabelSize::Default)),
            )
            .tooltip(Tooltip::text(action.tooltip()))
            .disabled(!self.can_run_action())
            .on_click(cx.listener(|this, _, window, cx| this.run_action(window, cx)));

        let action_menu = PopoverMenu::new("cmake-action-menu")
            .trigger(
                ButtonLike::new_rounded_right("cmake-action-menu-trigger")
                    .layer(ElevationIndex::ModalSurface)
                    .size(ButtonSize::Default)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .width(rems_from_px(24_f32))
                    .child(Icon::new(chevron_icon).size(IconSize::Small)),
            )
            .with_handle(self.action_menu_handle.clone())
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for menu_action in BuildAction::ALL {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            menu_action.label(),
                            menu_action == action,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_action(menu_action, cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
            .anchor(Anchor::TopLeft)
            .offset(point(px(0.), px(2.)));

        SplitButton::new(primary_button, action_menu.into_any_element())
    }

    fn render_stop_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_stop = self.can_stop();
        let tooltip = if self.running_action.is_some() {
            "Stop the build"
        } else {
            "Stop the running program"
        };
        IconButton::new("cmake-stop", IconName::Stop)
            .style(ButtonStyle::Outlined)
            .layer(ElevationIndex::ModalSurface)
            .size(ButtonSize::Default)
            .icon_size(IconSize::Small)
            .icon_color(if can_stop { Color::Error } else { Color::Muted })
            .tooltip(Tooltip::text(tooltip))
            .disabled(!can_stop)
            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
    }
}

#[derive(Clone)]
struct SelectorItem {
    id: SharedString,
    label: SharedString,
}

#[derive(Clone)]
struct ItemSelectorOptions {
    items: Vec<SelectorItem>,
    selected_id: Option<SharedString>,
    placeholder: &'static str,
    on_confirm: Rc<dyn Fn(SharedString, &mut Window, &mut App)>,
}

/// A searchable popup list, used for both the preset and the target selectors.
struct ItemSelector {
    picker: Entity<Picker<ItemSelectorDelegate>>,
}

impl ItemSelector {
    fn new(options: ItemSelectorOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selected_index = options
            .selected_id
            .as_ref()
            .and_then(|selected| options.items.iter().position(|item| &item.id == selected))
            .unwrap_or(0);
        let delegate = ItemSelectorDelegate {
            selector: cx.entity().downgrade(),
            filtered_items: options.items.clone(),
            options,
            selected_index,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(16.))
                .show_scrollbar(true)
        });
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for ItemSelector {}

impl Focusable for ItemSelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ItemSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("CMakeItemSelector")
            .child(self.picker.clone())
    }
}

struct ItemSelectorDelegate {
    selector: WeakEntity<ItemSelector>,
    options: ItemSelectorOptions,
    filtered_items: Vec<SelectorItem>,
    selected_index: usize,
}

impl PickerDelegate for ItemSelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "cmake item selector"
    }

    fn match_count(&self) -> usize {
        self.filtered_items.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.filtered_items.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.options.placeholder.into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.filtered_items = self
            .options
            .items
            .iter()
            .filter(|item| {
                item.id.to_lowercase().contains(&query)
                    || item.label.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        self.selected_index = self
            .options
            .selected_id
            .as_ref()
            .and_then(|selected| {
                self.filtered_items
                    .iter()
                    .position(|item| &item.id == selected)
            })
            .unwrap_or(0);
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(item) = self.filtered_items.get(self.selected_index) else {
            return;
        };
        (self.options.on_confirm)(item.id.clone(), window, cx);
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector
            .update(cx, |_selector, cx| cx.emit(DismissEvent))
            .ok();
    }

    // Rows mirror `ContextMenu`'s toggleable entries so the popup matches the
    // git panel's menus.
    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let item = self.filtered_items.get(ix)?;
        let is_active = self.options.selected_id.as_ref() == Some(&item.id);

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::default())
                .toggle_state(selected)
                .start_slot(
                    div()
                        .flex_none()
                        .child(Icon::new(IconName::Check).size(IconSize::Medium))
                        .when(!is_active, |this| this.invisible()),
                )
                .child(Label::new(item.label.clone()).size(LabelSize::Large)),
        )
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
