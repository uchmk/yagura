# yagura

A keyboard-driven system monitor for Windows, macOS and Linux, written in Rust
with [egui](https://github.com/emilk/egui). It is the sibling of
[kura](https://github.com/uchmk/kura) (a yazi-style file manager) and
[tsumugi](https://github.com/uchmk/tsumugi) (a terminal for AI CLI sessions),
and shares their look, their keys and their settings folder.

What it is for: seeing at a glance how the chip is busy. The CPU screen draws
one tile per physical core, grouped by the L3 cache they share (a CCD on
Ryzen, the P- and E-cores on a hybrid Intel chip), with a bar per SMT thread
inside each tile.

> **v0.19** has all ten screens: overview, cpu, mem, gpu·npu, procs, svc, net, disk, sensors and sys. **v0.43** adds devices (`D`): USB and the rest of what Device Manager lists. **v0.48** adds containers (`C`): what Docker or Podman runs.
> **v0.31** adds the fleet: other machines' summaries on one screen, over SSH or Tailscale (see [Fleet](#fleet)); **v0.41** lets it connect directly, with keys ([Direct connections](#direct-connections)).
> **v0.35** lets you enter another machine's screens from its card, and switch machines with `[` / `]`.
> What is still missing on each OS is in [TODO.md](TODO.md).

## What it looks like

![The cpu screen: the core tiles of a 16-core Ryzen in its two L3 groups, each tile coloured by how busy its core is, the total over the last minute and a heatmap of all 32 threads](docs/images/cpu.png)

![Going through the screens with the number keys: cpu, a row down with j, memory, GPUs and NPUs, drives and volumes, sensors, and back to the overview](docs/images/yagura.gif)

![The overview: CPU, memory, swap, GPU and drive gauges, each with its graph, and the top processes by CPU and by memory](docs/images/overview.png)

## Screens

| Key | Screen   | What it shows |
| --- | -------- | ------------- |
| `1` | overview | CPU / memory / swap gauges, then one per GPU and NPU (its usage) and per drive (how busy it is), and the system with what each device is; a graph for every gauge, laid out evenly (two columns of three for CPU, memory, swap, two GPUs and a drive; clicking one selects its gauge); the top processes by CPU and by memory |
| `2` | cpu      | Groups and counts; the core tiles (grown to fill their panel in balanced rows, each at most twice as wide as tall), the total over the last minute and a thread heatmap, a third of the column each; the selected core's threads and the top CPU users |
| `3` | mem      | Used / available / swap; memory and swap stacked, each a bar and its last minute (swap only when there is some); the processes using the most memory |
| `4` | gpu·npu  | The GPUs and NPUs found, with their usage now (and why a missing one is missing); the selected device's usage, engines and memory (dedicated and shared, with the VRAM graph) stacked; its temperature, fan, power and clock, the reason for any it cannot read, and the processes using it (`Enter` opens one in procs) |
| `5` | procs    | Scope and sort; the process list (filter with `/`, sort with `s`, a tree of parents and children with `t`, only your own with `u`, end one with `x`, step its priority with `}` / `{`, choose the CPUs it may run on with `A`); the selected process's details (its start in local time; its priority (nice, or Windows' priority class), the CPUs it may run on, how many files or handles it holds open and how many sockets, read for that one process every 5 s and only for this machine's processes) and graphs. On Windows without administrator, other users' processes get their user and memory from `WTSEnumerateProcessesExW`, as in Task Manager |
| `6` | svc      | Services: Windows' services, systemd's units, launchd's jobs (with their plists, so disabled jobs show too). Show all / running / stopped / failed, with counts by start type; the list (state, name, start, PID; filter with `/`; on Windows `u` switches it to the kernel and file-system drivers and back); the selected one's description, state, start, PID, last exit, account, command and file (`Enter` opens its process in procs). `S` starts, `x` stops and `R` restarts the selected one after asking; the question shows the same thing as a command line (`systemctl restart foo.service`, `launchctl kickstart -k gui/501/…`, `Restart-Service foo`, or `sc.exe stop foo` for a driver). Read every 5 s, and only while the screen shows |
| `7` | net      | The network interfaces with their rates (received plus sent), and counts of the sockets; in the middle, a row per interface that is up (loopback included where the OS lists it) with its received and sent graphs over the last minute side by side, above every TCP and UDP socket (protocol, local and remote address, state, PID, process; it scrolls with the wheel, `j` / `k`, `PageDown` / `PageUp`; filter with `/`, only the listening ones with `u`; `Enter` on an interface keeps only the sockets on its addresses and those bound to any address, until `Esc`); on the right, the selected interface's details (name, adapter, kind, link speed, addresses, MAC, MTU, totals) and under them the selected socket (`Enter` opens its process in procs). Interfaces are read every interval; sockets every 2 s, and only while the screen shows |
| `8` | disk     | The drives with their traffic now, and the totals of all of them; the selected drive's read, write and busy graphs stacked, each the column's width, over every mounted volume (mount point, device (on Windows its label), file system, size, used, free, use % turning yellow over 75 % and red over 90 %, read and write per second); the selected drive's SMART health (green when good, red when the drive predicts its failure, and why when it cannot be read), temperature with the drive's warning mark, what SMART counts (life used, turning yellow from 80 % and red at 100 %; hours powered on; lifetime writes; remapped, pending and media errors, yellow from the first), model, kind (SSD / HDD), bus, size, read and write rates, totals since boot (Linux and macOS; Windows keeps none) and volumes, and the selected volume with a bar of how full it is. `Enter` on a drive goes to its first volume, on a volume back to its drive. Read every interval |
| `9` | sensors  | The chips with sensors (the CPU, the drives, the board's monitor chip, the GPU), each with its hottest temperature over the last minute, and the hottest of all; every temperature and fan (chip, sensor, now, the lowest and highest of the last minute, the chip's high mark and critical point; a temperature turns yellow within 10 °C of its high mark and red past it, a fan below its least turns red); the selected sensor's graph with its marks, and where it was read. Moving through the chips moves the sensors to each chip's first, and the chip follows the sensor; `Enter` on a chip goes to its first sensor, on a sensor back to its chip. On Windows with no ACPI thermal zones, it says so and how HWiNFO or LibreHardwareMonitor would add the temperatures. Read every interval |
| `0` | sys      | What does not change while running: the system's maker and model, the OS, kernel, uptime and boot time, the processor (model, maker, socket, cores and threads, top speed, L1 / L2 / L3 cache), the firmware (maker, version, date, UEFI or legacy boot, a virtual machine's), the board, and the memory (installed, slots used, the most the board takes, error checking), who is logged in and the power plan (read every 15 s), the GPUs and NPUs (name, VRAM) and the drives (model, size), each list with the key of its own screen; every memory slot, filled or empty (size, type, speed, maker); the selected slot's bank, form factor, rated and configured speed, rank, voltage and part number. Read once at start, on a thread of its own. Under the processor, the fleet agent: whether one listens on this machine's port (asked every 5 s while the screen shows), where it takes direct connections and its key, its version, process, what it allows, its interval and how many viewers it has, or, when none runs, the port, what `[agent] allow` would allow and how to start one; inside another machine's screens, that machine's agent |
| `D` | devices  | What Device Manager lists: the classes (USB controllers and hubs, display adapters, network adapters, sound, storage, input, cameras, Bluetooth…) with their counts, red with a `!` while one of their devices has a problem; the class's devices (name, maker, status; a problem device red, a disabled one yellow, one with no driver dim; `t` lays every device out under the one it hangs off, so a hub shows what is plugged into it; filter with `/` by name, maker, driver, id or `VID:PID`); the selected device's class, kind, maker, status (with Windows' problem code and what it means), driver and its version, `VID:PID` or PCI `vendor:device`, USB revision and link speed, location, the devices it hangs off and its id (`y` copies it). Read every 5 s, and only while the screen shows, so a plugged-in stick appears within 5 s. Where each OS reads them from: [Building](#building) |
| `C` | containers | What Docker or Podman runs, read through the engine's own API (no `docker` command needed): the scopes (all, running, paused, stopped, with their counts); the containers (state, name, CPU %, memory, image; filter with `/` by name, image or id); the selected one's id, image, status, creation time, CPU, memory against its limit, process count and the last minute of CPU and memory (`y` copies its id). `S` starts, `x` stops and `R` restarts the selected one after asking, on this machine only; the question shows the same thing as a command line (`docker stop web`). Read every 3 s, and only while the screen shows. It looks at `DOCKER_HOST`, `/var/run/docker.sock`, the rootless and Podman sockets, and on Windows the `docker_engine` and `podman-machine-default` pipes; with none it says so, and with a socket this user may not use it says that. Another machine's containers come from its agent, which must be v0.48 or newer (fleet protocol 7) |
| `F` | fleet    | One card per machine: this one first, then every `[[host]]` in `config.toml`. Each card has a dot (green up, yellow slow, red down, grey connecting), the name, the address and round trip, the system, CPU / memory / GPU gauges with their last minute, the temperature, the battery on a laptop (`87% charging`, `87% plugged in`), the uptime or when it was last seen, the agent's version and what it allows, and what is wrong there (hot sensors, drives SMART warns about, failed services, devices with a problem, containers restarting or exited with an error), yellow, or red when a service failed. A down card is dimmed, keeps its last numbers and says why (nothing listens: the tunnel is not up; the agent closed; another version; not a yagura agent). `h` `j` `k` `l` move between cards, `Enter` on a card shows that machine's screens (this machine's card: back to its own). See [Fleet](#fleet) |

Every screen is three columns. Their widths change with the mouse (drag a
divider) or the keyboard, and are kept per screen in `state.toml`. A column
holding several panels splits evenly among them (three panels, three
thirds); drag the line between two panels to give one more room, or
double-click it to put it halfway along the room it splits, as with the
column dividers. The heights are kept per screen in
`state.toml` too, as long as the screen shows the same panels, and
`Ctrl+Shift+e` puts both the columns and the panels back to equal.

## Keys

| Key | Does |
| --- | ---- |
| `1`–`0`, `D`, `C`, `Tab`, `Shift+Tab` | Go to a screen (`D` devices, `C` containers), next screen, previous screen |
| `j` `k` / `↓` `↑` | Down, up |
| `gg` / `Home`, `G` / `End` | First row, last row |
| `Ctrl+d`, `Ctrl+u`, `PageDown`, `PageUp` | Half a page, a page |
| `h` `l` / `←` `→` | Column to the left, to the right |
| `Alt+Shift+←→↑↓` (macOS `Cmd+Ctrl+arrow`) | Move the focused column's divider (tsumugi's keys) |
| `Ctrl+Shift+e` (macOS `Cmd+Shift+e`) | Columns back to equal widths, and the panels in them to equal heights |
| `Enter` | Open the selection (a gauge's screen, on the device for a GPU, NPU or drive; a process in procs, a service's or a socket's process). On a net interface: only the sockets on it. On disk: a drive's first volume, or a volume's drive. On sensors: a chip's first sensor, or a sensor's chip |
| `/` | Filter processes, or on svc services by name and description, or on net sockets by protocol, address, state, PID and process, or on devices by name, maker, driver, id and `VID:PID`, or on containers by name, image and id (`Enter` keeps it, `Esc` drops it). Each of the five keeps its own |
| `s` | Next sort column |
| `t` | Procs as a tree of parents and children, or a flat list. Siblings keep the sort; a process whose parent is filtered out hangs under its nearest shown ancestor. On devices: every device under the one it hangs off (a USB stick under its hub, the hub under its controller), or the class's list |
| `u` | Procs' scope: every process, or only yours (the same as picking All / Mine in the left column). On net: every socket, or only the listening ones (TCP `LISTEN` and unconnected UDP). On svc (Windows): services, or drivers |
| `x` | End the selected process, after asking (`y` / `Enter` ends it, `n` / `Esc` keeps it). Windows uses `TerminateProcess`; macOS and Linux send SIGTERM, then SIGKILL if it is still there after 3 s. Works on procs and on the top lists of overview, cpu and mem. On svc it stops the selected service instead, and on containers the selected container |
| `S`, `R` | On svc: start, restart the selected service, after asking. On containers: start, restart the selected container, after asking, through the engine's API as the list is read (this machine only; another machine's are view only). A paused container is not started; the toast says the `docker unpause` line. Windows asks the Service Control Manager (run yagura as administrator); Linux runs `systemctl` (root or polkit); macOS runs `launchctl kickstart` / `kill` in the domain the list came from (system jobs need `sudo`); a job that is not loaded is loaded from its plist first (`launchctl bootstrap`), while a disabled one is left disabled and the toast says the `launchctl enable` line. A start on a running service, or a stop on a stopped one, only says so |
| `F` | The fleet screen, and back (`Esc` or a screen key also leave it). On it, `h` `j` `k` `l` and the arrows move between cards, `gg` / `G` to the first and last, `Enter` into the selected machine's screens. `x`, `S`, `R` on another machine's card say what its agent allows (when it allows them: `Enter` and act on its screens; a machine's containers are view only from elsewhere) |
| `]`, `[` | Next, previous machine: this one, then each `[[host]]` that is up. The screen stays the same, so `2` then `]` `]` compares the CPUs of the machines. While another machine shows, the crumb starts with its name and the status bar has a cyan pill with it (red `lost` when its link drops) |
| `P` | Pause / resume sampling |
| `+` / `=`, `-` | Sample less often, more often (0.5–10 s) |
| `y` | Copy: on devices the selected device's id (Windows' instance id, `USB\VID_…\…`), on containers the selected container's id, on fleet's hint the sample `[[host]]` (three lines of TOML, pasted as they are), on another machine's card its address |
| `Ctrl+,` (macOS `Cmd+,`) | The settings screen, and back. See [Settings screen](#settings-screen) |
| `Ctrl+Shift+P` (macOS `Cmd+Shift+P`) | The command palette: type letters of a command, Up/Down or Ctrl+P/N to move, Enter to run, Esc to close. It lists the commands of the `[main]` keymap with their keys |
| `?`, `F1` | Help: every key of the keymap |
| `F12` | Report a bug: opens the GitHub form in the browser with the version, the OS, the scale, the settings warnings and the config files read (by name, never a path) filled in |
| `Ctrl+=` / `Ctrl++` / `Ctrl+;`, `Ctrl+-`, `Ctrl+0` (macOS `Cmd`) | Zoom in, out, back to 100%. The scale is every uchmk app's (common.toml's `scale`), so the others follow; `Ctrl+;` is for JIS keyboards |
| Double click a graph | Open it full size over the last 5 minutes (overview, cpu, mem, gpu, net, disk and sensors). `]` / `Right` / `l` look further back (15 minutes as 5 s means, 1 hour as 30 s means), `[` / `Left` / `h` closer, `Esc`, `q` or `Enter` close. The range is kept for the next one. The longer history is kept for these system-wide series only (not per process) |
| `Esc` | Clear the filter, and on net the interface the sockets are narrowed to |
| `q`, `Ctrl+q` (macOS `Cmd+q`) | Quit |

## Settings

Everything lives in one folder shared by uchmk's apps:

```text
<config dir>/uchmk/common.toml         settings every uchmk app reads
<config dir>/uchmk/yagura/config.toml  yagura's own settings
<config dir>/uchmk/yagura/keymap.toml  laid over the built-in keymap
```

`<config dir>` is `%APPDATA%` on Windows, `~/Library/Application Support` on
macOS and `~/.config` on Linux. `UCHMK_CONFIG_DIR` moves the whole `uchmk`
folder, `YAGURA_CONFIG_HOME` yagura's own. yagura was called mimamori before
v0.33.0; its first start moves `uchmk/mimamori/` to `uchmk/yagura/` when only
the old folder is there.

The column widths and panel heights yagura writes for itself go to `state.toml` in the state
folder: `%LOCALAPPDATA%\uchmk\yagura` on Windows,
`~/Library/Application Support/uchmk/yagura/state` on macOS and
`~/.local/state/uchmk/yagura` on Linux (`$XDG_STATE_HOME`), or
`YAGURA_STATE_HOME`. Before v0.39.0 it was in `uchmk/yagura/`; the first
start moves it.

The window's position, size and maximized state go to `window.toml` in the same
folder (the file kura and tsumugi use too, one per app), written once the window
has stayed put for a moment and again on exit, and put back at the next start,
on a second screen at another scale too. A place no screen could show is
ignored, and so is any under `--keys`.

`common.toml` is shared: change it and every uchmk app (tsumugi, yagura,
and kura to come) follows within 2 s, with no restart:

```toml
language = "auto"            # "auto" follows the OS; "en" or "ja"
theme = "dark"               # a theme's name, or "dark" / "light" / "system"
dark_theme = "tsumugi Dark"  # what "dark", and "system" on a dark OS, use
light_theme = "tsumugi Light"

[clock]                      # the date and time at the right of the status bar
show = true
hour24 = true
date = true
date_format = "YYYY/MM/DD"   # or "YYYY-MM-DD", "MM/DD", "DD/MM", "MM/DD/YYYY", "DD.MM.YYYY"
weekday = true               # "(Sat)", or "(土)" in Japanese
```

The 20 built-in themes are tsumugi's: tsumugi Dark and Light, Tokyo Night and
Tokyo Night Day, Catppuccin Mocha and Latte, Dracula, Nord, Gruvbox Dark and
Light, Solarized Dark and Light, One Dark and One Light, Rosé Pine and Rosé Pine
Dawn, Campbell, VS Code Dark+ and Light+, and GitHub Light. A theme of
your own is a TOML file in `<config dir>/uchmk/themes/`, in the form of
tsumugi's themes; it shows in every app's list. The rules every uchmk app
keeps (where the files are, the settings screen, the clock) are ito's
[docs/common-spec.md](https://github.com/uchmk/ito/blob/main/docs/common-spec.md).

`config.toml`:

```toml
interval = 1.0      # seconds between samples, 0.5 to 10
check_updates = true # ask GitHub once at start whether a newer yagura is out
language = "en"     # overrides common.toml for yagura alone
backend = "auto"    # graphics: "auto", "gl", "vulkan", "dx12", "metal"

[agent]             # for `yagura --agent` on this machine
port = 7313         # listens on 127.0.0.1 only
allow = []          # "kill", "service": what a fleet viewer may do on this machine (empty: view only)
listen = "0.0.0.0:7314"   # also take direct connections from the viewers below (none when left out)
metrics = "127.0.0.1:9314" # also serve /metrics for Prometheus (plain HTTP; none when left out)

[[agent.viewer]]    # a machine that may watch this one directly, once per machine
name = "laptop"
key = "…"           # its public key, as `yagura --key` prints it there

[[host]]            # a machine on the fleet screen, once per machine
name = "nas"
addr = "localhost:7314"   # host or host:port; the port defaults to 7313, or 7314 with a key
key = "…"           # its agent's public key: connect directly (left out: plain, through a tunnel)
every = 5.0         # a summary every 5 s at most, for a thin link (left out: the agent's interval)
```

`keymap.toml` uses yazi's format, section by section (`[main]`, `[input]`,
`[help]`, `[confirm]`, `[zoom]`, `[settings]`): `keymap` replaces a section, `prepend_keymap` and `append_keymap`
add before and after it. A key sequence is written `"gg"` or `"g g"`, `for`
(`windows`, `macos`, `linux`, `unix`) keeps a binding to one kind of machine,
`on_mac` gives a binding its own keys on macOS, and
`C` is Ctrl on Windows and Linux and Cmd on macOS (where Ctrl works too):
`<C-,>` is Ctrl+, (Cmd+, on a Mac). The built-in
keymap is [src/defaults/keymap.toml](src/defaults/keymap.toml). The file is read
by the crates `ito-keymap` and `ito-keys` in [ito](https://github.com/uchmk/ito), as kura reads
its own, so it reads the same in uchmk's other apps: an unknown section,
field, key or command, a key bound twice, and a key that hides the chords it
starts are named in the warnings, and the rest of the file still counts.

```toml
[main]
prepend_keymap = [
    { on = "<C-r>", run = "pause", desc = "Pause / resume sampling" },
]
```

The settings screen's **Keys** page writes this file for you.

### Settings screen

`Ctrl+,` (`Cmd+,` on macOS) fills the window with the settings, laid out as
tsumugi's: on the left the search, the pages and **Open config.toml**; on the
right the page, its cards and their rows. What is changed is written at once,
keeping the comments in the file, and takes effect at once unless the row
says otherwise.

| Page | Rows |
| ---- | ---- |
| General | Language and scale (common.toml), the update interval, **Check for updates** (`check_updates`, from the next start), the clock card (common.toml) |
| Theme | Follow OS / Light / Dark, the list of themes, and a preview drawn with yagura's own tiles, process rows and status bar (common.toml) |
| Fleet | This machine's agent (allow ending processes, allow controlling services, the port); the `[[host]]` list (name, address, Delete, Add host); the setup lines (`yagura --agent`, the ssh line, the tailscale line, the sample `[[host]]`, config.toml's path); and the direct connections (this machine's public key, the `[[agent.viewer]]` that lets it in, the `[agent] listen` line, the `[[host]]` that watches it directly), each with Copy. The ssh and tailscale lines, the sample and a new host's address follow `[agent] port`; Add host and Delete change only the `[[host]]` tables, keeping config.toml's comments and order |
| Keys | Every operation in a row with its keys, its layer and the default when it was changed; search by name, command or key, **Find by key**, by layer, **Changed only**. **+** takes a new key (a sequence such as `g h` too, `Enter` sets it, `Esc` gives up, **Special keys** adds a bare `Enter` / `Esc` / `Backspace` / `Tab`), **x** takes one off, **Reset** gives the operation its defaults back. Only what was changed is written, to keymap.toml's `prepend_keymap` (a default taken off as `run = "noop"`), and the keys change at once. The same page as in every uchmk app (ito's `keys_page`) |
| Notifications | How a fired alert reaches you (`[notify]`): the system's notification and the sound (switches), the alert's sound with a play button, quiet in focus mode, the webhooks (as many as wanted: each with its address and format, **Remove** and **Send a test here**, then **Add an address**); **Send a test** and **Reset to defaults** (the webhooks stay). The same page as in every uchmk app (ito's `notify_page`) |
| Advanced | The graphics backend (from the next start), the config and state folders (Open), a reset (pressed twice: config.toml and keymap.toml are moved to `*.bak-<time>`, common.toml stays), **Backup** (write common.toml, the shared themes and yagura's `.toml` / `.json` files to one file, or read them back over the current ones, which are kept as `<name>.bak-<time>`; state and the fleet key stay out), and what reading the files warned about |
| About | The version, the OS, the licence, **Copy the details**, and the bug-report button (the same as `F12`) |

The General page also holds the **Window** card (`[window]` in common.toml: remember the window's place, keep it on top, forget the saved place) and the **Startup** card (start yagura when you sign in), the same as in every uchmk app.

Only the `[settings]` section's keys are taken (`Esc` and `Ctrl+,` close, a
second `Esc` when a control has the keys; `Ctrl+Tab` / `Ctrl+PageDown` and
`Ctrl+Shift+Tab` / `Ctrl+PageUp` turn the page; `Ctrl+f` searches); every other
key goes to the controls (`Tab` between them, typing into a field). The search
finds the labels in the screen's language and the names in the files (`key`,
`listen`, `every`, `interval`, `scale`, …) in either language.

## Commands

What `run` can say in a keymap:

| Command | Does |
| ------- | ---- |
| `screen overview` … `screen sys`, `screen devices`, `screen containers`, `screen next`, `screen prev` | Change screen |
| `arrow N`, `arrow top`, `arrow bot`, `arrow 50%` | Move the selection by rows, to an end, by a share of the page |
| `focus left`, `focus right` | Move between columns |
| `resize left` / `right` / `up` / `down` | Move the focused column's divider |
| `equalize` | Columns back to equal widths, and the panels in them to equal heights |
| `enter`, `filter`, `sort`, `pause` | As the keys above |
| `interval +`, `interval -` | Sample less / more often |
| `kill` | Ask, then end the selected process; on svc or containers, stop the selected service or container |
| `priority raise`, `priority lower` | On procs: one step more / less urgent for the selected process, on this machine only, no question (`}` / `{`; Windows: next priority class between Idle and High, elsewhere nice ∓5; raising needs privilege) |
| `affinity` | On procs: choose the CPUs the selected process may run on, typed as `0-3,8` or clicked, on this machine only (`A`; Windows: one processor group, CPUs 0-63; Linux: every thread; not on macOS) |
| `service start`, `service stop`, `service restart` | On svc or containers: ask, then start, stop or restart the selected service or container |
| `tree` | Procs (or devices) as a tree, or a flat list |
| `scope`, `scope all`, `scope mine` | Switch procs' scope, or set it |
| `fleet` | The fleet screen, and back |
| `machine next`, `machine prev` | The next or previous machine's screens (this one, then the `[[host]]` that are up) |
| `copy` | Copy the sample `[[host]]` (fleet's hint) or the card's address |
| `settings` | The settings screen, and back |
| `scale in`, `scale out`, `scale reset` | Step the scale every uchmk app shares (common.toml), or put it back to 100% |
| `page next`, `page prev`, `search` | The settings screen's keys (`[settings]`): turn the page, go to the search |
| `help`, `escape`, `quit` | As the keys above |
| `confirm`, `backspace` | The filter line's keys (`[input]`); `confirm` and `escape` also answer the question before a process is ended or a service or container is started, stopped or restarted (`[confirm]`) |

### Updates and bug reports

At start yagura asks GitHub once, through the system `curl`, whether a newer
release is out, and says so in a toast. It is never asked under `--keys`, and
`check_updates = false` (or the General page's **Check for updates**) turns it
off.

`yagura env` prints what a bug report would otherwise ask for, one question
at a time: the version and OS, which config files were read, the settings
(and what reading them warned about), which helper tools (`curl`, `smartctl`,
`systemctl`, …) are on the PATH, and which variables are set. `yagura env --out
FILE` writes it to a file as UTF-8.

## Export

Two options read the machine without opening a window:

```
yagura --snapshot > now.json                # everything, once, as JSON
yagura --log-metrics usage.csv --every 5    # a row every 5 seconds until Ctrl+C
```

`--snapshot` waits until every source has read (about two seconds) and prints
the system, CPU, memory, processes, GPUs, services, network (interfaces and
sockets), disks, sensors, battery, hardware and devices. `--log-metrics`
appends one row of the main numbers to a CSV (the header is written when the
file is new): UTC time, CPU %, load average, memory and swap, network and disk
bytes per second, the busiest GPU %, the hottest temperature, battery % and
the process count. A number the machine does not give is an empty cell.

## Alerts

`[[alert]]` rules in config.toml watch this machine, and `[notify]` says how a
fired one reaches you. `[notify]` has the same words as tsumugi's (the code
is ito's `ito-notify`), and the settings' Notifications page writes it:

```toml
[notify]
system = true                 # the system's notification (default)
sound = false                 # a sound of the system's
sound_name = "alert"          # chime, low, alert or default
focus_mode = true             # Windows: no sound while focus mode is on

[[notify.webhooks]]           # also send it here; as many as wanted
url = "https://ntfy.sh/my-topic"
format = "ntfy"               # ntfy, slack, discord or json

[[notify.webhooks]]
url = "https://discord.com/api/webhooks/…"
format = "discord"

[[alert]]
metric = "cpu"                # cpu, mem, swap, gpu, temp, disk or battery
above = 90                    # or: below = 15
for = 30                      # seconds it has to hold (default 10)
name = "CPU is busy"          # optional; else "CPU is over 90%"

[[alert]]
metric = "service"            # fires when a service is stopped or failed
service = "nginx"             # its name as svc shows it (case does not matter)
```

The one address of before (`[notify] webhook` and `webhook_format`) is still
read, as the first webhook; the settings page moves it to a `[[notify.webhooks]]`
when you change the webhooks there.

`cpu`, `mem`, `swap`, `gpu` (the busiest) and `disk` (the fullest volume) are
percentages, `temp` is the hottest sensor in °C, `battery` the charge. A `service` rule takes a
name instead of a number and fires when that service is stopped or failed (not
while it is starting or stopping, and not for a name svc does not list). A rule
fires once when it has held for `for` seconds and again only after it has
stopped holding; the status bar shows it too. There are no rules until you
write them. Only this machine's readings are watched, also while another
machine's screens show. A mistake in a rule is warned about on the settings
screen's warnings and the rule is left out.

## Fleet

The fleet screen (`F`) shows other machines next to this one. On each of them
run the same binary without a window:

```sh
yagura --agent
```

It listens on `127.0.0.1:7313`, and the viewer reaches it through a tunnel
that already decides who may connect and encrypts the way, or directly with
keys ([below](#direct-connections)). Either

```sh
ssh -N -L 7314:localhost:7313 nas          # on the viewer: nas's agent on localhost:7314
tailscale serve --bg --tcp 7313 tcp://localhost:7313   # on the agent: reachable as nas.<tailnet>.ts.net:7313
```

and name it in the viewer's `config.toml`:

```toml
[[host]]
name = "nas"
addr = "localhost:7314"        # through ssh -L

[[host]]
name = "pi"
addr = "pi.tail1234.ts.net"    # through tailscale serve
```

The agent samples like the window does (`interval`) and sends a summary each
round: CPU, memory, the first GPU's usage, the hottest temperature, the
battery, how many viewers it has and counts of what is wrong (temperatures
drawn red, drives failing or with SMART counts drawn yellow, failed services (not drivers), devices with a problem and containers restarting or exited with an error; services, devices and containers are read every minute for this even while their screens
are not shown), with its host name, OS, CPU, uptime, version and port once at the
start (protocol 9). After the first summary only what changed goes, which on a
quiet machine is a few bytes; a viewer whose channel fills up gets a whole one
again. A host with `every = 5.0` in the viewer's `config.toml` asks its agent
for a summary every 5 s at most (0.5 to 60; never faster than the agent
samples), for a thin link; the agent keeps sampling at its own pace for the
other viewers.

`Enter` on a card (or `[` / `]` from any screen) shows that machine's screens:
the viewer then asks its agent for every reading the window draws from, and the
agent streams them as they are sampled (protocol 2). Only what is watched is
sampled: processes about every 4 seconds while someone watches the machine,
services only while its svc screen shows, sockets only while its net screen
shows; what does not change (the system, the hardware) is kept and sent at once
to each new viewer. Several viewers share one sampler. This machine keeps
sampling in the background, so `[` / `]` back to it finds its last minute.
Ending a process and acting on services work there as here when its agent
allows them (below); pause and the interval are its own (change them there),
and on its screens those keys say so. The viewer keeps one thread per host, pings it every 5 s for the
round trip, marks it slow over 200 ms or after three missed summaries, and
connects again after 2 s, doubling to 30 s while it stays down. The messages
are [postcard](https://crates.io/crates/postcard) (serde's compact binary
form) with a protocol number, so a viewer and an agent that speak different
protocols say so instead of misreading each other.

The agent is view only unless its own `config.toml` says otherwise:

```toml
[agent]
allow = ["kill", "service"]   # either or both
```

Its card then shows `actions: kill service`, and inside its screens `x`
(procs, and the process lists of the other screens), `S` and `R` (svc) ask
first as on this machine: the window names the machine and the account its
agent runs as, and `y` sends the request (protocol 3). The agent checks it
against its own `allow`, not the viewer's word; it will not end itself, finds
the service in its own list (a launchd job is loaded from the plist that
machine read, never from a path the viewer sent), and runs it as its own
user, so what needs administrator or root there needs the agent to run that
way. The answer comes back as a toast that starts with the machine's name;
if the link drops before it comes, a toast says so (look at its screen to see
what happened), and a request is never sent again on the next connection.
The agent writes each request and its outcome to its console
(`yagura agent: 127.0.0.1:52144 asks to end 1234 (started …): Kill(Ended)`).
A viewer and an agent of different protocols do not connect (v0.41 and
v0.40, say; the card says which side to update).

To keep an agent up, let the OS start it at logon:

```sh
yagura --install-service     # and starts it now
yagura --uninstall-service   # stops it and forgets it
```

Neither needs an administrator. On Linux it writes a systemd user unit
(`~/.config/systemd/user/yagura-agent.service`) and runs `systemctl --user
enable --now`; what the agent says goes to `journalctl --user -u
yagura-agent`. On macOS it writes a LaunchAgent
(`~/Library/LaunchAgents/io.github.uchmk.yagura-agent.plist`, started again
if it fails) and bootstraps it; it writes to `agent.log` in yagura's state
folder. On Windows it sets a value in the user's `Run` key (a logon task in
the Task Scheduler would need an administrator) and starts the agent now
without a window, writing to `agent.log` in the state folder. `UCHMK_CONFIG_DIR`,
`YAGURA_CONFIG_HOME` and `YAGURA_STATE_HOME` are carried into the unit and the
LaunchAgent; on Windows they are not, and it says so.

`yagura.exe --agent` run from a console writes there. Anywhere,
`--agent --log FILE` also appends each line, with the time, to `FILE`
(started over past 1 MiB): Windows' release build has no console to write to
otherwise.

### Direct connections

Without ssh or Tailscale, a viewer can reach an agent across the network
over the [Noise protocol](https://noiseprotocol.org/)
(`Noise_IK_25519_ChaChaPoly_BLAKE2s`, through the pure Rust
[snow](https://crates.io/crates/snow)). Keys are swapped beforehand, the way
WireGuard does it: no certificates and no passwords. Each machine makes a
key pair the first time it is asked for one and keeps the private half in
`key` in yagura's config folder (readable by its account alone on Linux and
macOS). Print this machine's public key and the snippets that use it with

```sh
yagura --key
```

or copy them from the settings screen's Fleet page. On the machine watched:

```toml
[agent]
listen = "0.0.0.0:7314"   # or one address; it listens on 127.0.0.1:7313 as well

[[agent.viewer]]
name = "laptop"
key = "<the laptop's public key>"
```

and on the viewer:

```toml
[[host]]
name = "nas"
addr = "nas.lan"          # the port defaults to 7314 with a key; a name tries each address, IPv4 first
key = "<nas's public key>"
```

The viewer knows the agent's key and sends its own, encrypted, in the first
message; the agent answers only the viewers it lists, and the card says
when it does not know this one or when the key is another machine's. A
connection to the direct port without a key gets nowhere, and the plain
port stays on 127.0.0.1. The sys screen's agent section shows where it
listens directly and its key. Let the port through the machine's firewall
(on Windows it asks the first time the agent listens).

### Prometheus

`[agent] metrics = "127.0.0.1:9314"` (the port is 9314 when left out) also
makes `yagura --agent` answer `GET /metrics` in Prometheus' text format:
CPU in total and per logical CPU, the load average, memory and swap, each
interface's bytes and rates, each volume's size and free space, the disk
rates, temperatures and fans, each GPU's use, memory and temperature, the
battery, and, while a viewer is watching procs or containers, the process
count and the containers. The page is made from the agent's latest readings
only when it is asked for. It is plain HTTP with no login, so keep it on
`127.0.0.1` (or behind a firewall or a reverse proxy) unless the network is
yours; add the address to Prometheus' `scrape_configs` as a static target.

## Languages

Every word on screen goes through one table: [src/lang/en.toml](src/lang/en.toml)
for English, [src/lang/ja.toml](src/lang/ja.toml) for Japanese. A translation
has the same keys, and any key it leaves out reads in English; its
`[keydesc]` translates the built-in keymap's descriptions in `?` help (one
you wrote yourself reads as you wrote it). The language is picked from
`config.toml`, then `common.toml`, then the OS (`ja-JP` is Japanese).
The table and that choice come from `ito-i18n`, a crate in
[ito](https://github.com/uchmk/ito), the uchmk apps' shared crates; the settings screen's
Language row (in yagura or tsumugi) writes `common.toml`, and every running
app changes language within 2 s.

Japanese, and the CJK in process names, service descriptions and volume
labels whatever the language, is drawn with a system font behind the
built-in one, read on a thread at startup (the first found of BIZ UDGothic,
MS Gothic, Yu Gothic and Meiryo on Windows; Hiragino on macOS; Noto Sans CJK
or IPA Gothic on Linux). A wide character counts as two columns where text
is cut to fit.

## Downloading

Each release on the
[Releases page](https://github.com/uchmk/yagura/releases) has six files:
Windows (`.zip`), macOS and Linux (`.tar.gz`), each on x86_64 and ARM64,
holding the executable and the licence files (the Windows zip also holds `yagura.com`, see below). The notes list every file's
SHA-256. Nothing is code-signed: SmartScreen warns about an unknown
publisher, and macOS's Gatekeeper wants the binary allowed by hand.
Windows is where it is tested; the macOS builds are only compiled.

### `yagura.com` on Windows

`yagura.exe` is a windowed program, so PowerShell does not wait for it: `$v = & yagura env`
comes back empty and `yagura --version` prints after the prompt. The Windows zip holds
`yagura.com` beside it, a small console program. `PATHEXT` lists `.COM` before `.EXE`, so typing
`yagura` finds it first. For the commands that answer in text (`env`, `--version`, `--help`,
`--agent`, `--snapshot`, `--log-metrics`, `--key`, `--install-service`, `--uninstall-service`) it
runs `yagura.exe` with the same arguments and handles, waits, and passes on the exit code;
anything else opens the window and returns once it is up. Opened from a pipe, the window
writes into a file of its own in the temporary folder (`yagura-com-*.out` and `.err`), copied
out once it is up and deleted when it closes. The front is the one every uchmk app shares
(ito's `ito-console`). **Keep the two in one folder.**
Building yourself, `cargo build --release` makes `yagura-com.exe`: copy it to `yagura.com`
beside `yagura.exe`.

## Terminal version (Linux, Proxmox VE)

`yagura-tui` (in `crates/yagura-tui`, built with the workspace) puts the host and its Proxmox VE guests on one screen: the host's
cpu / memory / swap, and every guest from `pvesh` with its cpu, memory, swap (from the cgroup, red near the limit), network rate and
uptime. `Tab` switches to the host's processes, then the storages (type, free space, fullest first) and the latest tasks of every node (`Enter` reads a task log; failed backup jobs show in the header), `s` sorts, `Enter` looks inside a running container with `pct exec` (OS, packages,
upgradable count, reboot required; `apt update` is never run). `Space` marks guests so `u` `o` `d` `r` act on all of them, `l` inside a guest shows its journal, `S` / `b` / `m` take a snapshot, start a backup (`vzdump`: storage, mode, compression) or list, delete and roll back snapshots, each behind a y/n that shows the expected size and the free space, `C` removes snapshots older than `snapshot_days` (30) and backups beyond the newest `keep_backups` (3) per guest (protected ones stay), `x` in the task screen stops a running task, `c` shows the guest's configuration (read only) and `e` sets memory, cores, swap (containers) and CPU limit of the marked guests `z` grows a disk of a guest, `Y` clones a guest (a template gives a linked clone), `D` deletes a stopped one and `T` turns a stopped one into a template and `N` edits the bridge, VLAN and MAC of a network device (each behind a y/n), `t` opens the console of a running guest (`qm terminal` / `pct enter`) and comes back when it ends, `n` opens the node menu (maintenance mode, start all / shut down all guests of the node, apt update, reboot and shut down of the node, each behind a y/n), `/` searches the task log and a guest's log (`n` / `N` step through the matches), Enter in the storage screen lists the backup files of a storage (`Space` marks, `d` deletes, `r` restores as a new guest with a free id; each behind a y/n), the detail pane reads the task log and snapshots (last starts, stale after `YAGURA_STALE_DAYS`, failed tasks), and the header shows storage use and a wanted reboot. Run it as root on the host. It is not part of the release downloads yet.

```sh
cargo run --release -p yagura-tui
```

## Building

```sh
cargo build --release
```

To put the build output somewhere else on one machine only (a RAM disk, say), write
`.cargo/config.toml` in the clone; it is ignored by git, so CI and other machines keep `target/`:

```toml
[build]
target-dir = "R:/uchmk/yagura"
```

Pure Rust apart from the OS's own libraries; the six targets (Windows, macOS
and Linux on x86_64 and ARM64) need no C toolchain beyond the platform's
linker. `build.rs` gives `yagura.exe` its icon and version information on
the MSVC targets, writing the resource file in Rust (ito's `ito-winres`), so no
`rc.exe` is needed, from Windows or not. The icon is drawn in code
(`src/icon.rs`); the window uses the same drawing on every OS. The core layout comes from sysfs on Linux and
`GetLogicalProcessorInformationEx` on Windows. On Apple silicon it comes
from the device tree's `cpus` node in IOKit: a group per cluster (P or E
cores sharing one L2; there is no L3), with the L2's size from
`sysctl hw.perflevelN.l2cachesize`. Intel Macs use `hw.physicalcpu` /
`hw.logicalcpu` (threads side by side) and `hw.l3cachesize`.

The gpu·npu screen reads, without linking anything at build time:

| OS | GPUs | NPUs |
| --- | --- | --- |
| Windows | The `GPU Engine` and `GPU Adapter Memory` performance counters (PDH) for usage, engines, memory and per-process use, named through DXGI, with the driver version dxdiag shows (a process's memory above the card's is shown as `—`); temperature, fan and clock from D3DKMT (`D3DKMTQueryAdapterInfo`, what Task Manager shows) where the driver keeps them, and from NVML for NVIDIA | One with an MCDM driver, the kind Task Manager shows, from the same counters |
| Linux | `/sys/class/drm`: amdgpu has usage, VRAM and hwmon sensors; i915 / xe have the clock and sensors but no busy counter. Per-process use and memory come from the DRM fdinfo (`/proc/<pid>/fdinfo`, written by i915, xe, amdgpu, panfrost, msm…), and so does the usage of a GPU sysfs gives none for: only the processes yagura may read count, so run it as root to see everyone's | `/sys/class/accel`: `intel_vpu` has usage and clock; `amdxdna` is listed only |
| macOS | IOKit's `IOAccelerator` services and their `PerformanceStatistics`: Apple silicon has usage, the renderer and tiler engines and the unified memory in use; Intel GPUs have usage; AMD cards also VRAM and, on some, temperature, fan, power and clock. No per-process use; the SMC's GPU temperatures are on the sensors screen | The Neural Engine reports no usage to other programs |

The svc screen reads:

| OS | From | Notes |
| --- | --- | --- |
| Windows | The service control manager (`EnumServicesStatusExW`, and `QueryServiceConfigW` for the start type, account and command, kept a minute) | Drivers are read too, and shown instead of the services with `u`; a failed service shows its exit code |
| Linux | `systemctl list-unit-files` and `systemctl show '*.service'` | Without systemd running (a container, WSL without systemd) only the unit files are listed, their state unknown |
| macOS | `launchctl list` | As a user, that user's jobs; the system's need `sudo` |

The net screen reads:

| OS | Interfaces | Sockets |
| --- | --- | --- |
| Windows | The IP helper (`GetIfTable2`, `GetUnicastIpAddressTable`): the physical adapters, even unplugged, Bluetooth PAN, and any other that is up with an address (VPNs, Hyper-V switches), without the filter layers, the loopback pseudo-interface, removed devices' leftovers (`Not Present`), the hidden Wi-Fi Direct adapters and the adapters marked hidden in the registry (`NCF_HIDDEN`) or left out by `Get-NetAdapter` (a Wi-Fi card's extra ports, from WMI's `MSFT_NetAdapter`); Bluetooth PAN told apart by `PhysicalMediumType` | `GetExtendedTcpTable` / `GetExtendedUdpTable` with the owning PID |
| Linux | sysinfo for the counters and addresses; `/sys/class/net` for the kind (Bluetooth PAN from `DEVTYPE` in `uevent`), link speed and driver | `/proc/net/{tcp,tcp6,udp,udp6}`, the owner from the `socket:[inode]` links in `/proc/*/fd`. Another user's sockets show no process without root |
| macOS | sysinfo for the counters and addresses; `networksetup -listallhardwareports` for the kind and adapter (Wi-Fi, Ethernet, Bluetooth PAN, Thunderbolt Bridge), read once a minute; the rest (`utun`, `awdl`) by name | `netstat -anvW`, with the PID of the process that last used each socket |

The disk screen reads:

| OS | Drives | Volumes |
| --- | --- | --- |
| Windows | The `PhysicalDisk` performance counters (PDH) for the traffic and busy time; `IOCTL_STORAGE_QUERY_PROPERTY` and `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` on `\\.\PhysicalDriveN` for the model, bus, SSD / HDD and size, asked once per drive without administrator. Volumes are linked by their drive letters. If the counters are off (`diskperf -n`), the screen says so and shows the volumes alone | sysinfo |
| Linux | `/proc/diskstats` for the traffic, totals and busy time; `/sys/block` for the whole drives (not `loop`, `ram`, `zram` or partitions), their size, model, bus and `rotational`. Volumes are linked through the device they were mounted from | sysinfo |
| macOS | IOKit: each `IOBlockStorageDriver`'s `Statistics` for the traffic and totals since boot, the device above it for the model, SSD / HDD and bus, the whole disk below it for the BSD name (`disk0`) and size. Disk images are left out; macOS keeps no busy time, so there is no busy graph. Volumes are linked by walking up from the device they were mounted from (an APFS volume through its container to the partition it lives on) | sysinfo |

Each drive's SMART health is asked once a minute (and when the drives
change) in a thread of its own, so a slow drive never holds up the screen:

| OS | Health | Temperature | What SMART counts |
| --- | --- | --- | --- |
| Windows | `IOCTL_STORAGE_PREDICT_FAILURE`, skipped while the drive is spun down (`GetDevicePowerState`) so it is left asleep. Some drivers answer it only for an administrator, and the row says so. An NVMe drive's critical warning counts as failing too | `StorageDeviceTemperatureProperty` through `IOCTL_STORAGE_QUERY_PROPERTY`, without administrator: an SSD's every 5 seconds, any other drive's with the minute's check; else the NVMe health log's | NVMe: the SMART / health log through `IOCTL_STORAGE_QUERY_PROPERTY`'s `StorageDeviceProtocolSpecificProperty` (life used, hours, writes, media errors). SATA: the attributes from `SMART_RCV_DRIVE_DATA`'s SMART READ DATA (hours, remapped, pending, an SSD's life used, and the temperature from attribute 194 where the property gave none). Windows answers it only for an administrator; otherwise the drive shows `SMART counts: needs administrator` |
| Linux | `smartctl -H -A -n standby` (smartmontools), which leaves a spun-down drive asleep. It needs root or the `disk` group, and the row says so, or that smartctl is not installed; virtual disks (`virtio`, `xen`) have no SMART data | The drive's hwmon chip (`nvme`, `drivetemp`), else what smartctl read | The same smartctl output: NVMe's percentage used, hours, data units written and media errors; ATA's attributes 5 (reallocated), 9 (hours), 197 (pending) and the wear attribute (`Wear_Leveling_Count`, `SSD_Life_Left`…); SCSI's endurance, hours and grown defects. A drive left asleep keeps its last counts |
| macOS | `diskutil info -plist`'s `SMARTStatus` (internal drives; most USB enclosures pass nothing). An NVMe drive's critical warning, or an ATA attribute past its threshold, counts as failing too | The NVMe health log's; a SATA drive's attribute 194 (190 where there is none) | NVMe (the internal SSD, and enclosures that pass it through): the SMART / health log from IOKit's `IONVMeSMARTInterface`, without sudo (life used, hours, writes, media errors). SATA (older Macs' internal drives, enclosures that pass it through): the attributes from `IOATASMARTInterface`'s `SMARTReadData`, without sudo (hours, remapped, pending, an SSD's life used) |

The sensors screen reads:

| OS | Sensors |
| --- | --- |
| Windows | While [HWiNFO](https://www.hwinfo.com/) runs with Shared Memory Support on, every chip it reads (cores, board, fans, drives, GPUs) from its shared memory; else, while [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) (or OpenHardwareMonitor) runs, its WMI classes. Both read the chips through their own driver; yagura ships none and needs no administrator. Always the ACPI thermal zones from the `Thermal Zone Information` performance counters (PDH). With neither tool running, fans and the cores' own temperatures are not readable, so the screen says so and names the tools; GPUs add their temperature and fan from D3DKMT (and NVML for NVIDIA), and the drives theirs from the disk reading |
| Linux | `/sys/class/hwmon`: every chip's `temp*` (with `max` and `crit`) and `fan*` (with `min`), named by the chip and, for drives, their model. Where no chip has a temperature (many ARM boards), the `/sys/class/thermal` zones and their trip points instead. NVIDIA GPUs add theirs from NVML; an AMD GPU's own chip is not listed twice. A drive with no hwmon chip (SATA without `drivetemp`) adds the temperature smartctl read |
| macOS | The SMC itself, through the `AppleSMC` user client: the temperatures with a known key, named (`TC0P` is "CPU proximity" on Intel Macs; Apple silicon's `Tp…` / `Te…` / `Tg…` families are numbered "CPU 1", "GPU 1"…), and every fan with its least and most rpm. On Apple silicon also the HID temperatures sysinfo reads, under their own chip with friendlier names ("CPU die 3", "SSD", "Battery") |

The devices screen reads, with no administrator and no tool to install:

| OS | Devices |
| --- | --- |
| Windows | SetupAPI: every present device of every class (`SetupDiGetClassDevsW` with `DIGCF_ALLCLASSES`), named as Device Manager names it (the friendly name, else the description) under the class's own, localised name (`SetupDiGetClassDescriptionW`), with the maker, driver service, `DEVPKEY_Device_DriverVersion`, the hardware id (`USB\VID_046D&PID_C52B&REV_1211`, `PCI\VEN_8086&DEV_A0ED`), the location and the parent (`DEVPKEY_Device_Parent`). The status and problem code come from `CM_Get_DevNode_Status`; devices Device Manager hides are left out. USB link speeds are not read |
| Linux | sysfs: every USB device in `/sys/bus/usb/devices` (its interfaces decide the kind and the drivers when the device leaves them to it; `authorized` = 0 shows as disabled) and every PCI function in `/sys/bus/pci/devices` (no `driver` link: no driver), the parent from where each sits under `/sys/devices`, the driver's version from `/sys/module`. Names the device does not give come from `usb.ids` / `pci.ids` (the hwdata package) when one is installed; without it, the ids. Serial numbers are not read |
| macOS | IOKit: every `IOUSBHostDevice` (name, maker, `VID:PID`, revision, speed, location) and `IOPCIDevice` (model, `vendor:device`, class) in the registry, each under the nearest such device above it, the driver being the class of the object matched to it. No `system_profiler`, so the 5-s reading stays light |

Classes on macOS and Linux are yagura's own (from the USB and PCI class codes), named in yagura's language. Another machine's devices come from its agent, which must be v0.43 or newer (fleet protocol 6).

The sys screen reads:

| OS | Machine and memory slots |
| --- | --- |
| Windows | The firmware's SMBIOS table from `GetSystemFirmwareTable('RSMB')`, which needs no administrator (the processors from its types 4 and 7, the slots from 17), and the boot mode from `GetFirmwareType` |
| Linux | The SMBIOS table from `/sys/firmware/dmi/tables/DMI`, which only root may read. As a normal user, the world-readable `/sys/class/dmi/id` files give the system, board and firmware but no slots or processor details (the cores and threads then come from the core layout), and the screen says to run as root for them; boards without SMBIOS (many ARM boards) give the device tree's `model`. UEFI boot from `/sys/firmware/efi` |
| macOS | `system_profiler -xml SPHardwareDataType SPMemoryDataType`: the chip or CPU and its cores (and an Intel Mac's L3); an Intel Mac lists its slots; Apple silicon has one block of unified memory in the chip's package. Serial numbers are not shown |

On Windows and Linux, NVIDIA cards are read through NVML, loaded from the
driver at run time (`nvml.dll` / `libnvidia-ml.so.1`). On Linux NVML also
gives each process's memory and use (its busiest of the SM, encoder and
decoder, averaged since the last reading; Maxwell or newer); without it they are
listed from sysfs on Linux with nothing read, and on Windows keep only what
D3DKMT gives (temperature, fan and clock, no power).

`yagura --keys SCRIPT` presses keys once the window is up (as kura's
`--keys`), and `scripts/xrun.sh` runs it on a virtual X display and keeps the
screenshots: that is how changes are checked without a screen. CI does the
same on every push (`scripts/smoke-linux.sh`): it visits every screen on
Mesa's software OpenGL and fails when one panics or draws nothing.

## License

MIT or Apache-2.0, at your option.
