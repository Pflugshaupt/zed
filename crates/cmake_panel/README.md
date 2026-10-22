# CMake Panel

A dock panel for CMake projects, added to this fork of Zed. It sits next to the
Project, Git and Outline panels (triangle icon) and currently lets you:

- pick a **configure preset**, which runs `cmake --preset <name>` in Zed's terminal
- pick a **target**, from the targets CMake reports after configuring
- **build**, **run** or **debug** the target with one split button; build
  errors and warnings show up in Zed's Problems panel and in the editor

The same menus and buttons are also shown on the left of Zed's title bar,
for projects with a presets file.

Planned next: a **clean** button.

## Keeping the fork easy to update

Zed changes quickly upstream, so almost everything lives in this folder. Zed
only knows about it through two one-line entry points and a small hook in the
title bar:

| File | Change |
|---|---|
| `crates/zed/Cargo.toml` | `cmake_panel = { path = "../cmake_panel" }` |
| `crates/zed/src/main.rs` | `cmake_panel::init(cx);` next to `outline_panel::init(cx);` |
| `crates/title_bar/src/title_bar.rs` | A `left_items: Vec<AnyView>` field on `TitleBar`, its initialization in `TitleBar::new`, a public `add_left_item` method, one `.children(self.left_items.iter().cloned())` line at the end of the left group in `render` (before its `on_mouse_down`), and `AnyView` in the `gpui` import |
| `Cargo.lock` | Updated by Cargo. On merge conflicts, take upstream's and rebuild. |

The title bar hook is the only change inside an upstream crate. `TitleBar`
renders a fixed list of children and has no way for other crates to add
items, so it couldn't be avoided. It's additive, so upstream changes to the
title bar should rarely conflict with it. If one does, re-add those few lines.

The root `Cargo.toml` is untouched: Cargo makes the crate a workspace member
automatically because `crates/zed` depends on it by path. The repo-root
`README.md` also has a two-line review notice at the top, which the repo's
`CLAUDE.md` requires agents to add; delete it when you've reviewed the changes.

Choices made to keep it that way:

- **Registration.** `init()` uses `cx.observe_new::<Workspace>` to register the
  `cmake_panel::ToggleFocus` action and add the panel to every workspace
  window, so `crates/zed/src/zed.rs` (where the built-in panels are loaded)
  doesn't need changes. The panel is added synchronously while the workspace
  is created, so it's already in its dock when the saved dock layout is
  restored.
- **Panel order.** `activation_priority` is `100`. Priorities must be unique
  per dock (debug builds panic otherwise); the built-in panels use 0–7, so this
  stays clear of future upstream panels.
- **Icon.** Zed's existing `IconName::Triangle`, so no assets were added.
- **Title bar.** `init()` adds the title bar controls with
  `TitleBar::add_left_item`. The title bar is created by another
  `observe_new` callback that runs after this crate's, so the controls are
  added in a `defer_in`.
- **Settings.** The panel's settings are read directly from the user's
  `settings.json` instead of being registered in Zed's central settings
  schema, which lives in another crate (see below).

## Settings

Add these to your Zed `settings.json` (Zed: Open Settings File):

```json
"cmake": {
  "path": "/opt/homebrew/bin/cmake",
  "use_folders": false
}
```

- `path`: the cmake to run. By default the panel runs `cmake` from `PATH`.
- `use_folders`: whether the target menu groups targets into submenus by their
  `FOLDER` property. Defaults to `true`. CMake Tools has the same option,
  `cmake.useFolderPropertyInBuildTargetDropdown`, but off by default.

These work like `lsp.clangd.binary.path`, but as text-only settings: Zed keeps
keys it doesn't know, but they don't show up in the settings UI, aren't
autocompleted, and the editor marks `"cmake"` as an unknown property. Making
them real Zed settings would mean editing `settings_content` (the settings
struct and schema), `settings`, `assets/settings/default.json`, and
`settings_ui` for the settings UI, which this fork avoids. The panel re-reads
them whenever settings change. Only the user
settings file is read, not a project's `.zed/settings.json`.

## How it works

### Configure presets

The panel runs `cmake --list-presets=configure` in the project root. CMake
itself resolves inheritance, `include`s, `CMakeUserPresets.json`, `condition`s
and hidden presets, so the panel only parses the printed list (name plus
optional display name). This needs CMake 3.20 or newer.

- The project root is the first project folder with a `CMakePresets.json` or
  `CMakeUserPresets.json` at its top level.
- The list reloads when folders are added to or removed from the project, when
  either presets file changes, and when the cmake path setting changes.
- Errors from cmake (an invalid presets file, cmake not found) are shown under
  the selector. The selected preset survives a temporarily broken presets file.
- Remote (SSH) projects aren't supported; the panel says so instead of running
  the local cmake against remote paths.

### Configuring

Selecting a preset (including re-selecting the current one) runs
`cmake --preset <name>` as a Zed terminal task labelled "CMake: Configure".
The terminal opens without taking focus. All configures share one terminal
tab, and selecting another preset while one is running waits for it to finish.

Zed's terminal pastes a task's command and arguments into the shell command
line verbatim, so the panel quotes the cmake path and preset name for the
user's shell first. Preset names often contain spaces, and since they come from
the project's files, unquoted names could also inject shell commands.
While configuring, the target selector shows "Configuring...".

### Targets

Targets come from the
[CMake file API](https://cmake.org/cmake/help/latest/manual/cmake-file-api.7.html):

1. Before configuring, the panel writes an empty query file to
   `<build dir>/.cmake/api/v1/query/client-zed/codemodel-v2`.
2. During configure, CMake answers it with JSON files in
   `<build dir>/.cmake/api/v1/reply/`.
3. After the configure finishes, the panel reads the newest `index-*.json`,
   follows it to the `codemodel` v2 object and lists its buildable targets,
   sorted and deduplicated across configurations.

The query must be in the build directory *before* configuring, but CMake has no
command that prints a preset's build directory. So `presets.rs` resolves
`binaryDir` the way CMake does: it loads both presets files and their
`include`s, follows `inherits` depth first (earlier parents win), and expands
the macros `${sourceDir}`, `${sourceParentDir}`, `${sourceDirName}`,
`${presetName}`, `${generator}`, `${hostSystemName}`, `${fileDir}`,
`${dollar}`, `${pathListSep}`, `$env{}` (including a preset's `environment`)
and `$penv{}`. Relative paths are relative to the project root.

Targets are also loaded at startup and whenever presets reload, from whatever
reply is already in the selected preset's build directory. For executable
targets the panel also keeps the path of the built program (the target's first
artifact), which Run and Debug use.

The target menu starts with **All Targets**, which builds with
`cmake --build` and no `--target`, so CMake builds the project's default
target (`all`, or `ALL_BUILD` for Visual Studio and Xcode). Below it, targets
with a `FOLDER` property are shown in submenus, grouped like Visual Studio and
Xcode group them (unless `use_folders` is off). `/` separates nested folders,
so `GUI/Dialogs` is a `Dialogs` submenu inside a `GUI` submenu. Each menu lists
its folders first, then its targets, each sorted by name.

The selection follows what
[CMake Tools](https://github.com/microsoft/vscode-cmake-tools) does: All
Targets is the default, and a selected target that no longer exists after a
configure falls back to All Targets. While there are no targets yet (the build
directory isn't configured), the selection is kept.

### Saved selections

The selected preset, target and action are saved per workspace in Zed's
key-value database, the same way the built-in panels save their state, and
restored when the workspace is reopened. Presets are listed only after the
saved state is loaded, so the saved preset is picked instead of the first one.

### Build, Run and Debug

The split button below the target works like the git panel's "Stage All"
button. The left part runs the current action, and the chevron opens a menu
that switches the action between **Build**, **Run** and **Debug**. Picking an
action from the menu only switches it; it doesn't run it.

Every action starts with
`cmake --build <build dir> --target <target>` (without `--target` for All
Targets) in a "CMake: Build" terminal tab
(quoted like the configure command). The button shows "Building..." until it's
done. Then:

- **Build** is finished.
- **Run** starts the program in a "CMake: Run" terminal tab, with the
  program's directory as the working directory. Running again replaces a still
  running instance.
- **Debug** starts a debug session with Zed's CodeLLDB adapter, like a
  `debug.json` entry with `"request": "launch"`, `"program"` and `"cwd"`.

Run and Debug only continue if the build succeeds. They're unavailable for
targets that aren't executables. With All Targets selected, they start the
project's only executable. If there's more than one, the target menu opens
instead so you can pick one, and pressing the button again builds and runs it. Errors, such as a failed build, are shown
under the button.

The **Stop** button next to it kills the running build, or, when no build is
running, the program started by Run. Stopping a build skips Run or Debug and
shows no error, but issues printed before the stop still reach the Problems
panel. Debug sessions are stopped from the debugger as usual. To make this
possible, builds and programs are started through the terminal panel's
`spawn_task`, which returns the terminal, rather than through
`Workspace::spawn_in_terminal`, which only returns the exit status.

### Build issues in the Problems panel

After the build finishes, the panel reads the build terminal's text and picks
out GCC and Clang style lines (`path:line[:column]: error|warning|fatal error:
message`). It resolves relative paths against the build directory and drops
duplicates, since a problem in a header is reported once per file that includes
it.

Zed only accepts diagnostics from language servers, so the panel publishes the
issues as if they came from a language server of its own. It uses
`LspStore::merge_lsp_diagnostics` with a reserved server id far above the ids
Zed hands out, so restarting clangd never clears them. Their source is "build",
and they're marked as describing the files on disk. Starting a new build
clears the previous build's issues, and files that no longer have issues are
cleared explicitly. Files outside the project, such as system headers, are
skipped.

### State and views

All state and logic lives in one `CMakeProject` entity per workspace
(`cmake_project.rs`). The panel and the title bar are both views of it: they
observe it, render its menus and buttons, and call it to change selections or
run actions. Each view has its own `Controls` (`controls.rs`), which holds the
popup handles, so the panel's and the title bar's popups open independently.
When Run or Debug needs a target to be picked, the target menu of the view
whose button was clicked opens.

### UI

- The title bar shows the preset menu, the target menu, the action split button
  and Stop, in that order, after the project name and branch and a vertical
  divider, without labels or error messages. The buttons are
  borderless with small text, like the title bar's own. They're hidden for
  projects without a presets file and for remote projects. Errors are only
  shown in the panel.
- The panel has a "CMake" header and a section per selector, with a muted
  label above each.
- Each selector is a bordered button, styled like the git panel's buttons. Its
  chevron flips and it's tinted while its popup is open.
- The preset popup is a searchable list (`ItemSelector`, built on Zed's
  `Picker`). Its rows mimic Zed's context menus: a checkmark column and plain
  labels. Type to filter, arrow keys and Enter to pick, Escape to close.
- The target popup is a Zed context menu, because only context menus have
  submenus. Folders open on hover or with the right arrow key. It has no
  search.
- Text is one size step larger than Zed's panel defaults, by request.

## Files

| File | Contents |
|---|---|
| `src/cmake_panel.rs` | Crate root: `init()`, the panel, and the title bar view |
| `src/cmake_project.rs` | `CMakeProject`: settings, saved selections, presets, targets, configure/build/run terminal tasks, the debug scenario, and Stop |
| `src/controls.rs` | The preset and target menus, the action split button and Stop, in panel and title bar styles, plus the `ItemSelector` popup |
| `src/presets.rs` | Listing presets via cmake, and resolving a preset's `binaryDir` |
| `src/file_api.rs` | Writing the codemodel query, and reading targets, their executables and folders from the reply |
| `src/build_issues.rs` | Parsing compiler output and publishing it to the Problems panel |

## Known limitations

- The dock side (left or right) isn't saved across restarts.
- Only the first project folder with a presets file is used.
- Editing a file pulled in through `include` doesn't reload the preset list;
  only the two top-level presets files do.
- Configures run outside the panel (for example typed into a terminal) don't
  refresh the targets until presets reload or a preset is selected again. They
  also only produce a reply if the panel has already written its query into
  that build directory.
- A preset without `binaryDir` (allowed since presets version 3) has no known
  build directory, so its targets can't be listed. `$vendor{}` macros in
  `binaryDir` aren't supported.
- Target folders are shown even when the `USE_FOLDERS` global property is
  off. The file API reports `FOLDER` either way and has no way to read global
  properties. `USE_FOLDERS` is on by default since CMake 3.26 (policy CMP0143).
  Turn folders off with the `use_folders` setting instead.
- Visual Studio and Xcode generators also list their helper targets such as
  `ALL_BUILD` and `ZERO_CHECK`.
- Builds don't pass `--config`, so multi-config generators (Xcode, Ninja
  Multi-Config, Visual Studio) build their default configuration, usually
  Debug. Run and Debug use the program path of the first configuration CMake
  reports, which is usually the same one.
- Only GCC/Clang style issues are recognized. `note:` lines and linker errors
  without a file location (such as undefined symbols) don't appear in the
  Problems panel; they're still in the terminal. Issue columns are taken as
  printed, which matches the editor for ASCII lines.
- Issues are read from the terminal's scrollback, which keeps the last 10,000
  lines by default. Ninja only prints the output of failing commands, so this
  rarely matters.
- Stop doesn't cancel a configure; stop it in its terminal tab.

## Developing

```sh
cargo run                        # build and launch Zed with the panel
cargo test -p cmake_panel        # preset parsing, binaryDir resolution, file API, issue parsing and publishing
./script/clippy -p cmake_panel   # lint (the repo asks for this instead of cargo clippy)
```

A GPUI trap that already caused one crash: an entity can't be read while it's
being updated. `TerminalPanel::spawn_task` reads the workspace, so calling it
inside `workspace.update(...)` panics. Look up the panel with
`workspace.read_with(...)` first, then call `spawn_task` on the panel.
`Workspace::spawn_in_terminal` and `start_debug_session` are safe inside a
workspace update because they defer their work.

Local build environment notes (macOS):

- **Metal Toolchain.** Xcode 26 no longer bundles the Metal shader compiler,
  and GPUI's build fails without it. Install it with
  `xcodebuild -downloadComponent MetalToolchain`.
- **"Verifying Zed…" on every launch.** Each build is a new binary, so macOS
  scans it before running. Adding the app you launch from (VS Code, Terminal)
  under System Settings → Privacy & Security → Developer Tools skips this.
- **VS Code.** `.vscode/launch.json` (gitignored) has a "Debug Zed" config
  using the CodeLLDB extension: F5 builds and launches under the debugger.
