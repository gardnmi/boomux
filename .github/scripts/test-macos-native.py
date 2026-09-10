"""Run each selected native scenario with a process-group deadline and durable logs."""
import json
import os
from pathlib import Path
import signal
import subprocess

SCENARIOS = [
    "replacement_bootstrap_receives_listener_and_lock_ownership",
    "native_daemon_handoffs_multiple_detached_shells",
    "daemon_bounds_stalled_connections_and_recovers_capacity",
    "explicit_executable_handoff_preserves_live_runs_and_rolls_back_on_failure",
    "attachment_client_reconnects_across_daemon_restart",
    "graceful_restart_preserves_exited_run_and_terminal_state",
    "native_daemon_recovers_reproducible_metadata_after_restart",
    "graceful_restart_preserves_opencode_shared_runtime_and_stop_cleans_it_up",
    "graceful_restart_transfers_live_kiro_holder_authority_and_rolls_back_safely",
]


def main():
    output = Path("dist/macos/native")
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for scenario in SCENARIOS:
        path = output / f"{scenario}.log"
        with path.open("w") as log:
            process = subprocess.Popen(["cargo", "+stable", "test", "--locked", "--test",
                                        "native_backend", scenario, "--", "--test-threads=1", "--nocapture"],
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                code = process.wait(timeout=90)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                code = 124
        print(f"::group::{scenario} (exit {code})", flush=True)
        print(path.read_text(), flush=True)
        print("::endgroup::", flush=True)
        results.append(dict(scenario=scenario, code=code))
    (output / "result.json").write_text(json.dumps(results, indent=2) + "\n")
    raise SystemExit(int(any(result["code"] for result in results)))


if __name__ == "__main__":
    main()
