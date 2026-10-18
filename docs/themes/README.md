# Terminal skins

Rusty keeps ordinary terminal scrollback and inherits the terminal background.
Use `/theme rust` for Rust Dark, `/theme paper` for Paper Light, or `/theme mono`
for monochrome. Success, failure, warning and input have separate color roles;
labels and exit codes remain readable without color. `NO_COLOR` removes styling.

Pair the selected skin with the matching `.conf` file in this directory. These
are Ghostty terminal presets, also usable by cmux. Merge the values into your
terminal configuration; Rusty never changes global terminal settings itself.
In particular, select a light background before using Paper Light. Do not use
Paper Light on a dark terminal, or the dark skins on a light terminal.

New settings default to `/font compact`: a short header showing the workspace,
model, permissions and execution mode. Existing font preferences are preserved;
run `/font compact` once to switch. `/font rust`, `block`, `thin`, and `classic`
remain available. Long infrastructure targets remain visible and may wrap.

A single stationary progress row shows the active operation and elapsed time.
At 40 columns or wider it also shows `ctrl-c to stop`. A quiet rule separates
input from output without enclosing the line editor in a box. Input retains
its color across wraps, continuation lines, and submission.

The screen remains usable in narrow cmux splits: supplementary header metadata
is omitted below 60 columns. Full model names and workspace paths are available
in the existing settings and session commands; the compact header clips them.
