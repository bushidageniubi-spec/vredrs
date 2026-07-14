#!/bin/bash
# Vredrs 1.0 test suite runner
# Usage: ./tests/run_tests.sh [path/to/vredrs]

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Ensure clang is on PATH (required for native compilation)
if ! command -v clang &>/dev/null; then
    if [ -f "$PROJECT_DIR/../tools/bin/clang" ]; then
        export PATH="$PROJECT_DIR/../tools/bin:$PATH"
    elif [ -f "/home/z/my-project/tools/bin/clang" ]; then
        export PATH="/home/z/my-project/tools/bin:$PATH"
    fi
fi

VREDRS="${1:-$PROJECT_DIR/target/release/vredrs}"
if [ ! -f "$VREDRS" ]; then
    VREDRS="$PROJECT_DIR/target/debug/vredrs"
fi
if [ ! -f "$VREDRS" ]; then
    echo "Error: vredrs binary not found. Build with 'cargo build' first."
    exit 1
fi

PASS=0
FAIL=0
TOTAL=0

run_test() {
    local name="$1"
    local file="$SCRIPT_DIR/$2"
    local expected="$3"
    TOTAL=$((TOTAL + 1))
    echo -n "  $name... "
    local tmpout=$(mktemp)
    if "$VREDRS" build "$file" -o "$tmpout" 2>/dev/null; then
        chmod +x "$tmpout" 2>/dev/null
        if "$tmpout" 2>/dev/null | head -1 | grep -q "$expected"; then
            echo "PASS"
            PASS=$((PASS + 1))
        else
            echo "FAIL (output mismatch)"
            FAIL=$((FAIL + 1))
        fi
    else
        echo "FAIL (build error)"
        FAIL=$((FAIL + 1))
    fi
    rm -f "$tmpout"
}

echo "=== Vredrs 1.0 Test Suite ==="
echo ""

run_test "Basic types"      test_basic.veds       "Basic"
run_test "Containers"       test_containers.veds  "Container"
run_test "Classes"          test_classes.veds     "Class"
run_test "Exceptions"       test_exceptions.veds  "Exception"
run_test "Generators"       test_generators.veds  "Generator"
run_test "Modules"          test_modules.veds     "Module"
run_test "File I/O"         test_fileio.veds      "File"
run_test "Try/Catch"        test_try_catch.veds   "caught"

echo ""
echo "Results: $PASS/$TOTAL passed, $FAIL failed"
if [ "$FAIL" -eq 0 ]; then
    echo "All tests passed!"
    exit 0
else
    echo "Some tests failed."
    exit 1
fi
