# Desktop motion recordings

Recorded 2026-09-08 using the local release-profile Boomux Desktop binary in an
isolated Xvfb display at 1280 × 800, 30 fps. These are actual application frames,
not a browser imitation or composited animation. The clips add instructional
shortcut captions over the recording; pane motion is unmodified. The terminal contents are
deliberately fictional text fixtures (editor, development server, and tests),
not claims that those programs were running. No Agent activity is fabricated.

The Desktop uses Smooth motion. Move, Resize, and Keyboard use Tree;
the Tabs clip uses the default Tabs view. Hyprland inspired the interaction,
but these recordings exercise Boomux's own controls on X11, not Hyprland's
window manager. The isolated environment uses the default Desktop palette.

## Shots

- **Move:** drag the tests pane by its heading from the upper-right area, across
  the workspace, and release on the left. Its live view follows the pointer,
  then the tree rearranges around the drop. The drag follows one continuous
  curved path in 0.65 seconds, without pauses at intermediate points. Tap
  Ctrl + Space, then Shift + Down, Up, Right, Down, Up to swap panes through
  several arrangements. All three panes change positions. F expands the pane;
  Escape leaves layout mode for a clear view. Reenter layout mode and press F
  to return, then Escape. Playback is real-time with short settling pauses.
  All GIFs use 25 fps for smoother movement.
- **Resize:** drag the inside edge of an upper pane left and right, then drag
  its bottom edge vertically. The hit target is the pane edge, not the gap.
- **Keyboard:** tap Ctrl + Space; use Left, Down, Up to change focus;
  press J twice to flip the nearest split and return; Escape exits layout mode.
  Holding the leader is supported but not demonstrated in this clip.
- **Tabs:** press Ctrl + W three times to minimize all panes into restore tabs.
  Click each tab to restore the shells. No shells are removed or stopped.

## Recording safely

Use a dedicated display and a dedicated daemon runtime. Give the demo its own
HOME and XDG runtime/config/state/data/cache directories, and do not inherit
BOOMUX scope variables, harness configuration overrides, or the user's D-Bus
session. Create only fictional demo Shells. Never automate the user's active
display or stop their daemon to make a recording.

For these captures, Xvfb and xdotool ran from a temporary extracted package
directory; no desktop configuration or installed packages were changed.
The display number was allocated by Xvfb's `-displayfd`, and input automation
targeted that exact display. FFmpeg's `x11grab` recorded only that display.
The isolated app and daemon were stopped after recording.

Review each clip's start, movement, and end before publishing. Keep playback
at real speed. Export from the reviewed recording with:

```sh
ffmpeg -i recording.mp4 -an -c:v libx264 -preset slow -crf 24 \
  -pix_fmt yuv420p -movflags +faststart move.mp4
ffmpeg -i recording.mp4 -filter_complex \
  'fps=25,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=bayer:bayer_scale=4' \
  -loop 0 move.gif
ffmpeg -ss 0.5 -i recording.mp4 -frames:v 1 -quality 85 poster.webp
```

Use MP4 on the page to avoid automatic GIF downloads and allow pause, seek,
fullscreen, and reduced-motion behavior. Keep GIF copies for sharing. Check
the recordings under the `/boomux/` base path with the website browser tests.
