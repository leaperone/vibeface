# Terminal interface

The CLI serves people at a terminal and programs through pipes. Human presentation must preserve command meaning and machine output.

## Entry points

`vibeface` and `vibeface serve` replace the CLI process with the Node. They do not print a welcome screen or start the TUI. `vibeface tui --data-dir <PATH>` explicitly opens a read-only network monitor. It never starts, stops, joins, or changes a Node.

`status --json`, tools, hooks, provider output, and logs keep their existing protocols. Redirected `status` keeps its original text and exit status. Only human terminal `status` uses aligned labels, whitespace, and width-aware continuation lines. Full IDs and paths remain visible. Help describes every root command, explains foreground startup, and wraps to terminal width.

## Network monitor

The monitor reads the existing `ControlRequest::Inspect` response. The typed display model contains Node state, ID, listen address, data directory, version, Web state, write state, and peers. It has no CPU, throughput, latency, or sync-progress metrics. The local device is called `This node` because Inspect does not provide its name.

Write states are `aligning`, `read_only`, `writable`, and `forked`. Missing write state is unavailable. Missing peer data is distinct from an empty peer list. Peer reachability is `reachable`, `unreachable`, or `not tried yet`. Details show the full peer ID, actual direct or relay path, and last successful sync age.

An initial load shows `LOADING`. A successful response shows its real running, stopped, or unavailable state. Refresh failure keeps the last successful snapshot and labels the header `STALE`, the network `last known result`, and the update with its last successful age. A failure before any success shows an unavailable page. When the list has room for a second summary line, stopped Nodes show a startup command with an explicit data-directory argument. Terminal `status` always includes that command for a stopped Node.

At least 88 columns and enough content height show the network list beside a compact label/value summary. The summary keeps each value with its label and points to full details. Smaller windows use one list. Enter opens full-width details at every size, with scrolling for long fields. Windows smaller than 32 columns or 14 rows show a size hint and continue to accept exit keys. Selection follows the stable device ID through refresh and resize.

## Keyboard and refresh

- Up, Down, j, and k select devices or scroll details and help.
- PageUp and PageDown move by eight rows. Home and End move to the first or last row.
- Enter opens details. Esc closes help, returns from details, or quits from the network list.
- r refreshes. ? toggles help. q and Ctrl-C quit.
- Key release does nothing. Navigation accepts repeat events. Repeated action events do not reopen details or enqueue refreshes.
- Ctrl-Z and SIGTSTP are ignored. Use q to return to the shell.

Inspect refreshes every three seconds with at most one request in flight. A pending request preserves the page and shows a refresh label. Keyboard events stay responsive while Inspect waits. The existing control transport bounds the response and its deadline. Exiting drops the UI's kill-on-drop helper without touching the running Node. Ratatui updates changed cells rather than clearing the screen for each refresh.

## Presentation and terminal safety

The interface uses the terminal's default background and foreground throughout. Bold headings, warnings, and selected text provide emphasis alongside a visible selection marker. Metadata and key hints use normal text, with no dim attribute. Help and human status use the same default-foreground bold treatment. `NO_COLOR` removes emphasis. No fonts, terminal profile, title, palette, mouse handling, clipboard, or external URLs are changed. There is no startup animation, spinner, or simulated activity.

Display text escapes C0, C1, ESC, DEL, and bidirectional control characters. Ratatui's grapheme and cell-width APIs preserve CJK, combining characters, and emoji in shortening and wrapping. Raw JSON and redirected output are not rewritten. TUI does not read logs, access capabilities, wallet passphrases, relay credentials, or model keys.

TUI requires interactive stdin and stdout and a non-dumb TERM before entering raw mode. Its guard restores the cursor, alternate screen, and terminal mode on normal return, error, or panic. SIGINT, SIGTERM, and SIGHUP restore the terminal before the process exits with that signal. SIGKILL cannot run cleanup.

## Verification

Inline CLI tests cover argument isolation, truthful display parsing, untrusted terminal text, Unicode wrapping, keyboard events, stable selection, and stale snapshots. Ratatui TestBackend tests cover 100×28, 45×24, compact and tiny windows, plus exit hints across 32 through 110 columns. Real PTY validation separately checks the actual binary, signal restoration, refresh cancellation, and machine output. Source and buffer tests do not establish a visual award or replace real terminal acceptance.
