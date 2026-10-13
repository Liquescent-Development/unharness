# Sourced by assets/demo.tape: a scratch project with one bug, outside any
# real path, so nothing personal shows on screen.
export PS1='$ '

# The build from this checkout, when there is one.
[ -x "$PWD/target/release/unharness" ] && export PATH="$PWD/target/release:$PATH"

# Configuration and state of their own for the recording, so your MCP
# servers, rules and conversations stay out of it: Claude Code gets no
# servers, and each server in Codex's config is switched off.
export XDG_CONFIG_HOME=/tmp/unharness-demo-config
export XDG_STATE_HOME=/tmp/unharness-demo-state
rm -rf "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"
mkdir -p "$XDG_CONFIG_HOME/unharness" "$XDG_STATE_HOME"
codex_args=$(codex mcp list --json 2>/dev/null | python3 -c '
import json, sys
names = [s["name"] for s in json.load(sys.stdin)]
print(", ".join(f"\"-c\", \"mcp_servers.{n}.enabled=false\"" for n in names))
' 2>/dev/null)
cat > "$XDG_CONFIG_HOME/unharness/config.toml" <<EOT
[harnesses.claude]
extra_args = ["--strict-mcp-config"]

[harnesses.codex]
extra_args = [$codex_args]
EOT

# Mistral Vibe runs in a copy of its home (sign-in and caches included, its
# logs left out), so the recording writes nothing into the real one.
export VIBE_HOME=/tmp/unharness-demo-vibe
rm -rf "$VIBE_HOME"
mkdir -p "$VIBE_HOME"
cp -a "${HOME}/.vibe/." "$VIBE_HOME/" 2>/dev/null
rm -rf "$VIBE_HOME/logs" "$VIBE_HOME/vibehistory"

rm -rf /tmp/unharness-demo
mkdir -p /tmp/unharness-demo
cd /tmp/unharness-demo || return
git init -q -b main
cat > fib.py <<'EOT'
def fib(n):
    """Return the n-th Fibonacci number (fib(0) == 0, fib(1) == 1)."""
    a, b = 0, 1
    for _ in range(n - 1):
        a, b = b, a + b
    return a
EOT
git add fib.py
git -c user.name=demo -c user.email=demo@example.com commit -qm init
clear
