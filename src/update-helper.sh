
# Flodo's updater. Flodo writes the variables above, starts this script in its
# own process group, and quits. This waits for Flodo to exit, installs the
# offered version into the same npm prefix, records the outcome for the next
# launch, starts Flodo again and deletes itself.
trap '' HUP
cd "${TMPDIR:-/tmp}" 2>/dev/null || cd /
LOG="$STATE_DIR/update.log"
RESULT="$STATE_DIR/update-result.json"
OUT="$0.out"
NOTES="$0.notes"
# Test-only: install from a local tarball instead of the registry.
if [ -n "${FLODO_UPDATE_SPEC:-}" ]; then SPEC="$FLODO_UPDATE_SPEC"; fi
npm_config_update_notifier=false
npm_config_fund=false
npm_config_audit=false
npm_config_progress=false
npm_config_color=false
export npm_config_update_notifier npm_config_fund npm_config_audit npm_config_progress npm_config_color

note() { printf '%s\n' "$*" >>"$NOTES"; }
note "Flodo update from $FROM to $TO, started $(date)"

# The message is built only from fixed text and sanitised tokens, so it
# needs no JSON escaping.
write_result() {
  json=$(printf '{"ok":%s,"from":"%s","to":"%s","message":"%s","finished_at":"%s"}' \
    "$1" "$FROM" "$TO" "$2" "$(date -u +%Y-%m-%dT%H:%M:%SZ)")
  printf '%s\n' "$json" >"$RESULT.tmp" && mv -f "$RESULT.tmp" "$RESULT"
  note "Result: $json"
}

finish() {
  note "Finished $(date)"
  cp -f "$NOTES" "$LOG" 2>/dev/null
  rm -f "$NOTES" "$OUT" "$RESULT.tmp" "$0"
  exit 0
}

run_npm() {
  if [ -n "$NODE" ]; then set -- "$NODE" "$NPM_CLI"; else set -- npm; fi
  note "Running: $* install -g $SPEC --prefix $PREFIX"
  "$@" install -g "$SPEC" --prefix "$PREFIX" >"$OUT" 2>&1 &
  npm_pid=$!
  n=0
  while kill -0 "$npm_pid" 2>/dev/null; do
    n=$((n + 1))
    if [ "$n" -ge 600 ]; then
      kill "$npm_pid" 2>/dev/null
      wait "$npm_pid" 2>/dev/null
      return 124
    fi
    sleep 1
  done
  wait "$npm_pid"
}

reason() {
  if [ "$1" -eq 124 ]; then echo 'npm took longer than ten minutes'; return; fi
  code=$(grep -m 1 -oE 'npm (error|ERR!) code [A-Za-z0-9_]+' "$OUT" 2>/dev/null | awk '{print $NF}')
  if [ -n "$code" ]; then echo "npm error $code"; else echo "npm exited with code $1"; fi
}

n=0
while kill -0 "$FLODO_PID" 2>/dev/null; do
  n=$((n + 1))
  if [ "$n" -ge 60 ]; then
    # Still open, so there is nothing to restart.
    write_result false 'Flodo did not close within a minute'
    finish
  fi
  sleep 1
done

run_npm
status=$?
if [ "$status" -ne 0 ]; then
  note "First attempt failed ($(reason "$status")); retrying in 3 seconds."
  sleep 3
  run_npm
  status=$?
fi
note 'npm output (last 20 lines):'
tail -n 20 "$OUT" 2>/dev/null | sed 's/^/  /' >>"$NOTES"

installed=$(sed -n 's/.*"version" *: *"\([^"]*\)".*/\1/p' "$PACKAGE/package.json" 2>/dev/null |
  head -n 1 | tr -cd '0-9A-Za-z.+-')
if [ "$status" -ne 0 ]; then
  write_result false "$(reason "$status")"
elif [ "$installed" != "$TO" ]; then
  write_result false "npm finished but version $installed is installed"
else
  write_result true ''
fi

# Start Flodo again whatever happened, so the app is never left missing.
if [ -n "$APP" ]; then
  if open -a "$APP" >/dev/null 2>&1; then note 'Started Flodo again.'; else note 'Could not start Flodo again.'; fi
else
  cd "$WORK_DIR" 2>/dev/null || cd "$HOME" 2>/dev/null || cd /
  if command -v setsid >/dev/null 2>&1; then
    setsid "$EXE" </dev/null >/dev/null 2>&1 &
  else
    nohup "$EXE" </dev/null >/dev/null 2>&1 &
  fi
  note "Started Flodo again (process $!)."
fi
finish
