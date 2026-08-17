# Resource Monitor

**Website: [resourcemonitor.app](https://resourcemonitor.app)** — download the
signed installer there.

A lightweight Windows system-tray resource monitor written in Rust. Two
binaries, no runtime dependencies: `resmon.exe` (~740 KB) and the MCP shim
`resmon-mcp.exe` (~365 KB), both statically linked against the Win32 API.

Idle cost is kept low by sampling only what is on screen: per-process,
per-core, GPU and audio sampling run only while the panel is open — or while
an alert rule or an AI query needs them. With the panel closed the sampler
reads a handful of system counters at the chosen interval.

## Features

- **Live tray icons, up to six**: app icon, CPU %, RAM %, disk activity,
  network speed and FPS. CPU and RAM are on by default; the set is chosen in
  settings. Each icon is redrawn only when its own value changes, and they all
  share a hover tooltip (CPU, RAM, download/upload, plus "N fps in <app>" when
  something is presenting).
- Click a tray icon → popup panel with current values and 60-sample
  sparklines for CPU, RAM, GPU, FPS, disk, network and sound, plus a used/free
  bar per fixed drive.
- Click any metric row → top apps by that metric, aggregated per process name,
  with a filter box, mouse-wheel scrolling, a pause toggle that freezes the
  list so fast-moving rows can be clicked, and an × per row to end every
  process with that name (with a confirmation dialog). The CPU drill-down adds
  a per-core bar grid.
- **FPS counter**: PresentMon-style ETW tracing of DXGI Present events shows
  the frame rate of whatever game/3D app is currently presenting. Clicking the
  FPS row lists every presenting app and its frame rate.
- Per-app network usage via the `Microsoft-Windows-Kernel-Network` ETW
  provider; per-app GPU via PDH "GPU Engine" counters; per-app sound via the
  Core Audio session API (the same source as the volume mixer).
- **Live connections**: "endpoints" in the network drill-down (or the network
  row of any watched app) lists every open connection with the app that owns
  it, the remote address and port, and the hostname it resolved from —
  `Microsoft-Windows-DNS-Client` ETW events supply names *with the process
  that asked*, which a machine-wide DNS cache dump cannot. Names fall back to
  reverse lookups, and a row with no name says so rather than guessing.
  Enumeration only runs while the list is open, an alert rule needs it, or an
  AI query asks — closed, it costs nothing.
- Right-click tray icon → Settings, "Start with Windows" (elevated Scheduled
  Task, so no UAC prompt at logon) and Exit.

## More features

- **Settings** (gear in the panel) is a menu of five pages:
  - *General* — start with Windows, update rate (0.5 s / 1 s / 2 s), theme
    (dark / black / light), text size.
  - *AI tools* — the MCP connection, notification expectations, agent history.
  - *Main panel* — which metric rows show, and in what order.
  - *Desktop extras* — taskbar widget, FPS overlay, tray icons.
  - *Alerts* — the rule list and the new-alert builder.

  Everything is stored in `%LOCALAPPDATA%\resmon.ini`.
- **The panel stays open**: it does not close when you click away. Dismiss it
  with the × at the top right, Esc, or by clicking the tray icon again. Esc
  steps back one level from a drill-down or settings page.
- **Pin mode**: "pin" turns the flyout into a real, resizable app window
  with a taskbar button (which carries its own close button, so the in-panel ×
  is hidden); "top" toggles always-on-top, which only means anything once
  pinned. Size and position persist, as does where the unpinned flyout was
  last dragged to.
- **Reorderable main panel** — drag ≡ to reorder the CPU / RAM / GPU / FPS /
  disk / net / sound rows, or untick one to hide it.
- **Process watch**: click an app's name to see its CPU / RAM / GPU / disk /
  network (and FPS, when it is presenting) together with history sparklines;
  "close app" from there or via the × on any row. The subprocess breakdown
  (collapsed by default — a browser can be 70+ processes) has its own filter
  box and metric chips: pick cpu / ram / gpu / disk / net / sound and the list
  re-sorts and shows that column, so "which one is making that noise" is one
  click.
- **Alerts** — user-defined rules managed entirely in the settings GUI:
  per-rule enable/disable and delete, and a "+ new alert" builder (metric,
  above/below, threshold, delivery, include-top-apps, cooldown). Each alert
  chooses its own delivery — desktop notification, a log file, or both — so
  there is no global switch that can silently override a rule.
  Metrics: cpu/ram/gpu %, disk/net MB/s, fps, sound %, or a specific process's
  cpu/ram/disk/net/sound. Cooldowns are 30 s / 60 s / 5 min, and
  "include top apps" appends the top five processes by CPU to each log line.
  Rules are stored as `logN=` lines in `resmon.ini` (editable by hand too;
  changes there apply after restart, GUI changes apply instantly).

  A rule can also watch **connections** rather than a number: pick hostname,
  remote IP, port or app, and give it a pattern —

  ```ini
  log3=conn:host=*.asus.com; cooldown=300
  log4=conn:port=445; file=C:\logs\smb.log
  log5=conn:ip=204.79.
  log6=conn:proc=mscopilot.exe
  ```

  These fire on either signal: a connection that was not open on the previous
  tick, or a DNS lookup matching the pattern. Both are needed — polling alone
  misses a beacon that opens and closes between two ticks, and DNS alone is
  blind to hardcoded addresses. An armed connection rule is what keeps the
  sweep running while the panel is closed.
- **FPS overlay** — a floating, draggable frame counter with five colors and
  three opacity levels (settings → Desktop extras). Shows over borderless and
  windowed games; exclusive-fullscreen games bypass desktop overlays.
- **App finder on the main panel** — type in "Find app" to jump straight to
  any app's full overview (CPU/RAM/GPU/disk/net with history).
- **Taskbar widget** — an always-visible strip of live metrics (choose any of
  CPU/RAM/GPU/FPS/disk/net plus an AI chip showing running agents and waiting
  messages); drag it anywhere, resize it by its bottom-right corner, give it
  its own theme, or "move next to the clock" to sit on the taskbar like a
  native widget.
- **Themes** — dark / black (OLED) / light, switchable in settings, with a
  separate theme for the widget.
- **Text size** — small / default / large / larger (settings → General).
  Multiplies whatever scaling Windows already applies, and grows the panel
  with the text rather than cramming bigger text into the same box.
- Real per-core CPU or GPU temperatures are not shown: they require a kernel
  driver, which this app deliberately does not ship.
- Ships with an embedded app icon and a Common Controls v6 manifest.

## MCP server (Claude Code integration)

`resmon-mcp.exe` (installed alongside the app) is an MCP stdio server that
lets Claude Code and other AI agents query the running monitor. Settings →
*AI tools* shows the exact command for your install directory with a Copy
button:

```sh
claude mcp add resourcemonitor "C:\Program Files\Resource Monitor\resmon-mcp.exe"
```

Tools: `system_snapshot`, `top_processes(metric, limit)`, `app_detail(name)`,
`network_connections(...)` (below), `history` (the last ~360 samples — about
6 minutes at the default rate), `fps_status`, `notify(title, message)` —
agents can pop a tray notification when a long build or task finishes — plus
`notify_rules` and `report_agents` (below). Reads never change the system; the
only writes are what an AI tells the app about itself. The tray app must be
running, and "Allow AI tools to connect" must be on.

`network_connections` answers "what is this machine talking to, and which app
is doing it": pid, image name, remote address and port, TCP state, and the
hostname the address resolved from. Every filter is optional and they combine
— `process`, `pid`, `remote_ip` (exact or a prefix like `204.79.`), `host`
(substring or `*` wildcard), `port`, `state`, `scope`, `limit`. It defaults to
established connections to public addresses, because unfiltered the table is
mostly loopback and listeners.

Each row says where its name came from in `name_source`: `dns_event` when the
app was seen resolving it, `reverse` for a PTR lookup, or `null` when the name
is simply unknown — a browser resolving over DNS-over-HTTPS bypasses the
Windows resolver, so its rows are often unnamed. The app reports what it
observed and leaves the judgement of whether a connection belongs there to you
or to the assistant reading it; it ships no list of "known" endpoints.

The transport is a local named pipe (`\\.\pipe\resmon-mcp`), plain-text
commands in, one line of JSON out. Because the app runs elevated, the pipe
carries an explicit ACL: this user, SYSTEM and Administrators only, with
network logons denied so it cannot be reached over SMB.

Messages sent with `notify` pop a tray balloon and are also kept in a
**Messages** list in the panel; clicking the balloon opens the panel on that
message.

### Telling AI tools what you expect

Settings → *AI tools* → **NOTIFY ME WHEN**: four presets (a build or long task
finishes / something errors / it needs your input / every step of note), plus
anything you type under **CUSTOM AI REQUESTS**. Typed instructions are added
to the same list with "add", each with its own tick box and an × to remove it,
so one can be silenced without being deleted. All the ticked entries compose
into an instruction string handed to clients two ways:

- in the MCP `initialize` response's `instructions` field, so a connecting
  assistant receives them without being asked;
- from the `notify_rules` tool, for assistants that connected before you
  edited them.

Already-connected clients keep the old copy until they reconnect or call
`notify_rules`. This is guidance, not enforcement — the app cannot make an
assistant call `notify`.

### AI activity

`report_agents(agents[])` lets an assistant say what it and its sub-agents are
working on; the app lists them under **AI activity** (from the footer bar) with
status, detail and age. Each call replaces the whole list *for that session*,
so anything omitted is treated as finished and nothing needs an explicit "end"
call. One session is one `resmon-mcp.exe` process, labelled with the client
name and the folder it was launched in, so two assistants cannot overwrite each
other. Entries that stop being refreshed are marked stale after 5 minutes and
dropped after 30, because an assistant that crashed will never report its own
death.

Finished agents move to a Finished list (the last 200), and can optionally be
appended to a file of your choosing (settings → *AI tools* → AGENT HISTORY),
which is the only way they survive a restart.

Nothing here is measured: it is whatever the assistant claims. The app's own
per-process figures are separate — an assistant's reported activity is never
mixed in with what the sampler observed.

## Elevation

The FPS counter, per-app network and connection host names need an ETW
real-time session, which requires running as Administrator (or membership in
Performance Log Users). Unelevated, everything else still works and those
readouts degrade to a "needs administrator" hint — the connection list still
enumerates with full per-process attribution, but only reverse lookups can put
names to addresses.

## Building

### On Windows

Install the [MSVC Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
(or Visual Studio with the "Desktop development with C++" workload) and a
Rust toolchain via [rustup](https://rustup.rs), then:

```powershell
cargo build --release
# → target\release\resmon.exe, target\release\resmon-mcp.exe
```

`x86_64-pc-windows-msvc` is the default target on Windows, so no extra
target needs adding.

### Cross-compiling from Linux

The app is Windows-native; this is only for developing on a Linux machine:

```sh
sudo apt-get install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
# → target/x86_64-pc-windows-gnu/release/resmon.exe
```

Either build produces a working `resmon.exe`. Run it on Windows (right-click
→ Run as administrator, or install the autostart task which runs it elevated
at logon).

### Installer

The NSIS script builds from the `x86_64-pc-windows-gnu` output and works with
Linux `makensis`:

```sh
sudo apt-get install nsis
python3 tools/gen_icon.py     # regenerates assets/app.ico
makensis installer/resmon.nsi
# → dist/ResourceMonitorSetup.exe
```

It installs both exes into `%PROGRAMFILES%\Resource Monitor`, with optional
desktop shortcut and autostart-task components. Pushing a `v*` tag runs the
same steps in GitHub Actions and attaches the installer to a release, signing
it first once the SignPath secrets are set.

### Tests

```sh
cargo test    # 109 tests, and they run on Linux too
```

The parsing and bookkeeping layers — settings, alert rules, agent tracking,
formatting helpers — are platform-independent on purpose, so they are unit
tested without a Windows host.

## CLI

- `resmon.exe` — run normally (tray app, single instance).
- `resmon.exe --install` — create the "ResourceMonitor" elevated logon task.
- `resmon.exe --uninstall` — remove it.

Errors are appended to `%LOCALAPPDATA%\resmon.log`.
