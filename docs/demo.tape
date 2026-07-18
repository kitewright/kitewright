# Kitewright demo — a reproducible terminal GIF.
#
# Render:  vhs docs/demo.tape        → produces docs/demo.gif
# Install vhs (no brew needed): download the binary from
#   https://github.com/charmbracelet/vhs/releases  (needs ttyd + ffmpeg)
# Re-render on every version bump so the GIF never goes stale.

Output docs/demo.gif

Set Shell "bash"
Set FontSize 20
Set Width 1120
Set Height 640
Set Padding 26
Set Theme "Catppuccin Mocha"
Set TypingSpeed 55ms
Set PlaybackSpeed 1.0

# --- intro ---
Type "# Kitewright — browser automation for AI agents, in one 7 MB binary"
Enter
Sleep 900ms

# --- it's a real, versioned CLI ---
Type "kite --version"
Enter
Sleep 500ms
Type "kite --help | head -6"
Enter
Sleep 1600ms

# --- start the MCP server: fast + tiny ---
Type "# start the Streamable-HTTP MCP server (browser launches lazily, headless here)"
Enter
Sleep 400ms
Type "KITE_HEADLESS=1 kite & sleep 0.4"
Enter
Sleep 1200ms

# --- the money shot: ~8 MB idle, vs 100+ MB for Node/Playwright ---
Type "ps -o rss= -p $! | awk '{printf \"kite idle RSS: %.1f MB\\n\", $1/1024}'"
Enter
Sleep 1400ms

# --- prove it's a live MCP endpoint ---
Type "curl -s -o /dev/null -w 'GET /mcp -> HTTP %{http_code} (MCP handshake required)\\n' http://127.0.0.1:8090/mcp"
Enter
Sleep 1600ms

Type "# 23 tools: navigate, screenshot, extract, fill_form, browser_pdf, ..."
Enter
Sleep 900ms

# --- one-line install (the call to action) ---
Type "kill %1 2>/dev/null ; clear"
Enter
Sleep 300ms
Type "# add it to any MCP client in one line:"
Enter
Sleep 400ms
Type "claude mcp add kitewright -- npx -y @kitewright/mcp"
Enter
Sleep 2200ms
