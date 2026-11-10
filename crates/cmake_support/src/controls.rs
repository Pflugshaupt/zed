use std::{collections::BTreeMap, rc::Rc, sync::Arc};

use gpui::{
    Anchor, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ManagedView, Render, Task,
    WeakEntity, point,
};
use picker::{Picker, PickerDelegate};
use ui::{
    ButtonLike, ContextMenu, ElevationIndex, ListItem, ListItemSpacing, PopoverMenu,
    PopoverMenuHandle, SplitButton, SplitButtonStyle, TintColor, Tooltip, prelude::*,
};

use crate::{
    cmake_project::{ALL_TARGETS_LABEL, BuildAction, CMakeProject, TargetSelection},
    file_api::Target,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlsStyle {
    /// Outlined buttons, with text one step larger than Zed's panel defaults.
    Panel,
    /// Borderless buttons with small text, like the title bar's own.
    TitleBar,
}

impl ControlsStyle {
    fn id(self, name: &str) -> SharedString {
        let prefix = match self {
            Self::Panel => "cmake-panel",
            Self::TitleBar => "cmake-title-bar",
        };
        SharedString::from(format!("{prefix}-{name}"))
    }

    fn label_size(self) -> LabelSize {
        match self {
            Self::Panel => LabelSize::Default,
            Self::TitleBar => LabelSize::Small,
        }
    }

    fn icon_size(self) -> IconSize {
        match self {
            Self::Panel => IconSize::Small,
            Self::TitleBar => IconSize::XSmall,
        }
    }

    fn button_style(self) -> ButtonStyle {
        match self {
            Self::Panel => ButtonStyle::Outlined,
            Self::TitleBar => ButtonStyle::Subtle,
        }
    }
}

/// The preset and target menus and the action and stop buttons of a
/// `CMakeProject`. The panel and the title bar each have their own, so that
/// each opens its own popups.
pub(crate) struct Controls {
    cmake_project: Entity<CMakeProject>,
    style: ControlsStyle,
    configure_preset_menu_handle: PopoverMenuHandle<ItemSelector>,
    target_menu_handle: PopoverMenuHandle<ContextMenu>,
    action_menu_handle: PopoverMenuHandle<ContextMenu>,
}

impl Controls {
    pub(crate) fn new(cmake_project: Entity<CMakeProject>, style: ControlsStyle) -> Self {
        Self {
            cmake_project,
            style,
            configure_preset_menu_handle: PopoverMenuHandle::default(),
            target_menu_handle: PopoverMenuHandle::default(),
            action_menu_handle: PopoverMenuHandle::default(),
        }
    }

    pub(crate) fn configure_preset_selector(&self, cx: &App) -> impl IntoElement {
        let cmake_project = self.cmake_project.read(cx);
        let weak_cmake_project = self.cmake_project.downgrade();
        let options = ItemSelectorOptions {
            items: cmake_project
                .configure_presets()
                .iter()
                .map(|preset| SelectorItem {
                    id: preset.name.clone(),
                    label: preset.label(),
                })
                .collect(),
            selected_id: cmake_project.selected_configure_preset().cloned(),
            placeholder: "Search presets...",
            on_confirm: Rc::new(move |preset, window, cx| {
                let cmake_project = weak_cmake_project.clone();
                // Deferred because configuring updates the workspace, which may
                // be mid-update while the popover handles the confirmation.
                window.defer(cx, move |window, cx| {
                    cmake_project
                        .update(cx, |cmake_project, cx| {
                            cmake_project.select_configure_preset(preset, window, cx)
                        })
                        .ok();
                });
            }),
        };
        self.render_selector(
            "configure-preset",
            cmake_project.selected_configure_preset_label(),
            "Select Configure Preset",
            cmake_project.configure_presets().is_empty(),
            &self.configure_preset_menu_handle,
            move |window, cx| {
                let options = options.clone();
                Some(cx.new(|cx| ItemSelector::new(options, window, cx)))
            },
        )
    }

    pub(crate) fn target_selector(&self, cx: &App) -> impl IntoElement {
        let cmake_project = self.cmake_project.read(cx);
        let weak_cmake_project = self.cmake_project.downgrade();
        let folder = Rc::new(TargetFolder::new(
            cmake_project.targets(),
            cmake_project.use_folders(),
        ));
        let selected_target = cmake_project.target_selection().clone();
        self.render_selector(
            "target",
            cmake_project.selected_target_label(),
            "Select Target",
            cmake_project.is_configuring() || cmake_project.targets().is_empty(),
            &self.target_menu_handle,
            move |window, cx| {
                let cmake_project = weak_cmake_project.clone();
                let folder = folder.clone();
                let selected_target = selected_target.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let menu = menu
                        .toggleable_entry(
                            ALL_TARGETS_LABEL,
                            selected_target == TargetSelection::All,
                            IconPosition::Start,
                            None,
                            {
                                let cmake_project = cmake_project.clone();
                                move |_, cx| {
                                    cmake_project
                                        .update(cx, |cmake_project, cx| {
                                            cmake_project.select_target(TargetSelection::All, cx)
                                        })
                                        .ok();
                                }
                            },
                        )
                        .separator();
                    let selected_target = match &selected_target {
                        TargetSelection::All => None,
                        TargetSelection::Named(target) => Some(target),
                    };
                    folder.add_to_menu(menu, selected_target, &cmake_project)
                }))
            },
        )
    }

    fn render_selector<M: ManagedView>(
        &self,
        name: &str,
        label: SharedString,
        tooltip: &'static str,
        disabled: bool,
        menu_handle: &PopoverMenuHandle<M>,
        menu: impl Fn(&mut Window, &mut App) -> Option<Entity<M>> + 'static,
    ) -> impl IntoElement {
        let style = self.style;
        let chevron_icon = if menu_handle.is_deployed() {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };

        PopoverMenu::new(style.id(&format!("{name}-popover")))
            .with_handle(menu_handle.clone())
            .menu(menu)
            .trigger_with_tooltip(
                Button::new(style.id(name), label)
                    .style(style.button_style())
                    .when(style == ControlsStyle::Panel, |button| {
                        button.layer(ElevationIndex::ModalSurface)
                    })
                    .size(ButtonSize::Default)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .label_size(style.label_size())
                    .truncate(true)
                    .end_icon(
                        Icon::new(chevron_icon)
                            .size(style.icon_size())
                            .color(Color::Muted),
                    )
                    .disabled(disabled),
                Tooltip::text(tooltip),
            )
            .anchor(Anchor::TopLeft)
            .offset(point(px(0.), px(2.)))
    }

    /// A split button like the git panel's: the left part runs the current
    /// action, and the chevron opens a menu that switches between actions.
    pub(crate) fn action_button(&self, cx: &App) -> impl IntoElement {
        let style = self.style;
        let cmake_project = self.cmake_project.read(cx);
        let action = cmake_project.action();
        let label = if cmake_project.running_action().is_some() {
            "Building..."
        } else {
            action.label()
        };
        let chevron_icon = if self.action_menu_handle.is_deployed() {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };

        let primary_button = ButtonLike::new_rounded_left(style.id("action"))
            .when(style == ControlsStyle::Panel, |button| {
                button.layer(ElevationIndex::ModalSurface)
            })
            .size(ButtonSize::Default)
            .child(
                h_flex()
                    .gap_1()
                    .px_1()
                    .child(
                        Icon::new(action.icon())
                            .size(style.icon_size())
                            .color(Color::Muted),
                    )
                    .child(Label::new(label).size(style.label_size())),
            )
            .tooltip(Tooltip::text(action.tooltip()))
            .disabled(!cmake_project.can_run_action())
            .on_click({
                let cmake_project = self.cmake_project.clone();
                let target_menu_handle = self.target_menu_handle.clone();
                move |_, window, cx| {
                    if cmake_project.read(cx).needs_target_choice() {
                        target_menu_handle.show(window, cx);
                    } else {
                        cmake_project
                            .update(cx, |cmake_project, cx| cmake_project.run_action(window, cx));
                    }
                }
            });

        let weak_cmake_project = self.cmake_project.downgrade();
        let action_menu = PopoverMenu::new(style.id("action-menu"))
            .trigger(
                ButtonLike::new_rounded_right(style.id("action-menu-trigger"))
                    .when(style == ControlsStyle::Panel, |button| {
                        button.layer(ElevationIndex::ModalSurface)
                    })
                    .size(ButtonSize::Default)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .width(rems_from_px(24_f32))
                    .child(Icon::new(chevron_icon).size(style.icon_size())),
            )
            .with_handle(self.action_menu_handle.clone())
            .menu(move |window, cx| {
                let cmake_project = weak_cmake_project.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for menu_action in BuildAction::ALL {
                        let cmake_project = cmake_project.clone();
                        menu = menu.toggleable_entry(
                            menu_action.label(),
                            menu_action == action,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                cmake_project
                                    .update(cx, |cmake_project, cx| {
                                        cmake_project.set_action(menu_action, cx)
                                    })
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
            .anchor(Anchor::TopLeft)
            .offset(point(px(0.), px(2.)));

        SplitButton::new(primary_button, action_menu.into_any_element()).style(match style {
            ControlsStyle::Panel => SplitButtonStyle::Filled,
            ControlsStyle::TitleBar => SplitButtonStyle::Transparent,
        })
    }

    pub(crate) fn stop_button(&self, cx: &App) -> impl IntoElement {
        let style = self.style;
        let cmake_project = self.cmake_project.read(cx);
        let can_stop = cmake_project.can_stop();
        let tooltip = if cmake_project.running_action().is_some() {
            "Stop the build"
        } else {
            "Stop the running program"
        };
        IconButton::new(style.id("stop"), IconName::Stop)
            .style(style.button_style())
            .when(style == ControlsStyle::Panel, |button| {
                button.layer(ElevationIndex::ModalSurface)
            })
            .size(ButtonSize::Default)
            .icon_size(style.icon_size())
            .icon_color(if can_stop { Color::Error } else { Color::Muted })
            .tooltip(Tooltip::text(tooltip))
            .disabled(!can_stop)
            .on_click({
                let cmake_project = self.cmake_project.clone();
                move |_, _, cx| cmake_project.update(cx, |cmake_project, cx| cmake_project.stop(cx))
            })
    }
}

/// The target menu's contents. Targets are placed in submenus by their
/// `FOLDER` property, nested at each `/`, the way IDEs group them.
#[derive(Clone, Debug, Default, PartialEq)]
struct TargetFolder {
    subfolders: BTreeMap<SharedString, TargetFolder>,
    targets: Vec<SharedString>,
}

impl TargetFolder {
    fn new(targets: &[Target], use_folders: bool) -> Self {
        let mut root = Self::default();
        for target in targets {
            let mut folder = &mut root;
            let folder_names = target
                .folder
                .iter()
                .filter(|_| use_folders)
                .flat_map(|folder| folder.split('/'))
                .filter(|name| !name.is_empty());
            for folder_name in folder_names {
                folder = folder
                    .subfolders
                    .entry(SharedString::from(folder_name.to_string()))
                    .or_default();
            }
            folder.targets.push(target.name.clone());
        }
        root
    }

    fn add_to_menu(
        &self,
        mut menu: ContextMenu,
        selected_target: Option<&SharedString>,
        cmake_project: &WeakEntity<CMakeProject>,
    ) -> ContextMenu {
        for (name, subfolder) in &self.subfolders {
            let subfolder = subfolder.clone();
            let selected_target = selected_target.cloned();
            let cmake_project = cmake_project.clone();
            menu = menu.submenu_with_icon(name.clone(), IconName::Folder, move |menu, _, _| {
                subfolder.add_to_menu(menu, selected_target.as_ref(), &cmake_project)
            });
        }
        for target in &self.targets {
            let cmake_project = cmake_project.clone();
            let target_name = target.clone();
            menu = menu.toggleable_entry(
                target.clone(),
                selected_target == Some(target),
                IconPosition::Start,
                None,
                move |_, cx| {
                    cmake_project
                        .update(cx, |cmake_project, cx| {
                            cmake_project
                                .select_target(TargetSelection::Named(target_name.clone()), cx)
                        })
                        .ok();
                },
            );
        }
        menu
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

/// A searchable popup list, used for the preset selector.
pub(crate) struct ItemSelector {
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
    fn test_target_folder_nests_at_slashes() {
        let target = |name: &'static str, folder: Option<&'static str>| Target {
            name: name.into(),
            executable: None,
            folder: folder.map(SharedString::from),
        };
        let folder = |subfolders: Vec<(&'static str, TargetFolder)>, targets: Vec<&'static str>| {
            TargetFolder {
                subfolders: subfolders
                    .into_iter()
                    .map(|(name, folder)| (SharedString::from(name), folder))
                    .collect(),
                targets: targets.into_iter().map(SharedString::from).collect(),
            }
        };

        let targets = [
            target("app", None),
            target("dialogs", Some("GUI/Dialogs")),
            target("gui", Some("GUI")),
            target("tests", Some("/Tests/")),
            target("widgets", Some("GUI/Dialogs")),
        ];
        assert_eq!(
            TargetFolder::new(&targets, true),
            folder(
                vec![
                    (
                        "GUI",
                        folder(
                            vec![("Dialogs", folder(vec![], vec!["dialogs", "widgets"]))],
                            vec!["gui"],
                        ),
                    ),
                    ("Tests", folder(vec![], vec!["tests"])),
                ],
                vec!["app"],
            )
        );
        assert_eq!(
            TargetFolder::new(&targets, false),
            folder(vec![], vec!["app", "dialogs", "gui", "tests", "widgets"])
        );
    }
}
