#!/usr/bin/env bash
# Live end-to-end smoke test: real `codex` + agent-mux, no API spend.
#
#   scripts/codex-e2e/smoke.sh [local|remote|all]     (default: all)
#
# Runs real codex in tmux against a localhost Responses-API mock
# (mock_responses.py), runs agent-mux in a second tmux session, and drives
# the scenario through tmux, reading the dashboard back with capture-pane.
# Notifications are captured by an `osascript` PATH shim (agent-mux is
# configured with backend = "osascript"), so nothing reaches the desktop.
#
#   local   codex in a plain tmux session (an externally-started session,
#           matched to its row by cwd)
#   remote  the same against a loopback "SSH host": an `ssh` shim runs
#           commands locally under a fake remote $HOME and its own tmux
#           server, so the real SshHost / remote-poller code paths run
#
# Each scenario checks: approval prompt → `! blocked` row + "needs your
# input" toast (from codex's pane title; no hook installed) → approve →
# `◐ working` → `✓ done` + "finished" toast → an apply_patch turn whose file
# shows in the edited-files picker.
#
# Needs: codex, python3, tmux, git; remote also needs GNU find (`gfind`, or
# findutils in /nix/store). Everything lives in a temp dir (KEEP=1 keeps
# it) plus a short /tmp socket dir (macOS caps unix socket paths at 104
# bytes). AGENT_MUX_BIN overrides the binary (default target/debug, built
# if missing). Exit status is non-zero if any check failed.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WHICH="${1:-all}"
case "$WHICH" in local | remote | all) ;; *) echo "usage: $0 [local|remote|all]" >&2; exit 2 ;; esac

for tool in codex python3 tmux git; do
  command -v "$tool" >/dev/null || { echo "smoke: '$tool' not on PATH" >&2; exit 2; }
done
BIN="${AGENT_MUX_BIN:-$REPO/target/debug/agent-mux}"
if [ ! -x "$BIN" ]; then (cd "$REPO" && cargo build --quiet) || exit 2; fi

# Canonical path: codex keys project trust by the resolved directory
# (macOS's $TMPDIR sits under the /var -> /private/var symlink).
WORK="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/agent-mux-e2e.XXXXXX")" && pwd -P)"
SOCK="$(mktemp -d /tmp/amx.XXXXXX)"
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
MODEF="$WORK/mode"
FAILS=0
MOCK_PID=""

cleanup() {
  for d in "$SOCK"/*; do [ -d "$d" ] && TMUX_TMPDIR="$d" tmux kill-server 2>/dev/null; done
  [ -n "$MOCK_PID" ] && { kill "$MOCK_PID" 2>/dev/null; wait "$MOCK_PID" 2>/dev/null; }
  rm -rf "$SOCK"
  if [ "${KEEP:-0}" = 1 ]; then echo "smoke: kept $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

mode() { echo "mode=$1 delay=${2:-0}" >"$MODEF"; }
mode text
python3 "$HERE/mock_responses.py" "$PORT" "$WORK/mock.log" "$MODEF" &
MOCK_PID=$!

# ---- helpers over the current scenario (set by each scenario) ----
# CODEX_TMUX: argv prefix reaching codex's tmux server
# DASH_TMUX:  argv prefix reaching the dashboard's tmux server
# TOASTS:     the osascript shim's log

pass() { echo "  ok   $1"; }
fail() {
  echo "  FAIL $1"
  FAILS=$((FAILS + 1))
  echo "  --- dashboard"; "${DASH_TMUX[@]}" capture-pane -p -t dash 2>/dev/null | grep -v '^[│ ]*$' | head -15 | sed 's/^/  | /'
  echo "  --- codex"; "${CODEX_TMUX[@]}" capture-pane -p -t cx 2>/dev/null | grep -v '^\s*$' | tail -8 | sed 's/^/  | /'
  echo "  --- toasts"; sed 's/^/  | /' "$TOASTS" 2>/dev/null
}
dash() { "${DASH_TMUX[@]}" capture-pane -p -t dash 2>/dev/null; }
codex_screen() { "${CODEX_TMUX[@]}" capture-pane -p -t cx 2>/dev/null; }
# wait_for <seconds> <command...>: poll until the command succeeds
wait_for() {
  local secs=$1; shift
  local end=$((SECONDS + secs))
  while [ $SECONDS -lt $end ]; do "$@" && return 0; sleep 0.5; done
  return 1
}
row_is() { dash | grep -q -- "$1"; }
toasts_have() { grep -q -- "$1" "$TOASTS" 2>/dev/null; }
codex_ready() {
  local screen
  screen="$(codex_screen)"
  # Belt and braces: answer the directory-trust prompt if the config's
  # trust entry didn't match.
  if grep -q 'Do you trust the contents' <<<"$screen"; then
    "${CODEX_TMUX[@]}" send-keys -t cx Enter
    return 1
  fi
  grep -q 'mock-model default' <<<"$screen"
}
codex_send() {
  "${CODEX_TMUX[@]}" send-keys -t cx -l "$1"
  sleep 0.3
  "${CODEX_TMUX[@]}" send-keys -t cx Enter
}
picker_lists() {
  "${DASH_TMUX[@]}" send-keys -t dash e
  sleep 1
  local hit=1
  dash | grep -q "$1" && hit=0
  "${DASH_TMUX[@]}" send-keys -t dash Escape
  sleep 0.3
  return $hit
}

# write_codex_home <codex_home> <project_dir>
write_codex_home() {
  mkdir -p "$1"
  cat >"$1/config.toml" <<EOF
model = "mock-model"
model_provider = "mock"
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[model_providers.mock]
name = "mock"
base_url = "http://127.0.0.1:$PORT/v1"
wire_api = "responses"

[projects."$2"]
trust_level = "trusted"

[tui.model_availability_nux]
"gpt-5.5" = 4
EOF
}

# write_dash_config <home> [extra toml]
write_dash_config() {
  mkdir -p "$1/.config/agent-mux"
  cat >"$1/.config/agent-mux/config.toml" <<EOF
[agents.codex]
enabled = true

[notifications]
backend = "osascript"

[[tools]]
key = "e"
name = "edit"
command = ["cat", "{file}"]
${2:-}
EOF
}

# write_shims <bin_dir>: the osascript notification catcher
write_osascript_shim() {
  mkdir -p "$1"
  printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "%s"\n' "$TOASTS" >"$1/osascript"
  chmod +x "$1/osascript"
}

# The scenario proper, against whichever servers the caller set up.
# $1: label suffix expected in toasts ("" local, " · fakeremote" remote)
run_scenario() {
  local suffix=$1
  if wait_for 30 codex_ready; then pass "codex is up against the mock"; else fail "codex is up against the mock"; return; fi

  # Interactive codex writes no rollout until the first prompt, so the row
  # can only appear after it.
  mode tool 1
  codex_send "smoke-approval-turn"
  if wait_for 30 row_is 'codex'; then pass "dashboard lists the codex session"; else fail "dashboard lists the codex session"; return; fi
  if wait_for 30 row_is '! blocked'; then pass "approval prompt reads ! blocked (pane title, no hook)"; else fail "approval prompt reads ! blocked"; fi
  if wait_for 10 toasts_have "needs your input"; then pass "blocked toast"; else fail "blocked toast"; fi
  if [ -n "$suffix" ] && ! toasts_have "$suffix"; then fail "toast carries the host label"; fi

  mode text 6 # hold the post-approval turn open, past the 5 s debounce
  "${CODEX_TMUX[@]}" send-keys -t cx y
  if wait_for 10 row_is '◐ working'; then pass "approved: row back to ◐ working"; else fail "approved: row back to ◐ working"; fi
  if wait_for 30 row_is '✓ done'; then pass "turn end: ✓ done"; else fail "turn end: ✓ done"; fi
  if wait_for 10 toasts_have "finished"; then pass "finished toast"; else fail "finished toast"; fi

  mode patch 0
  codex_send "smoke-patch-turn"
  if wait_for 30 picker_lists 'patched_by_mock.txt'; then pass "edited-files picker lists the patched file"; else fail "edited-files picker lists the patched file"; fi
}

scenario_local() {
  echo "== local"
  local d="$WORK/local" proj="$WORK/local/proj"
  mkdir -p "$proj" && git -C "$proj" init -q
  TOASTS="$d/toasts.log"
  write_codex_home "$d/home/.codex" "$proj"
  write_dash_config "$d/home"
  write_osascript_shim "$d/bin"
  mkdir -p "$SOCK/local"
  local envv=(env -u TMUX -u TMUX_PANE HOME="$d/home" CODEX_HOME="$d/home/.codex" TMUX_TMPDIR="$SOCK/local" PATH="$d/bin:$PATH")
  CODEX_TMUX=("${envv[@]}" tmux)
  DASH_TMUX=("${envv[@]}" tmux)
  "${CODEX_TMUX[@]}" -f /dev/null new-session -d -s cx -x 160 -y 45 -c "$proj" codex
  "${DASH_TMUX[@]}" new-session -d -s dash -x 200 -y 50 "${envv[*]} '$BIN' 2>'$d/agent-mux.err'"
  run_scenario ""
}

scenario_remote() {
  echo "== remote (loopback ssh)"
  local d="$WORK/remote"
  local R="$d/remote-home" L="$d/local-home" proj="$d/remote-home/workspace/repo1"
  local gfind
  gfind="$(command -v gfind || true)"
  [ -n "$gfind" ] || { find --version 2>/dev/null | grep -q GNU && gfind="$(command -v find)"; }
  [ -n "$gfind" ] || gfind="$(ls -d /nix/store/*-findutils-4*/bin/find 2>/dev/null | head -1)"
  if [ -z "$gfind" ]; then fail "GNU find for the fake remote (install findutils / gfind)"; return; fi
  mkdir -p "$proj" "$d/bin" "$d/remote-bin" "$SOCK/remote" "$SOCK/dash"
  git -C "$proj" init -q
  ln -sf "$gfind" "$d/remote-bin/find"
  TOASTS="$d/toasts.log"
  write_codex_home "$R/.codex" "$proj"
  write_dash_config "$L" $'\n[hosts.fakeremote]\nssh = "fakeremote"'
  write_osascript_shim "$d/bin"
  # ssh shim: ControlMaster ops are no-ops; commands run locally as the
  # fake remote (its own HOME, tmux server, and GNU find).
  cat >"$d/bin/ssh" <<SHIM
#!/bin/bash
master=""; target=""
while [ \$# -gt 0 ]; do
  case "\$1" in
    -O) exit 0 ;;
    -fN|-M|-N|-f) master=1; shift ;;
    -S|-o|-p|-l|-i|-F|-J|-L|-R|-D|-W|-E|-c|-m|-b) shift 2 ;;
    -*) shift ;;
    *) target="\$1"; shift; break ;;
  esac
done
[ -n "\$master" ] && [ \$# -eq 0 ] && exit 0
[ "\$target" = fakeremote ] || { echo "ssh shim: unknown target '\$target'" >&2; exit 255; }
cd "$R" || exit 255
exec env -u TMUX -u TMUX_PANE HOME="$R" CODEX_HOME="$R/.codex" TMUX_TMPDIR="$SOCK/remote" \\
  PATH="$d/remote-bin:\$PATH" sh -c "\$*"
SHIM
  chmod +x "$d/bin/ssh"
  CODEX_TMUX=("$d/bin/ssh" fakeremote tmux)
  local dashenv=(env -u TMUX -u TMUX_PANE HOME="$L" TMUX_TMPDIR="$SOCK/dash" PATH="$d/bin:$PATH")
  DASH_TMUX=("${dashenv[@]}" tmux)
  "$d/bin/ssh" fakeremote "tmux -f /dev/null new-session -d -s cx -x 160 -y 45 -c '$proj' codex"
  "${DASH_TMUX[@]}" -f /dev/null new-session -d -s dash -x 200 -y 50 "${dashenv[*]} '$BIN' 2>'$d/agent-mux.err'"
  run_scenario " · fakeremote"
}

[ "$WHICH" = remote ] || scenario_local
[ "$WHICH" = local ] || scenario_remote

if [ "$FAILS" -eq 0 ]; then echo "smoke: all checks passed"; else echo "smoke: $FAILS check(s) failed"; fi
[ "$FAILS" -eq 0 ]
