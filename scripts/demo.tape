# Records docs/demo.gif: the demo (made-up sessions), browsed with a few keys,
# in a window on the same wallpaper as the screenshot. From the repository:
#
#   cargo build --release
#   python3 scripts/screenshot.py --wallpaper 1600x1260 target/wallpaper.png
#   vhs scripts/demo.tape
#
# Needs vhs (https://github.com/charmbracelet/vhs) and the Fira Code font.

Output docs/demo.gif

Set Shell bash
Set FontFamily "Fira Code"
Set FontSize 14
Set LineHeight 1.15
Set Width 1600
Set Height 1260
Set Padding 18
Set Margin 56
Set MarginFill "target/wallpaper.png"
Set WindowBar Colorful
Set WindowBarSize 40
Set BorderRadius 12
Set Theme "Catppuccin Mocha"
Set Framerate 24

Hide
Type "clear && target/release/petsitter --demo"
Enter
Sleep 1.5s
Show

# The selected session works: its name shimmers, its details below.
Sleep 3s

# Up the tree: a session waiting for its next prompt, then the folder.
Up
Sleep 1.8s
Up
Up
# A Codex session with two background shells.
Sleep 2s
Up
# One blocked on a permission prompt.
Sleep 2.5s

# Back down to the session at work, and hide the details.
Down
Down
Down
Down
Sleep 1.5s
Type "d"
Sleep 2s
Type "d"
Sleep 2s

Hide
Type "q"
