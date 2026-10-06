# Sourced by assets/demo.tape: a scratch project with one bug, outside any
# real path, so nothing personal shows on screen.
export PS1='$ '
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
