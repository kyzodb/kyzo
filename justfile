# Process entrypoints, not build config — the container is the only place
# cargo runs (docker-compose.yml, pre-bash-guard.sh). Never type the raw
# multi-line docker/cargo invocations directly into a foreground shell —
# use these recipes so the heavy runs stay backgrounded and logged.

# Runs resonance + fast lib tests in the background; returns immediately.
gate:
    @nohup crates/xtask/gate-fast.sh >/dev/null 2>&1 & disown; \
    echo "gate running in background — watch crates/xtask/resonance.log"

# Current gate verdict, one line.
gate-status:
    @head -1 crates/xtask/resonance.log 2>/dev/null || echo "no resonance.log yet"
