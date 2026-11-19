# agentflow demo recording.
#
# Produce the GIF with charmbracelet's vhs (https://github.com/charmbracelet/vhs):
#
#     cargo build --release      # the tape runs the prebuilt binary
#     vhs demo.tape              # -> demo.gif
#
# The demo loops a scripted ~14.5s session; we record one full loop.

Output demo.gif

Set Shell bash
Set FontFamily "JetBrains Mono"
Set FontSize 16
Set Width 1120
Set Height 560
Set Padding 28
Set Framerate 24
Set LoopOffset 40%

# Launch straight into the looping demo (hide the command itself).
Hide
Type "./target/release/agentflow --demo"
Enter
Sleep 800ms
Show

# Watch one full loop of the scripted session.
Sleep 15s

# Quit cleanly.
Hide
Type "q"
Sleep 300ms
