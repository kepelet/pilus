# dmgbuild settings: the "drag pilus to Applications" Finder window.
#
# Invoked by `package-macos-dmg.sh` as
#   dmgbuild -s scripts/dmg-settings.py -D app=<pilus.app> -D background=<tiff> pilus out.dmg
#
# The updater (`gpui-updater-core`) mounts this image and takes the first
# `*.app` at the volume root, so the Applications link and the hidden
# background must never gain an `.app` extension. Window and icon geometry
# must match `scripts/dmg-background.py`, which draws the arrow between them.
import os.path

app = defines["app"]  # noqa: F821 - injected by dmgbuild
app_name = os.path.basename(app)

format = "UDZO"
files = [app]
symlinks = {"Applications": "/Applications"}
background = defines["background"]  # noqa: F821

window_rect = ((200, 120), (660, 400))
icon_size = 128
text_size = 13
icon_locations = {app_name: (180, 170), "Applications": (480, 170)}

show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
default_view = "icon-view"
arrange_by = None
