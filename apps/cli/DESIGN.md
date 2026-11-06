# Terminal interface

The CLI serves people at a terminal and programs through pipes. Human presentation must preserve command meaning and machine output.

## Entry points

`vibeface` starts the Node. An interactive dashboard must use an explicit subcommand so opening it does not start or stop a service. `status --json`, tools, hooks and provider protocol output remain machine-readable.

## Visual contract

The terminal interface uses a compact wordmark, clear section headings, aligned labels and space between related groups. Status has both a word and a visual marker. Color supports hierarchy but never carries meaning alone. The terminal controls font choice and size.

A dashboard shows live Node and peer information from `ControlRequest::Inspect`. Missing, unreachable and stale data remain distinct. Refreshing must preserve navigation and must not replay commands or mutate the Node.

Layouts adapt to terminal width and height. Long paths and device names must not push navigation off-screen. User-provided text cannot emit terminal control sequences. Keyboard actions remain visible, and exiting restores the cursor, terminal mode and prior screen.

Use motion only to explain activity or a state change. Repeated refreshes must not blink the entire display. Colorless and basic terminals retain readable output.

## Acceptance

Inspect real terminal output for typography, whitespace, hierarchy, color, motion, keyboard feedback, narrow layouts and a coherent visual identity. Exercise loading, offline, empty, populated and refresh-failure states. Verify redirected output, JSON, exit codes and terminal restoration separately. Award-site references guide visual criticism; they do not prove terminal acceptance.
