# agentflow demo recording.
#
# Produce the GIF with charmbracelet's vhs (https://github.com/charmbracelet/vhs):
#
#     cargo build --release      # the tape runs the prebuilt binary
#     vhs demo.tape              # -> demo.gif
#
# A tour of the views over one pass of the scripted demo (~18s): the tree,
# `v` to the orbit for the handoff chain (reviewer → fixer → test-runner and
# both validator verdicts), `v` to the timeline, ↓ + Enter for an agent's
# detail view, then Esc back. The demo loops every 20s, so nothing wraps.

Output demo.gif

Set Shell bash
Set FontFamily "JetBrains Mono"
Set FontSize 16
Set Width 1120
Set Height 720
Set Padding 28
Set Framerate 24
Set TypingSpeed 0
Set LoopOffset 40%

# Launch straight into the looping demo (hide the command itself).
Hide
Type "./target/release/agentflow --demo"
Enter
Sleep 800ms
Show

# Tree: scout, researcher and a test-runner start at once (team player ✦);
# the researcher waits on a permission.
Sleep 2.2s

# Orbit: results return (the first test-runner ✗ fails), the reviewer rejects
# (✗ fail), its findings go to the fixer, the patch goes to a second
# test-runner, which passes (✓).
Type "v"
Sleep 10.3s

# Timeline: the whole session as swimlanes.
Type "v"
Sleep 2s

# Detail view for the first agent, then back.
Down
Sleep 300ms
Enter
Sleep 2.2s
Escape
Sleep 1s

# Quit cleanly.
Hide
Type "q"
Sleep 300ms
