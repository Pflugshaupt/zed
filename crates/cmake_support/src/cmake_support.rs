mod build_issues;
mod cmake_project;
mod controls;
mod file_api;
mod presets;

use cmake_project::CMakeProject;
use controls::{Controls, ControlsStyle};
use gpui::{Action, Entity, EventEmitter, FocusHandle, Focusable, Render, Subscription, actions};
use title_bar::TitleBar;
use ui::{Divider, DividerColor, Tab, prelude::*};
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

        let workspace_handle = cx.weak_entity();
        let project = workspace.project().clone();
        let serialization_key = cmake_project::serialization_key(workspace);
        let cmake_project =
            cx.new(|cx| CMakeProject::new(workspace_handle, project, serialization_key, cx));

        // Added synchronously while the workspace is being created, so the panel
        // already exists in its dock when serialized dock state is restored.
        let panel = cx.new(|cx| CMakePanel::new(cmake_project.clone(), cx));
        workspace.add_panel(panel, window, cx);

        // The title bar is created by an observer that runs after this one.
        let title_bar_item = cx.new(|cx| CMakeTitleBarItem::new(cmake_project, cx));
        cx.defer_in(window, move |workspace, _window, cx| {
            let Some(title_bar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            else {
                log::error!("the CMake controls can't be added to the title bar");
                return;
            };
            title_bar.update(cx, |title_bar, cx| {
                title_bar.add_left_item(title_bar_item.into(), cx)
            });
        });
    })
    .detach();
}

pub struct CMakePanel {
    cmake_project: Entity<CMakeProject>,
    controls: Controls,
    focus_handle: FocusHandle,
    position: DockPosition,
    _observe_cmake_project: Subscription,
}

impl CMakePanel {
    fn new(cmake_project: Entity<CMakeProject>, cx: &mut Context<Self>) -> Self {
        Self {
            controls: Controls::new(cmake_project.clone(), ControlsStyle::Panel),
            _observe_cmake_project: cx.observe(&cmake_project, |_, _, cx| cx.notify()),
            cmake_project,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
        }
    }
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
        let cmake_project = self.cmake_project.read(cx);

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
                        self.controls.configure_preset_selector(cx),
                        cmake_project.configure_presets_error().cloned(),
                    ))
                    .child(render_section(
                        "Target",
                        self.controls.target_selector(cx),
                        cmake_project.targets_error().cloned(),
                    ))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(self.controls.action_button(cx))
                                    .child(self.controls.stop_button(cx)),
                            )
                            .when_some(cmake_project.action_error().cloned(), |this, error| {
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

/// The panel's menus and buttons, mirrored in Zed's title bar for projects
/// with a presets file.
struct CMakeTitleBarItem {
    cmake_project: Entity<CMakeProject>,
    controls: Controls,
    _observe_cmake_project: Subscription,
}

impl CMakeTitleBarItem {
    fn new(cmake_project: Entity<CMakeProject>, cx: &mut Context<Self>) -> Self {
        Self {
            controls: Controls::new(cmake_project.clone(), ControlsStyle::TitleBar),
            _observe_cmake_project: cx.observe(&cmake_project, |_, _, cx| cx.notify()),
            cmake_project,
        }
    }
}

impl Render for CMakeTitleBarItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .when(self.cmake_project.read(cx).has_presets_file(), |this| {
                this.ml_1()
                    .child(Divider::vertical().color(DividerColor::Border))
                    .child(self.controls.configure_preset_selector(cx))
                    .child(self.controls.target_selector(cx))
                    .child(self.controls.action_button(cx))
                    .child(self.controls.stop_button(cx))
            })
    }
}
