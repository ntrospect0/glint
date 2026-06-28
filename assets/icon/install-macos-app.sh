#!/bin/bash
#
# Build a double-clickable macOS launcher (~/Applications/Glint.app) that
# opens glint in your terminal, using the bundled glint icon.
#
# Auto-detects an installed terminal. Supported:
#   kitty, ghostty, wezterm, alacritty, rio   (launched directly)
#   iterm2, terminal                          (launched via AppleScript)
#
# Not supported: Warp, Hyper, Tabby — they expose no CLI to run a command
# in a new window. Add a case in write_launcher() if yours isn't listed.
#
# Override the choice with the TERMINAL env var, e.g.:
#   TERMINAL=alacritty ./install-macos-app.sh
#
# Requires: glint on your $PATH. Re-run any time to rebuild the bundle.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ICON="$SCRIPT_DIR/glint.icns"
APP="$HOME/Applications/Glint.app"

GLINT="$(command -v glint || true)"
[ -n "$GLINT" ] || { echo "error: 'glint' not found on \$PATH — install it first." >&2; exit 1; }
[ -f "$ICON" ]  || { echo "error: icon not found at $ICON" >&2; exit 1; }

# Detection order (most terminal-emulator-ish first; Terminal.app always exists).
ORDER=(kitty ghostty wezterm alacritty rio iterm2 terminal)

# For a terminal key, echo the thing we need to launch it:
#   direct terminals  -> absolute path to the binary
#   applescript ones  -> the AppleScript application name
# Empty output means "not installed".
resolve() {
  case "$1" in
    kitty)     _bin /Applications/kitty.app/Contents/MacOS/kitty kitty ;;
    ghostty)   _bin /Applications/Ghostty.app/Contents/MacOS/ghostty ghostty ;;
    wezterm)   _bin /Applications/WezTerm.app/Contents/MacOS/wezterm wezterm ;;
    alacritty) _bin /Applications/Alacritty.app/Contents/MacOS/alacritty alacritty ;;
    rio)       _bin /Applications/rio.app/Contents/MacOS/rio rio ;;
    iterm2)    [ -d /Applications/iTerm.app ] && echo iTerm || true ;;
    terminal)  echo Terminal ;;
  esac
  return 0
}
_bin() {  # $1 = expected app-bundle path, $2 = command name to fall back to
  if [ -x "$1" ]; then echo "$1"
  elif command -v "$2" >/dev/null 2>&1; then command -v "$2"
  fi
  return 0
}

is_applescript() { case "$1" in iterm2|terminal) return 0 ;; *) return 1 ;; esac; }

# Pick the terminal: explicit override, else first one detected.
if [ -n "${TERMINAL:-}" ]; then
  chosen="$TERMINAL"
  case " ${ORDER[*]} " in *" $chosen "*) ;; *)
    echo "error: TERMINAL='$chosen' is not one of: ${ORDER[*]}" >&2; exit 1 ;;
  esac
else
  chosen=""
  for t in "${ORDER[@]}"; do
    if [ -n "$(resolve "$t")" ]; then chosen="$t"; break; fi
  done
fi

BIN="$(resolve "$chosen")"
[ -n "$BIN" ] || { echo "error: terminal '$chosen' not found on this system." >&2; exit 1; }

# Write the bundle's executable, tailored to the chosen terminal. glint's
# absolute path is baked in; cwd is set to $HOME at launch.
write_launcher() {
  local out="$1"
  case "$chosen" in
    kitty)
      cat > "$out" <<EOF
#!/bin/bash
exec "$BIN" \\
  --single-instance --instance-group glint \\
  --directory "\$HOME" \\
  -o macos_quit_when_last_window_closed=yes \\
  "$GLINT"
EOF
      ;;
    ghostty)
      cat > "$out" <<EOF
#!/bin/bash
exec "$BIN" --working-directory="\$HOME" -e "$GLINT"
EOF
      ;;
    wezterm)
      cat > "$out" <<EOF
#!/bin/bash
exec "$BIN" start --cwd "\$HOME" "$GLINT"
EOF
      ;;
    alacritty)
      cat > "$out" <<EOF
#!/bin/bash
exec "$BIN" --working-directory "\$HOME" -e "$GLINT"
EOF
      ;;
    rio)
      cat > "$out" <<EOF
#!/bin/bash
exec "$BIN" --working-dir "\$HOME" -e "$GLINT"
EOF
      ;;
    iterm2)
      # No launch-a-command CLI; drive it with AppleScript. exec replaces the
      # login shell so quitting glint leaves no stray prompt.
      cat > "$out" <<EOF
#!/bin/bash
osascript <<'APPLESCRIPT'
tell application "iTerm"
  activate
  set w to (create window with default profile)
  tell current session of w to write text "cd ~; clear; exec '$GLINT'"
end tell
APPLESCRIPT
EOF
      ;;
    terminal)
      cat > "$out" <<EOF
#!/bin/bash
osascript <<'APPLESCRIPT'
tell application "Terminal"
  activate
  do script "cd ~; clear; exec '$GLINT'"
end tell
APPLESCRIPT
EOF
      ;;
  esac
}

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
write_launcher "$APP/Contents/MacOS/glint-launch"
chmod +x "$APP/Contents/MacOS/glint-launch"

cat > "$APP/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>               <string>Glint</string>
  <key>CFBundleDisplayName</key>        <string>Glint</string>
  <key>CFBundleIdentifier</key>         <string>com.ntrospect0.glint</string>
  <key>CFBundlePackageType</key>        <string>APPL</string>
  <key>CFBundleExecutable</key>         <string>glint-launch</string>
  <key>CFBundleIconFile</key>           <string>glint</string>
  <key>LSMinimumSystemVersion</key>     <string>11.0</string>
  <key>NSHighResolutionCapable</key>    <true/>
</dict>
</plist>
EOF

cp "$ICON" "$APP/Contents/Resources/glint.icns"

# Nudge LaunchServices to pick up the new bundle + icon.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP" 2>/dev/null || true

echo "Built $APP  (terminal: $chosen)"
echo "Launch it from ~/Applications (or drag it to the Dock)."
