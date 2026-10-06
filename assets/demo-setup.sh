# Sourced by assets/demo.tape: a scratch project with one bug, outside any
# real path, so nothing personal shows on screen.
export PS1='$ '

# A config of its own for the recording, so your MCP servers stay out of it:
# Claude Code gets none, and each server in Codex's config is switched off.
export XDG_CONFIG_HOME=/tmp/unharness-demo-config
rm -rf "$XDG_CONFIG_HOME"
mkdir -p "$XDG_CONFIG_HOME/unharness"
codex_args=$(codex mcp list --json 2>/dev/null | python3 -c '
import json, sys
names = [s["name"] for s in json.load(sys.stdin)]
print(", ".join(f"\"-c\", \"mcp_servers.{n}.enabled=false\"" for n in names))
' 2>/dev/null)
cat > "$XDG_CONFIG_HOME/unharness/config.toml" <<EOF
[harnesses.claude]
extra_args = ["--strict-mcp-config"]

[harnesses.codex]
extra_args = [$codex_args]
EOF

rm -rf /tmp/unharness-demo
mkdir -p /tmp/unharness-demo
cd /tmp/unharness-demo || return
git init -q -b main
cat > fib.py <<'EOF'
def fib(n):
    """Return the n-th Fibonacci number (fib(0) == 0, fib(1) == 1)."""
    a, b = 0, 1
    for _ in range(n - 1):
        a, b = b, a + b
    return a
EOF
git add fib.py
git -c user.name=demo -c user.email=demo@example.com commit -qm init
clear
