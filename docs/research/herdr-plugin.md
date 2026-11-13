# Herdr Plugin System Research (v0.9.5)

Verified against Herdr CLI 0.9.5 (`herdr api schema --json`, Protocol 22, Schema v1) and official docs ([herdr.dev](https://herdr.dev), [herdr.dev/plugins](https://herdr.dev/plugins)).

## 1. Plugin Manifest (`herdr-plugin.toml`)

Herdr plugins are directories containing a `herdr-plugin.toml` manifest at the package root (or `--subdir`).

### Top-Level Fields
* `id` (string, required): Unique identifier (`^[A-Za-z0-9:._-]+$`, <= 120 chars), e.g. `"herdmesh"`.
* `name` (string, required): Display title in UI and plugin lists.
* `version` (string, required): Semantic version string, e.g. `"0.1.0"`.
* `min_herdr_version` (string, required): Minimum compatible Herdr version (e.g. `"0.9.5"`).
* `description` (string, optional): Summary of what the plugin does.
* `platforms` (array of string, optional): `["linux", "macos", "windows"]`. Cannot be empty; omit if undeclared.

### Sections & Arrays of Tables
* `[[panes]]`: Custom terminal pane entrypoints.
  * Fields: `id` (req), `title` (req), `command` (argv array, req), `placement` (`"overlay"|"popup"|"split"|"tab"|"zoomed"`, default `"overlay"`), `width` / `height` (cells or percentage like `"85%"`, popup only), `description` (opt), `platforms` (opt).
* `[[actions]]`: Palette commands invokable in Herdr.
  * Fields: `id` (req), `title` (req), `command` (argv array, req), `contexts` (array of `"global"|"workspace"|"tab"|"pane"|"selection"`, opt), `description` (opt), `platforms` (opt).
* `[[events]]`: Event hooks triggered on Herdr state changes.
  * Fields: `on` (string, event hook name like `"pane.agent_status_changed"`, req), `command` (argv array, req), `platforms` (opt).
* `[[startup]]`: Commands executed automatically on Herdr boot or plugin load.
  * Fields: `command` (argv array, req), `platforms` (opt).
* `[[build]]`: Build commands executed sequentially on remote installation (`herdr plugin install`). Skipped on `link`.
  * Fields: `command` (argv array, req), `platforms` (opt).
* `[[link_handlers]]`: URL pattern handlers intercepting terminal link clicks.
  * Fields: `id` (req), `title` (req), `pattern` (regex, req), `action` (action ID, req), `platforms` (opt).

```toml
# Example herdr-plugin.toml for herdmesh
id = "herdmesh"
name = "Herdmesh"
version = "0.1.0"
min_herdr_version = "0.9.5"
description = "Multi-agent shared context room and status dashboard"
platforms = ["linux", "macos", "windows"]

[[panes]]
id = "dashboard"
title = "Herdmesh Room"
placement = "popup"
width = "85%"
height = 24
command = ["herdmesh", "room", "dashboard"]

[[actions]]
id = "open-room"
title = "Herdmesh: Open Room Dashboard"
contexts = ["workspace", "pane"]
command = ["herdr", "plugin", "pane", "open", "--plugin", "herdmesh", "--entrypoint", "dashboard"]

[[events]]
on = "pane.agent_status_changed"
command = ["herdmesh", "hook", "agent-status"]
```

## 2. Running a TUI Binary in a Plugin Pane

* **Command**: `herdr plugin pane open --plugin <plugin_id> --entrypoint <entrypoint_id> [options]`
  * Options: `--placement <overlay|popup|split|tab|zoomed>`, `--width <SIZE>`, `--height <SIZE>`, `--workspace <ID>`, `--target-pane <PANE>`, `--direction <right|down>`, `--cwd <PATH>`, `--env <KEY=VALUE>`, `--focus|--no-focus`.
* **Socket API**: Method `plugin.pane.open` accepting `PluginPaneOpenParams`.
* **Execution**: Herdr allocates a full interactive pseudo-terminal (PTY) actor running the `command` argv. ANSI escapes, colors, keyboard shortcuts, mouse events, and alternate-screen buffers work identically to native panes.
* **Injected Environment**:
  * `HERDR_PLUGIN_ENTRYPOINT_ID`: Entrypoint ID (e.g. `"dashboard"`).
  * `HERDR_PLUGIN_CONTEXT_JSON`: JSON with `workspace_id`, `workspace_cwd`, `tab_id`, `focused_pane_id`, `focused_pane_agent`, `focused_pane_status`, `worktree`.
  * `HERDR_WORKSPACE_ID`: Active workspace ID.
  * `HERDR_BIN_PATH`: Path to host `herdr.exe`.

## 3. Remote Installation (`herdr plugin install`)

* **CLI Syntax**: `herdr plugin install <owner>/<repo>[/subdir...] [--ref REF] [--yes]`
* **Resolution**: Clones `https://github.com/<owner>/<repo>` via `git`. Optional `--ref <REF>` checks out specific branch/tag/commit.
* **Expected Files**: `herdr-plugin.toml` must exist at repo root (or specified `/subdir`).
* **Storage Location**: Stored in Herdr-managed cache (`AppData\Roaming\herdr\plugins` on Windows), recorded with `managed_path` in `plugins.json`.
* **Build Execution**: In interactive mode, displays a security trust preview showing manifest actions and build commands (bypass with `-y, --yes`). Runs declared `[[build]]` commands under user privileges in the checkout directory.
* **Publishing**: Any public GitHub repo with topic `herdr-plugin` containing `herdr-plugin.toml` is auto-indexed on `herdr.dev/plugins`.

## 4. Event Subscriptions & Payload Format

* **Hook Declaration**: `[[events]]` in `herdr-plugin.toml` with `on = "<event_name>"` and `command = [...]`.
* **Key Lifecycle Events**:
  * `pane.agent_status_changed`: Dispatched when an agent changes state (`idle`, `working`, `blocked`, `done`, `unknown`).
  * Other events: `workspace_created`, `workspace_closed`, `workspace_updated`, `tab_created`, `pane_opened`.
* **Delivery**: Executed as asynchronous child processes. Output is logged in `herdr plugin log list [--plugin ID] [--limit N]`.
* **Payload Variables**:
  * `HERDR_PLUGIN_EVENT`: Name of triggering event (e.g. `"pane.agent_status_changed"`).
  * `HERDR_PLUGIN_EVENT_JSON`: JSON payload object:
    ```json
    {
      "event": "pane.agent_status_changed",
      "data": {
        "pane_id": "w3:p2",
        "workspace_id": "w3",
        "tab_id": "w3:t1",
        "agent": "agy",
        "agent_status": "working",
        "previous_status": "idle",
        "cwd": "E:\\Codes\\hm-wt-agy"
      }
    }
    ```
  * `HERDR_WORKSPACE_ID`: Originating workspace ID.
* **Socket Alternative**: Real-time event streaming via local socket API (`herdr.sock`).

## 5. Local Development (`herdr plugin link`)

* **CLI Syntax**: `herdr plugin link <path> [--disabled] [--enabled]`
* **Workflow**: Run `herdr plugin link .` pointing to the directory containing `herdr-plugin.toml`.
* **Validation**: Immediately parses manifest, confirms Herdr version >= `min_herdr_version`, validates schema.
* **Builds**: `[[build]]` commands are explicitly SKIPPED during `link` for instant iteration.
* **Registry & Runtime**: Recorded in `plugins.json` as `kind: "local"`. Commands execute with working directory set to plugin root; code changes take effect immediately on next run without re-linking.
* **Unlink / Disable**: `herdr plugin unlink <plugin_id>` unlinks; `herdr plugin disable <plugin_id>` disables.
* **Isolated Config**: `herdr plugin config-dir <plugin_id>` outputs `AppData\Roaming\herdr\plugins\config\<plugin_id>`.

## 6. Exact CLI Output Verification (Herdr 0.9.5)

```text
$ herdr plugin
herdr plugin commands:
  herdr plugin install <owner>/<repo>[/subdir...] [--ref REF] [--yes]
  herdr plugin update [<plugin_id|owner/repo[/subdir...]>...] [--yes]
  herdr plugin uninstall <plugin_id|owner/repo[/subdir...]>
  herdr plugin link <path> [--disabled]
  herdr plugin list [--plugin ID] [--json]
  herdr plugin config-dir <plugin_id>
  herdr plugin unlink <plugin_id>
  herdr plugin enable <plugin_id>
  herdr plugin disable <plugin_id>
  herdr plugin action <list|invoke>
  herdr plugin log list [--plugin ID] [--limit N]
  herdr plugin pane <open|focus|close>

$ herdr plugin pane
herdr plugin pane commands:
  herdr plugin pane open --plugin ID --entrypoint ID [--placement overlay|popup|split|tab|zoomed] [--width SIZE] [--height SIZE] [--workspace ID] [--target-pane PANE] [--direction right|down] [--cwd PATH] [--env KEY=VALUE] [--focus|--no-focus]

$ herdr plugin config-dir herdmesh
%APPDATA%\herdr\plugins\config\herdmesh
```
